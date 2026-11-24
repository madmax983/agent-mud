//! The deterministic command engine.
//!
//! The server owns physics. Every state change goes through
//! [`World::execute`], which parses a line with the one verb parser and
//! resolves it with the dice of the world. Nobody, human or agent, can
//! narrate state into existence.

mod combat;
mod items;
mod look;
mod social;

use std::collections::VecDeque;
use std::sync::Arc;

use mud_rules::{DiceRng, FixedDice, SeededDice, self_control_roll};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::event::{ActorRef, Event, EventKind};
use crate::ids::{EntityId, ItemId, ProtoId, RoomId, Seq, SkillId};
use crate::pack::{Direction, EntityKind, Slot, WorldPack};
use crate::parser::{Command, ParseError, parse};
use crate::state::{Entity, Holder, WorldState};

/// How many events the world keeps in memory for `mud_events`.
pub const EVENT_LOG_CAP: usize = 10_000;

/// The cue placeholder for the self-control number.
const TARGET_SLOT: &str = concat!("{", "target", "}");

/// The cue placeholder for what set the roll off.
const TRIGGER_SLOT: &str = concat!("{", "trigger", "}");

/// How long a trait waits between two self-control rolls, in ms.
pub const NUDGE_COOLDOWN_MS: u64 = 120_000;

/// A structured error code. Agents branch on these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    /// Nothing typed.
    Empty,
    /// The verb is unknown.
    UnknownVerb,
    /// The arguments are wrong.
    BadArgs,
    /// The text is too long.
    TooLong,
    /// No such entity here.
    NoSuchTarget,
    /// No such item here.
    NoSuchItem,
    /// No exit that way.
    NoExit,
    /// The exit is locked.
    ExitLocked,
    /// A foe blocks the way: flee instead.
    InCombat,
    /// It cannot be picked up.
    CannotTake,
    /// The container is locked.
    ContainerLocked,
    /// Not enough money.
    InsufficientFunds,
    /// The keeper cannot pay that much.
    KeeperShort,
    /// The keeper does not sell that.
    NotForSale,
    /// The keeper does not buy that.
    NotBuying,
    /// No shopkeeper here.
    NoKeeper,
    /// Merchandise moves only through a sale.
    PolicyTradeRequired,
    /// This entity cannot start a fight.
    PolicyNoFirstStrike,
    /// The watch keeps the peace here.
    SafeZone,
    /// Knocked out or dead.
    Incapacitated,
    /// No board here.
    NoBoard,
    /// It cannot be equipped.
    NotEquippable,
    /// It cannot be used that way.
    CannotUse,
    /// No such skill.
    UnknownSkill,
    /// The actor is not in the world.
    Offline,
    /// The tell target is not online.
    NotOnline,
    /// The name is taken or invalid.
    BadName,
}

/// A refused command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fail {
    /// The code.
    pub code: ErrorCode,
    /// The message to the actor.
    pub message: String,
}

impl Fail {
    pub(crate) fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<ParseError> for Fail {
    fn from(e: ParseError) -> Self {
        let code = match e {
            ParseError::Empty => ErrorCode::Empty,
            ParseError::UnknownVerb(_) => ErrorCode::UnknownVerb,
            ParseError::Usage(_) => ErrorCode::BadArgs,
            ParseError::TooLong => ErrorCode::TooLong,
        };
        Self::new(code, e.to_string())
    }
}

/// One change a command made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StateChange {
    /// An entity moved.
    Moved {
        /// Who.
        entity: EntityId,
        /// From.
        from: RoomId,
        /// To.
        to: RoomId,
    },
    /// An item moved.
    Item {
        /// The item.
        item: ItemId,
        /// Its prototype.
        proto: ProtoId,
        /// Where it was.
        from: Option<Holder>,
        /// Where it is.
        to: Option<Holder>,
    },
    /// Money changed.
    Currency {
        /// Whose.
        entity: EntityId,
        /// By how much.
        delta: i64,
        /// The new balance.
        balance: i64,
    },
    /// HP changed.
    Hp {
        /// Whose.
        entity: EntityId,
        /// By how much.
        delta: i32,
        /// The new value.
        hp: i32,
    },
    /// A status flag changed.
    Status {
        /// Whose.
        entity: EntityId,
        /// The status, such as `unconscious` or `dead`.
        status: String,
        /// Its new value.
        value: bool,
    },
    /// An exit was unlocked.
    Unlocked {
        /// The room.
        room: RoomId,
        /// The exit.
        dir: Direction,
    },
    /// A container was unlocked.
    ContainerUnlocked {
        /// The container.
        item: ItemId,
    },
    /// Equipment changed.
    Equipped {
        /// Whose.
        entity: EntityId,
        /// The slot.
        slot: Slot,
        /// The item now there.
        item: Option<ItemId>,
    },
    /// A notice went up.
    BoardPost {
        /// Where.
        room: RoomId,
        /// The event.
        seq: Seq,
    },
}

/// The result of one command. The wire form of `mud_command`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandResult {
    /// Whether it worked.
    pub ok: bool,
    /// The verb, when the line parsed.
    pub verb: Option<String>,
    /// The error code of a refusal.
    pub error_code: Option<ErrorCode>,
    /// What the actor sees.
    pub narrative: String,
    /// Structured details: views, rolls, prices.
    pub resolution: Value,
    /// What changed.
    pub state_changes: Vec<StateChange>,
    /// The events it emitted.
    pub events: Vec<Seq>,
}

/// An entry for the observer dashboard. Never shown to entities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Audit {
    /// A self-control roll, passed or failed.
    SelfControl {
        /// Who rolled.
        entity: EntityId,
        /// The trait.
        trait_name: String,
        /// The roll.
        roll: mud_rules::SelfControlRoll,
        /// The event that set it off.
        trigger_seq: Seq,
        /// Whether a nudge went out.
        nudged: bool,
        /// When.
        at_ms: u64,
    },
    /// A command ran.
    Command {
        /// Who.
        entity: EntityId,
        /// The line.
        line: String,
        /// Whether it worked.
        ok: bool,
        /// The error code.
        error_code: Option<ErrorCode>,
        /// When.
        at_ms: u64,
    },
}

/// The dice of a world: seeded in play, fixed in tests.
#[derive(Debug, Clone)]
enum Dice {
    Seeded(SeededDice),
    Fixed(FixedDice),
}

impl DiceRng for Dice {
    fn d6(&mut self) -> u8 {
        match self {
            Self::Seeded(d) => d.d6(),
            Self::Fixed(d) => d.d6(),
        }
    }

    fn below(&mut self, n: u32) -> u32 {
        match self {
            Self::Seeded(d) => d.below(n),
            Self::Fixed(d) => d.below(n),
        }
    }
}

/// Why a login failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoginError {
    /// The name is not 2 to 16 letters, digits or underscores.
    #[error("a name is 2 to 16 letters, digits or underscores")]
    Invalid,
    /// Another entity has that name.
    #[error("that name is taken")]
    Taken,
}

/// What one emit needs.
#[derive(Debug, Clone)]
pub(crate) struct Emit {
    pub room: Option<RoomId>,
    pub kind: EventKind,
    pub tags: Vec<String>,
    pub actor: Option<EntityId>,
    pub verb: Option<&'static str>,
    pub target: Option<EntityId>,
    pub text: Option<String>,
    pub narrative: String,
    pub data: Value,
    pub extra: Vec<EntityId>,
    /// Only these perceive it (instead of the room).
    pub only: Option<Vec<EntityId>>,
}

impl Emit {
    pub(crate) fn new(kind: EventKind, room: Option<RoomId>, narrative: impl Into<String>) -> Self {
        Self {
            room,
            kind,
            tags: Vec::new(),
            actor: None,
            verb: None,
            target: None,
            text: None,
            narrative: narrative.into(),
            data: Value::Null,
            extra: Vec::new(),
            only: None,
        }
    }

    pub(crate) fn actor(mut self, id: &EntityId, verb: &'static str) -> Self {
        self.actor = Some(id.clone());
        self.verb = Some(verb);
        self
    }

    pub(crate) fn target(mut self, id: &EntityId) -> Self {
        self.target = Some(id.clone());
        self
    }

    pub(crate) fn text(mut self, t: &str) -> Self {
        self.text = Some(t.to_owned());
        self
    }

    pub(crate) fn data(mut self, d: Value) -> Self {
        self.data = d;
        self
    }

    pub(crate) fn tags(mut self, tags: impl IntoIterator<Item = String>) -> Self {
        self.tags.extend(tags);
        self
    }
}

/// The per-command scratchpad.
#[derive(Debug, Default)]
pub(crate) struct Ctx {
    pub now: u64,
    pub changes: Vec<StateChange>,
    pub events: Vec<Seq>,
}

type Done = Result<(String, Value), Fail>;

/// A running world: the pack, the state, the dice and the recent events.
#[derive(Debug, Clone)]
pub struct World {
    pack: Arc<WorldPack>,
    state: WorldState,
    dice: Dice,
    log: VecDeque<Event>,
    fresh: Vec<Event>,
    audit: Vec<Audit>,
}

impl World {
    /// A fresh world from a pack.
    #[must_use]
    pub fn new(pack: Arc<WorldPack>, seed: u64) -> Self {
        let state = WorldState::from_pack(&pack, seed);
        Self::from_state(pack, state, Vec::new())
    }

    /// A world resumed from a stored state and its recent events.
    #[must_use]
    pub fn from_state(pack: Arc<WorldPack>, state: WorldState, recent: Vec<Event>) -> Self {
        let dice = Dice::Seeded(SeededDice::new(state.dice_state));
        Self {
            pack,
            state,
            dice,
            log: recent.into_iter().collect(),
            fresh: Vec::new(),
            audit: Vec::new(),
        }
    }

    /// Replaces the dice with fixed faces, for tests.
    pub fn fix_dice(&mut self, faces: impl IntoIterator<Item = u8>) {
        self.dice = Dice::Fixed(FixedDice::new(faces));
    }

    /// The pack.
    #[must_use]
    pub const fn pack(&self) -> &Arc<WorldPack> {
        &self.pack
    }

    /// The state.
    #[must_use]
    pub const fn state(&self) -> &WorldState {
        &self.state
    }

    /// The state with the dice position folded in, ready to store.
    #[must_use]
    pub fn snapshot(&self) -> WorldState {
        let mut s = self.state.clone();
        if let Dice::Seeded(d) = &self.dice {
            s.dice_state = d.state();
        }
        s
    }

    /// One entity.
    #[must_use]
    pub fn entity(&self, id: &EntityId) -> Option<&Entity> {
        self.state.entities.get(id)
    }

    /// Takes the events emitted since the last call, for the store and
    /// the broadcast.
    pub fn drain_fresh(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.fresh)
    }

    /// Takes the observer audit entries since the last call.
    pub fn drain_audit(&mut self) -> Vec<Audit> {
        std::mem::take(&mut self.audit)
    }

    /// Whether a human is present in `room`.
    #[must_use]
    pub fn human_present(&self, room: &RoomId) -> bool {
        self.state
            .entities
            .values()
            .any(|e| e.human && e.present() && &e.room == room)
    }

    /// The entities present in `room`.
    pub fn present_in<'a>(&'a self, room: &'a RoomId) -> impl Iterator<Item = &'a Entity> + 'a {
        self.state
            .entities
            .values()
            .filter(move |e| e.present() && &e.room == room)
    }

    /// The events `viewer` perceived after `since`, oldest first, at most
    /// `limit`.
    #[must_use]
    pub fn events_since(&self, viewer: &EntityId, since: Seq, limit: usize) -> Vec<&Event> {
        self.log
            .iter()
            .filter(|e| e.seq > since && e.reaches(viewer))
            .take(limit)
            .collect()
    }

    /// The last sequence number used.
    #[must_use]
    pub const fn last_seq(&self) -> Seq {
        Seq(self.state.next_seq.saturating_sub(1))
    }

    /// Parses and runs one line for `actor`.
    pub fn execute(&mut self, actor: &EntityId, line: &str, now_ms: u64) -> CommandResult {
        let result = match parse(line) {
            Ok(cmd) => self.execute_command(actor, &cmd, now_ms),
            Err(e) => refusal(None, &Fail::from(e)),
        };
        self.audit.push(Audit::Command {
            entity: actor.clone(),
            line: line.chars().take(200).collect(),
            ok: result.ok,
            error_code: result.error_code,
            at_ms: now_ms,
        });
        result
    }

    /// Runs one parsed command for `actor`.
    pub fn execute_command(
        &mut self,
        actor: &EntityId,
        cmd: &Command,
        now_ms: u64,
    ) -> CommandResult {
        let mut ctx = Ctx {
            now: now_ms,
            ..Ctx::default()
        };
        let outcome = self.dispatch(&mut ctx, actor, cmd);
        match outcome {
            Ok((narrative, resolution)) => CommandResult {
                ok: true,
                verb: Some(cmd.verb().to_owned()),
                error_code: None,
                narrative,
                resolution,
                state_changes: ctx.changes,
                events: ctx.events,
            },
            Err(fail) => {
                let mut r = refusal(Some(cmd.verb()), &fail);
                r.state_changes = ctx.changes;
                r.events = ctx.events;
                r
            }
        }
    }

    fn dispatch(&mut self, ctx: &mut Ctx, actor: &EntityId, cmd: &Command) -> Done {
        let me = self
            .state
            .entities
            .get(actor)
            .ok_or_else(|| Fail::new(ErrorCode::Offline, "you are not in the world"))?;
        if !me.online {
            return Err(Fail::new(ErrorCode::Offline, "you are not in the world"));
        }
        let read_only = cmd.is_read_only();
        let incapacitated = me.unconscious || me.dead_until_ms.is_some();
        if incapacitated && !matches!(cmd, Command::Sheet | Command::Help | Command::Who) {
            return Err(Fail::new(
                ErrorCode::Incapacitated,
                "you are in no state to do anything",
            ));
        }
        if !read_only {
            let keep_hidden = matches!(cmd, Command::Move { .. } | Command::Attempt { .. });
            if let Some(e) = self.state.entities.get_mut(actor) {
                if !matches!(cmd, Command::Defend) {
                    e.defending = false;
                }
                if !keep_hidden {
                    e.hidden = false;
                }
            }
        }
        match cmd {
            Command::Look => Ok(self.look(actor)),
            Command::Exits => Ok(self.exits(actor)),
            Command::Examine { target } => self.examine(actor, target),
            Command::Inventory => Ok(self.inventory(actor)),
            Command::Sheet => Ok(self.sheet(actor)),
            Command::Who => Ok(self.who()),
            Command::Help => Ok((
                crate::parser::HELP.to_owned(),
                json!({ "help": crate::parser::HELP }),
            )),
            Command::Move { dir } => self.walk(ctx, actor, *dir, false),
            Command::Flee { dir } => self.flee(ctx, actor, *dir),
            Command::Say { text } => self.say(ctx, actor, text),
            Command::Emote { text } => self.emote(ctx, actor, text),
            Command::Tell { target, text } => self.tell(ctx, actor, target, text),
            Command::BoardRead => self.board_read(ctx, actor),
            Command::BoardPost { text } => self.board_post(ctx, actor, text),
            Command::Take { item, from } => self.take(ctx, actor, item, from.as_deref()),
            Command::Drop { item } => self.drop_item(ctx, actor, item),
            Command::Give { what, to } => self.give(ctx, actor, what, to),
            Command::Buy { item, from } => self.buy(ctx, actor, item, from.as_deref()),
            Command::Sell { item, to } => self.sell(ctx, actor, item, to.as_deref()),
            Command::List { keeper } => self.list(actor, keeper.as_deref()),
            Command::Equip { item } => self.equip(ctx, actor, item),
            Command::Unequip { item } => self.unequip(ctx, actor, item),
            Command::Use { item, on } => self.use_item(ctx, actor, item, on.as_deref()),
            Command::Attack { target } => self.attack(ctx, actor, target),
            Command::Defend => Ok(self.defend(ctx, actor)),
            Command::Attempt { skill, on } => self.attempt(ctx, actor, skill, on.as_deref()),
        }
    }

    // ------------------------------------------------------------ helpers

    pub(crate) fn me(&self, id: &EntityId) -> Result<&Entity, Fail> {
        self.state
            .entities
            .get(id)
            .ok_or_else(|| Fail::new(ErrorCode::Offline, "you are not in the world"))
    }

    /// Finds a visible entity in `room` by a word. `me` and `self` find the
    /// viewer.
    pub(crate) fn find_here(&self, viewer: &EntityId, word: &str) -> Option<EntityId> {
        if matches!(word, "me" | "self" | "myself") {
            return Some(viewer.clone());
        }
        let room = &self.state.entities.get(viewer)?.room;
        self.present_in(room)
            .filter(|e| &e.id != viewer && !e.hidden)
            .find(|e| e.matches(word))
            .map(|e| e.id.clone())
    }

    /// Finds an item in a list by a word, skipping hidden prototypes.
    pub(crate) fn find_item(&self, list: &[ItemId], word: &str) -> Option<ItemId> {
        list.iter().copied().find(|i| {
            self.state.items.get(i).is_some_and(|it| {
                self.pack
                    .items
                    .get(&it.proto)
                    .is_some_and(|d| !d.hidden && (d.matches(word) || it.proto.as_str() == word))
            })
        })
    }

    pub(crate) fn item_name(&self, item: ItemId) -> String {
        self.state
            .items
            .get(&item)
            .and_then(|it| self.pack.items.get(&it.proto))
            .map_or_else(|| "something".to_owned(), |d| d.name.clone())
    }

    pub(crate) fn item_proto(&self, item: ItemId) -> Option<ProtoId> {
        self.state.items.get(&item).map(|i| i.proto.clone())
    }

    /// Whether `room` has light: it is not dark, or someone present holds a
    /// light.
    #[must_use]
    pub fn lit(&self, room: &RoomId) -> bool {
        let dark = self.pack.rooms.get(room).is_some_and(|r| r.dark);
        !dark
            || self.present_in(room).any(|e| {
                e.equipped.values().any(|i| {
                    self.state
                        .items
                        .get(i)
                        .and_then(|it| self.pack.items.get(&it.proto))
                        .is_some_and(|d| d.light)
                })
            })
    }

    /// The effective level of a skill: trained or default, with the
    /// darkness penalty. `None` for a skill the pack does not know.
    #[must_use]
    pub fn skill_level(&self, e: &Entity, skill: &SkillId) -> Option<i32> {
        let def = self.pack.skills.get(skill)?;
        let base = e.skills.get(skill).copied().unwrap_or_else(|| {
            e.attrs.get(&def.attr).unwrap_or(10) + def.difficulty.default_penalty()
        });
        let dark = if self.lit(&e.room) {
            0
        } else {
            self.pack.world.darkness_penalty
        };
        Some(base + dark)
    }

    pub(crate) fn change_currency(&mut self, ctx: &mut Ctx, who: &EntityId, delta: i64) {
        if let Some(e) = self.state.entities.get_mut(who) {
            e.currency += delta;
            ctx.changes.push(StateChange::Currency {
                entity: who.clone(),
                delta,
                balance: e.currency,
            });
        }
    }

    pub(crate) fn relocate_item(&mut self, ctx: &mut Ctx, item: ItemId, to: Holder) {
        let proto = self.item_proto(item).unwrap_or_else(|| ProtoId::from("?"));
        let from = self.state.move_item(item, &to);
        ctx.changes.push(StateChange::Item {
            item,
            proto,
            from,
            to: Some(to),
        });
    }

    pub(crate) fn destroy_item(&mut self, ctx: &mut Ctx, item: ItemId) {
        let proto = self.item_proto(item).unwrap_or_else(|| ProtoId::from("?"));
        let from = self.state.detach(item);
        self.state.items.remove(&item);
        ctx.changes.push(StateChange::Item {
            item,
            proto,
            from,
            to: None,
        });
    }

    /// Emits one event and runs the self-control checks it sets off.
    pub(crate) fn emit(&mut self, ctx: &mut Ctx, spec: Emit) -> Seq {
        let seq = self.push_event(ctx.now, spec);
        ctx.events.push(seq);
        let nudges = self.self_control(ctx.now, seq);
        ctx.events.extend(nudges);
        seq
    }

    /// Emits an event outside a command (ticks, logins).
    pub(crate) fn emit_bare(&mut self, now: u64, spec: Emit) -> Seq {
        let seq = self.push_event(now, spec);
        self.self_control(now, seq);
        seq
    }

    fn push_event(&mut self, now: u64, spec: Emit) -> Seq {
        let seq = Seq(self.state.next_seq);
        self.state.next_seq += 1;
        let aref = |id: &EntityId| -> Option<ActorRef> {
            self.state.entities.get(id).map(Entity::actor_ref)
        };
        let mut recipients: Vec<EntityId> = match (&spec.only, &spec.room) {
            (Some(only), _) => only.clone(),
            (None, Some(room)) => self.present_in(room).map(|e| e.id.clone()).collect(),
            (None, None) => Vec::new(),
        };
        for x in spec.extra {
            if !recipients.contains(&x) {
                recipients.push(x);
            }
        }
        let human_actor = spec
            .actor
            .as_ref()
            .and_then(|a| self.state.entities.get(a))
            .is_some_and(|e| e.human);
        let event = Event {
            seq,
            at_ms: now,
            room: spec.room,
            kind: spec.kind,
            tags: spec.tags,
            actor: spec.actor.as_ref().and_then(aref),
            verb: spec.verb.map(str::to_owned),
            target: spec.target.as_ref().and_then(aref),
            untrusted_text: spec.text,
            narrative: spec.narrative,
            data: spec.data,
            recipients,
            human_actor,
        };
        self.log.push_back(event.clone());
        while self.log.len() > EVENT_LOG_CAP {
            self.log.pop_front();
        }
        self.fresh.push(event);
        seq
    }

    /// Rolls self-control for each recipient whose trait the event sets
    /// off. Each failure sends that entity a private nudge.
    fn self_control(&mut self, now: u64, seq: Seq) -> Vec<Seq> {
        let Some(event) = self.log.back().filter(|e| e.seq == seq).cloned() else {
            return Vec::new();
        };
        if event.kind == EventKind::SelfControlNudge {
            return Vec::new();
        }
        let haystack = format!(
            "{} {}",
            event.untrusted_text.as_deref().unwrap_or(""),
            event.narrative
        )
        .to_ascii_lowercase();
        let kind = event.kind.name();
        let mut rolls = Vec::new();
        for r in &event.recipients {
            if event.is_by(r) {
                continue;
            }
            let Some(e) = self.state.entities.get(r).filter(|e| e.can_act()) else {
                continue;
            };
            for t in &e.traits {
                let (Some(control), Some(def)) = (t.control, self.pack.traits.get(&t.name)) else {
                    continue;
                };
                let by_kind = def
                    .kinds
                    .iter()
                    .any(|k| *k == kind || event.tags.contains(k));
                let by_word = def
                    .keywords
                    .iter()
                    .any(|w| haystack.contains(&w.to_ascii_lowercase()));
                if !(by_kind || by_word) {
                    continue;
                }
                let key = (r.clone(), t.name.clone());
                if self
                    .state
                    .nudge_rolled_ms
                    .get(&key)
                    .is_some_and(|last| now.saturating_sub(*last) < NUDGE_COOLDOWN_MS)
                {
                    continue;
                }
                rolls.push((key, control, def.cue.clone()));
            }
        }
        let mut out = Vec::new();
        for ((who, trait_name), control, cue) in rolls {
            self.state
                .nudge_rolled_ms
                .insert((who.clone(), trait_name.clone()), now);
            let roll = self_control_roll(&mut self.dice, control);
            self.audit.push(Audit::SelfControl {
                entity: who.clone(),
                trait_name: trait_name.clone(),
                roll,
                trigger_seq: seq,
                nudged: !roll.resisted,
                at_ms: now,
            });
            if roll.resisted {
                continue;
            }
            let trigger = describe_trigger(&event);
            let cue = cue
                .unwrap_or_else(|| {
                    format!("You failed your {TARGET_SLOT}- roll at {TRIGGER_SLOT}.")
                })
                .replace(TARGET_SLOT, &control.to_string())
                .replace(TRIGGER_SLOT, &trigger);
            let spec = Emit {
                only: Some(vec![who.clone()]),
                data: json!({
                    "trait": trait_name,
                    "target_number": control,
                    "roll": { "dice": roll.dice, "total": roll.total, "margin": roll.margin },
                    "trigger_seq": seq,
                    "cue": cue,
                }),
                ..Emit::new(EventKind::SelfControlNudge, event.room.clone(), "")
            };
            out.push(self.push_event(now, spec));
        }
        out
    }

    // ------------------------------------------------------- sessions

    /// Logs a human in by name, making the character on first use.
    ///
    /// # Errors
    /// The name is invalid or belongs to another entity.
    pub fn login(&mut self, name: &str, now: u64) -> Result<EntityId, LoginError> {
        let name = name.trim();
        let valid = (2..=16).contains(&name.chars().count())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            return Err(LoginError::Invalid);
        }
        let id = EntityId::new(format!("pc_{}", name.to_ascii_lowercase()));
        let taken =
            self.state.entities.values().any(|e| {
                e.id != id && (e.matches(name) || e.id.as_str() == name.to_ascii_lowercase())
            });
        if taken {
            return Err(LoginError::Taken);
        }
        if let Some(e) = self.state.entities.get(&id) {
            if !e.human {
                return Err(LoginError::Taken);
            }
        } else {
            let start = self.pack.world.start_room.clone();
            self.state.new_player(&id, name, start, &self.pack);
        }
        self.bring_online(&id, now);
        Ok(id)
    }

    /// Brings an entity into the world (a login).
    pub fn bring_online(&mut self, id: &EntityId, now: u64) -> Option<Seq> {
        let e = self.state.entities.get_mut(id)?;
        if e.online {
            return None;
        }
        e.online = true;
        let (room, name) = (e.room.clone(), e.name.clone());
        let spec = Emit::new(EventKind::Enter, Some(room), format!("{name} arrives."))
            .actor(id, "login")
            .tags(["login".to_owned()]);
        Some(self.emit_bare(now, spec))
    }

    /// Takes an entity out of the world (a logout).
    pub fn take_offline(&mut self, id: &EntityId, now: u64) -> Option<Seq> {
        let e = self.state.entities.get(id)?;
        if !e.online {
            return None;
        }
        let (room, name) = (e.room.clone(), e.name.clone());
        let spec = Emit::new(
            EventKind::Leave,
            Some(room),
            format!("{name} fades from view."),
        )
        .actor(id, "logout")
        .tags(["logout".to_owned()]);
        let seq = self.emit_bare(now, spec);
        if let Some(e) = self.state.entities.get_mut(id) {
            e.online = false;
            e.hidden = false;
        }
        Some(seq)
    }

    /// Advances world time: respawns mobs and regenerates HP once a minute.
    pub fn tick(&mut self, now: u64) -> Vec<Seq> {
        let mut out = Vec::new();
        let due: Vec<EntityId> = self
            .state
            .entities
            .values()
            .filter(|e| e.dead_until_ms.is_some_and(|t| t <= now))
            .map(|e| e.id.clone())
            .collect();
        for id in due {
            if let Some(e) = self.state.entities.get_mut(&id) {
                e.dead_until_ms = None;
                e.unconscious = false;
                e.pools = mud_rules::Pools::full(&e.attrs);
                e.room = e.home.clone();
                let (room, name) = (e.room.clone(), e.name.clone());
                let spec = Emit::new(
                    EventKind::Enter,
                    Some(room),
                    format!("A {name} crawls out of the dark."),
                )
                .actor(&id, "respawn");
                out.push(self.emit_bare(now, spec));
            }
        }
        if now.saturating_sub(self.state.last_regen_ms) >= 60_000 {
            self.state.last_regen_ms = now;
            for e in self.state.entities.values_mut() {
                if e.present() && e.pools.hp < e.pools.hp_max {
                    e.pools.hp += 1;
                    if e.unconscious && e.pools.hp > 0 {
                        e.unconscious = false;
                    }
                }
            }
        }
        out
    }

    /// The entity ids of mobs and players, by kind, for tests and tools.
    #[must_use]
    pub fn ids_of_kind(&self, kind: EntityKind) -> Vec<EntityId> {
        self.state
            .entities
            .values()
            .filter(|e| e.kind == kind)
            .map(|e| e.id.clone())
            .collect()
    }
}

fn describe_trigger(event: &Event) -> String {
    let who = event.actor.as_ref().map_or("someone", |a| a.name.as_str());
    match event.kind {
        EventKind::Speech | EventKind::Tell | EventKind::Emote => format!("{who}'s words"),
        EventKind::BoardPost => format!("{who}'s notice"),
        _ => {
            let n = event.narrative.trim_end_matches('.');
            if n.is_empty() {
                "what just happened".to_owned()
            } else {
                n.to_owned()
            }
        }
    }
}

fn refusal(verb: Option<&str>, fail: &Fail) -> CommandResult {
    CommandResult {
        ok: false,
        verb: verb.map(str::to_owned),
        error_code: Some(fail.code),
        narrative: fail.message.clone(),
        resolution: Value::Null,
        state_changes: Vec::new(),
        events: Vec::new(),
    }
}

#[cfg(test)]
mod tests;
