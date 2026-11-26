//! The deterministic command engine.
//!
//! The server owns physics. Every state change goes through
//! [`World::execute`], which parses a line with the one verb parser and
//! resolves it with the dice of the world. Nobody, human or agent, can
//! narrate state into existence.

mod combat;
mod items;
mod look;
mod script;
mod social;

pub use script::{DECLINED, HookStatus, Inadmissible};

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;

use mud_rules::{recovery_rolls, recovery_step, self_control_roll, success_roll};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::event::{ActorRef, Event, EventKind};
use crate::hooks::{WorldDice, WorldHooks};
use crate::ids::{EntityId, ItemId, PoolId, ProtoId, RoomId, Seq, SkillId};
use crate::pack::{Direction, EntityKind, Slot, WorldPack};
use crate::parser::{Command, MAX_TEXT_CHARS, ParseError, forbidden_in_text, parse_with};
use crate::rewards::MintRecord;
use crate::state::{Entity, Holder, NameOwner, WorldState};

/// How many events the world keeps in memory for `mud_events`.
pub const EVENT_LOG_CAP: usize = 10_000;

/// The cue placeholder for the self-control number.
const TARGET_SLOT: &str = concat!("{", "target", "}");

/// The cue placeholder for what set the roll off.
const TRIGGER_SLOT: &str = concat!("{", "trigger", "}");

use crate::pack::SELF_WORDS;

/// How long a trait waits between two self-control rolls, in ms.
pub const NUDGE_COOLDOWN_MS: u64 = 120_000;

/// The tag of the room line an entity's coming to emits.
///
/// The event has no actor and targets the entity, so the router wakes the
/// entity's mind for it as for an attack on it, and never skips it as the
/// event's own actor.
pub const CAME_TO_TAG: &str = "came_to";

/// What one world tick did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickOutcome {
    /// The events it emitted (respawns, coming to).
    pub seqs: Vec<Seq>,
    /// Who came to, each with the seq of its room line. The supervisor
    /// wakes each one's mind.
    pub came_to: Vec<(EntityId, Seq)>,
    /// What changed: HP, and the `unconscious` status of who came to.
    pub changes: Vec<StateChange>,
}

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
    /// Something urgent happened before a typed command committed.
    Interrupted,
    /// A custom verb that does not work in this room.
    NotHere,
    /// A world script refused the command.
    Refused,
    /// A world script failed; nothing happened.
    ScriptFailed,
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
    /// A refusal.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
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
            ParseError::Usage(_) | ParseError::ControlCharacter => ErrorCode::BadArgs,
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
    /// An exit was locked.
    Locked {
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
    /// A script's reward was minted from a pack pool and recorded in the
    /// ledger. The money and the new items also appear as `currency` and
    /// `item` changes (an item from nowhere).
    Minted {
        /// The pool.
        pool: PoolId,
        /// Who received it.
        entity: EntityId,
        /// The currency made.
        currency: i64,
        /// The items made.
        items: Vec<ItemId>,
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
    /// Whether the line was a read-only verb (or did not parse). Not on
    /// the wire: the store uses it to tell an effectful command, whose
    /// transaction carries the actor's staged side ops (ADR 0011).
    #[serde(skip)]
    pub read_only: bool,
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
    /// A world hook or a mind script faulted.
    ScriptFault {
        /// The hook key, such as `verbs.pray` or `minds.guard_01.on_wake`.
        hook: String,
        /// Who set it off, or whose mind it is.
        entity: EntityId,
        /// What went wrong (at most 300 characters).
        message: String,
        /// Its consecutive faults now.
        faults: u8,
        /// Whether its breaker is now open.
        tripped: bool,
        /// When.
        at_ms: u64,
    },
    /// A line a script logged.
    ScriptLog {
        /// The hook key.
        hook: String,
        /// Who set it off.
        entity: EntityId,
        /// The line.
        line: String,
        /// When.
        at_ms: u64,
    },
    /// The effects a world hook applied.
    ScriptEffects {
        /// The hook key.
        hook: String,
        /// Who set it off.
        entity: EntityId,
        /// Each effect, described.
        effects: Vec<String>,
        /// When.
        at_ms: u64,
    },
    /// A script version was loaded.
    ScriptsLoaded {
        /// The new version.
        version: String,
        /// The one before, if any.
        previous: Option<String>,
        /// `start` or `reload`.
        why: String,
        /// When.
        at_ms: u64,
    },
    /// An operator's reload was refused; the old scripts keep running.
    ScriptsReloadRefused {
        /// The version still running.
        version: String,
        /// Every problem found.
        problems: Vec<String>,
        /// When.
        at_ms: u64,
    },
    /// An unconscious entity rolled to come to (pack `[recovery]
    /// come_to_roll`).
    ComeToRoll {
        /// Who rolled.
        entity: EntityId,
        /// The attribute, such as `ht`.
        attr: String,
        /// The roll.
        roll: mud_rules::SuccessRoll,
        /// Its HP when it rolled, after the step's regeneration.
        hp: i32,
        /// Whether it came to.
        came_to: bool,
        /// When.
        at_ms: u64,
    },
    /// A world hook's reward was minted and recorded in the ledger.
    Minted {
        /// The hook key.
        hook: String,
        /// Who received it.
        entity: EntityId,
        /// The pool.
        pool: PoolId,
        /// The currency made.
        currency: i64,
        /// The prototypes of the items made.
        items: Vec<ProtoId>,
        /// When.
        at_ms: u64,
    },
    /// An operator cleared a mind's script memory.
    MindMemoryReset {
        /// Whose.
        entity: EntityId,
        /// When.
        at_ms: u64,
    },
    /// The store kept failing and its backlog passed the in-memory log's
    /// size: the oldest unsaved events were dropped (they were broadcast,
    /// but will not be in the file).
    StoreBacklog {
        /// How many were dropped this time.
        dropped: u64,
        /// When.
        at_ms: u64,
    },
    /// Retention archived, then trimmed, old events from the store.
    EventsArchived {
        /// Rows trimmed from the table.
        trimmed: u64,
        /// Lines written to the archive (fewer after a crash re-run).
        archived: u64,
        /// The newest seq trimmed.
        through: Seq,
        /// When.
        at_ms: u64,
    },
}

/// Why the scripts of a world changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HooksChange {
    /// The server started.
    Start,
    /// An operator reloaded them.
    Reload,
}

impl HooksChange {
    /// `start` or `reload`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Reload => "reload",
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
    /// The name is a word that means the speaker, such as `me`.
    #[error("that word is reserved; choose another name")]
    Reserved,
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
    /// This one does not perceive it, though it is in the room.
    pub except: Option<EntityId>,
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
            except: None,
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
    /// Who came to, with the seq of the room line.
    pub came_to: Vec<(EntityId, Seq)>,
}

type Done = Result<(String, Value), Fail>;

/// The longest suffix of `recent` (oldest first) whose seqs run without a
/// hole up to `last`; empty when the newest event is not `last`.
fn contiguous_tail(recent: Vec<Event>, last: u64) -> VecDeque<Event> {
    let mut want = last;
    let keep = recent
        .iter()
        .rev()
        .take_while(|e| {
            let ok = e.seq.0 == want && want > 0;
            want = want.saturating_sub(1);
            ok
        })
        .count();
    let skip = recent.len() - keep;
    recent.into_iter().skip(skip).collect()
}

/// A running world: the pack, the state, the dice and the recent events.
#[derive(Debug, Clone)]
pub struct World {
    pack: Arc<WorldPack>,
    state: WorldState,
    dice: WorldDice,
    log: VecDeque<Event>,
    fresh: Vec<Event>,
    audit: Vec<Audit>,
    /// Ledger rows not yet saved: the store appends them with the events.
    mints: Vec<MintRecord>,
    hooks: Option<Arc<dyn WorldHooks>>,
}

impl World {
    /// A fresh world from a pack.
    #[must_use]
    pub fn new(pack: Arc<WorldPack>, seed: u64) -> Self {
        let state = WorldState::from_pack(&pack, seed);
        Self::from_state(pack, state, Vec::new())
    }

    /// A world resumed from a stored state and its recent events.
    ///
    /// The log keeps only the run of `recent` that ends at the state's last
    /// seq with no hole: an older event across a hole (a version 0 store
    /// lost a failed save's events) is left out, so the log always holds
    /// `oldest..=last` and `mud_events` reports `truncated` for any
    /// `since_seq` below the hole.
    ///
    /// Every human starts offline: no session is connected yet, and a
    /// state saved before a crash may still say otherwise. A human comes
    /// back when their session logs in. Agent PCs and NPCs are left to
    /// the supervisor and the pack. The state is first reconciled with the
    /// pack (see [`WorldState::reconcile`]).
    #[must_use]
    pub fn from_state(pack: Arc<WorldPack>, mut state: WorldState, recent: Vec<Event>) -> Self {
        state.reconcile(&pack);
        for e in state.entities.values_mut().filter(|e| e.human) {
            e.online = false;
            e.hidden = false;
        }
        let dice = WorldDice::seeded(state.dice_state);
        let log = contiguous_tail(recent, state.next_seq.saturating_sub(1));
        Self {
            pack,
            state,
            dice,
            log,
            fresh: Vec::new(),
            audit: Vec::new(),
            mints: Vec::new(),
            hooks: None,
        }
    }

    /// Replaces the dice with fixed faces, for tests.
    pub fn fix_dice(&mut self, faces: impl IntoIterator<Item = u8>) {
        self.dice = WorldDice::fixed(faces);
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
        if let Some(d) = self.dice.seeded_state() {
            s.dice_state = d;
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

    /// The ledger rows minted since the last [`World::drain_mints`].
    #[must_use]
    pub fn fresh_mints(&self) -> &[MintRecord] {
        &self.mints
    }

    /// Takes the ledger rows minted since the last call, for the store's
    /// `mints` table. The snapshot keeps only their running totals.
    pub fn drain_mints(&mut self) -> Vec<MintRecord> {
        std::mem::take(&mut self.mints)
    }

    /// Whether a human is present in `room`.
    #[must_use]
    pub fn human_present(&self, room: &RoomId) -> bool {
        self.state
            .entities
            .values()
            .any(|e| e.human && e.present() && &e.room == room)
    }

    /// Whether a player character is present in `room`: a human or an
    /// agent-played PC. NPCs react to players, whoever plays them.
    #[must_use]
    pub fn player_present(&self, room: &RoomId) -> bool {
        self.state
            .entities
            .values()
            .any(|e| e.kind == EntityKind::Player && e.present() && &e.room == room)
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

    /// Every event of the in-memory log, oldest first: what a restart
    /// reloaded and what happened since. The supervisor folds it at boot
    /// (ADR 0011, phase 4). Raw events carry their recipients: never hand
    /// one to an entity; [`Event::view_for`] is the entity's view.
    pub fn logged(&self) -> impl Iterator<Item = &Event> + '_ {
        self.log.iter()
    }

    /// The seq of the oldest event the in-memory log still holds.
    #[must_use]
    pub fn oldest_logged(&self) -> Option<Seq> {
        self.log.front().map(|e| e.seq)
    }

    /// The last sequence number used.
    #[must_use]
    pub const fn last_seq(&self) -> Seq {
        Seq(self.state.next_seq.saturating_sub(1))
    }

    /// Parses and runs one line for `actor`.
    pub fn execute(&mut self, actor: &EntityId, line: &str, now_ms: u64) -> CommandResult {
        let result = match parse_with(line, &|v| self.is_custom_verb(v)) {
            Ok(cmd) => self.execute_command(actor, &cmd, now_ms),
            // An unknown word from someone out cold (`wake`, `get up`)
            // answers with what still works and how recovery goes.
            Err(ParseError::UnknownVerb(_))
                if self
                    .state
                    .entities
                    .get(actor)
                    .is_some_and(|e| e.online && e.incapacitated()) =>
            {
                refusal(None, &self.incapacitated_fail(actor))
            }
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
        // `Command` and this method are public, so a custom command built
        // by hand is checked as the parser would check it.
        if let Command::Custom { name, args } = cmd {
            let fail = if !self.is_custom_verb(name) {
                Some(Fail::from(ParseError::UnknownVerb(name.clone())))
            } else if args.chars().count() > MAX_TEXT_CHARS {
                Some(Fail::from(ParseError::TooLong))
            } else if args.chars().any(forbidden_in_text) {
                Some(Fail::from(ParseError::ControlCharacter))
            } else {
                None
            };
            if let Some(fail) = fail {
                return refusal(Some(cmd.display_verb()), &fail);
            }
        }
        let mut ctx = Ctx {
            now: now_ms,
            ..Ctx::default()
        };
        let mut outcome = self.dispatch(&mut ctx, actor, cmd);
        if let Ok((narrative, resolution)) = &mut outcome {
            for line in self.event_hook(&mut ctx, actor, cmd, resolution) {
                if !narrative.is_empty() {
                    narrative.push('\n');
                }
                narrative.push_str(&line);
            }
        }
        match outcome {
            Ok((narrative, resolution)) => CommandResult {
                ok: true,
                verb: Some(cmd.display_verb().to_owned()),
                error_code: None,
                narrative,
                resolution,
                state_changes: ctx.changes,
                events: ctx.events,
                read_only: cmd.is_read_only(),
            },
            Err(fail) => {
                let mut r = refusal(Some(cmd.display_verb()), &fail);
                r.state_changes = ctx.changes;
                r.events = ctx.events;
                r.read_only = cmd.is_read_only();
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
        // Out cold, an entity may still read itself, the help, who is on
        // and where it lies (`look` matches `mud_look`, which is a view).
        if me.incapacitated()
            && !matches!(
                cmd,
                Command::Sheet | Command::Help | Command::Who | Command::Look
            )
        {
            return Err(self.incapacitated_fail(actor));
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
            Command::Help => {
                let help = self.help_text(actor);
                Ok((help.clone(), json!({ "help": help })))
            }
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
            Command::Custom { name, args } => self.custom_verb(ctx, actor, name, args),
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
        if SELF_WORDS.contains(&word) {
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
        if let Some(x) = &spec.except {
            recipients.retain(|r| r != x);
        }
        let actor = spec.actor.as_ref().and_then(|a| self.state.entities.get(a));
        let human_actor = actor.is_some_and(|e| e.human);
        let player_actor = actor.is_some_and(|e| e.kind == EntityKind::Player);
        // A roomless event keeps where its parties stood, for the
        // anti-chatter counters: they may move before a boot folds it.
        let party_rooms: Vec<RoomId> = if spec.room.is_none() {
            spec.actor
                .iter()
                .chain(&recipients)
                .filter_map(|id| self.state.entities.get(id))
                .map(|e| e.room.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        } else {
            Vec::new()
        };
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
            player_actor,
            party_rooms,
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

    /// Who holds the login name `name`; see [`WorldState::name_owner`].
    ///
    /// # Errors
    /// The name is invalid or reserved.
    pub fn name_owner(&self, name: &str) -> Result<NameOwner, LoginError> {
        self.state.name_owner(name)
    }

    /// Logs a human in by name, making the character on first use. The
    /// server calls it only once the login has proved its password.
    ///
    /// # Errors
    /// The name is invalid, reserved, or belongs to another entity.
    pub fn login(&mut self, name: &str, now: u64) -> Result<EntityId, LoginError> {
        let id = match self.state.name_owner(name)? {
            NameOwner::Free(id) => {
                let start = self.pack.world.start_room.clone();
                self.state.new_player(&id, name.trim(), start, &self.pack);
                id
            }
            NameOwner::Human(id) => id,
            NameOwner::OtherPlayer | NameOwner::NonPlayer => return Err(LoginError::Taken),
        };
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

    /// The refusal of a command from an entity that is out cold or dead:
    /// what still works, and how it recovers.
    pub(crate) fn incapacitated_fail(&self, actor: &EntityId) -> Fail {
        let dead = self
            .state
            .entities
            .get(actor)
            .is_some_and(|e| e.dead_until_ms.is_some());
        let state = if dead {
            "You are dead"
        } else {
            "You are unconscious: nothing you try lands until you come to"
        };
        Fail::new(
            ErrorCode::Incapacitated,
            format!(
                "{state}. Still works: sheet, help, who, look. {}",
                self.recovery_rule()
            ),
        )
    }

    /// The recovery rule of this pack, in one or two sentences
    /// ([`crate::pack::RecoveryRules::rule`]).
    #[must_use]
    pub fn recovery_rule(&self) -> String {
        self.pack.recovery.rule()
    }

    /// The death rule of this pack, in one sentence ([`crate::pack::DeathRules::rule`]).
    #[must_use]
    pub fn death_rule(&self) -> String {
        self.pack.death.rule()
    }

    /// Brings an unconscious entity to: clears the flag, records the
    /// change, and tells the room ("Marta stirs and comes to."). The line
    /// has no actor: it targets the entity, tagged [`CAME_TO_TAG`]. Returns
    /// its seq, or `None` when the entity was not unconscious.
    pub(crate) fn come_to(&mut self, ctx: &mut Ctx, id: &EntityId) -> Option<Seq> {
        let e = self.state.entities.get_mut(id).filter(|e| e.unconscious)?;
        e.unconscious = false;
        let (room, name) = (e.room.clone(), e.name.clone());
        ctx.changes.push(StateChange::Status {
            entity: id.clone(),
            status: "unconscious".into(),
            value: false,
        });
        let spec = Emit::new(
            EventKind::System,
            Some(room),
            format!("{name} stirs and comes to."),
        )
        .target(id)
        .tags([CAME_TO_TAG.to_owned()]);
        let seq = self.emit(ctx, spec);
        ctx.came_to.push((id.clone(), seq));
        Some(seq)
    }

    /// One recovery step for every entity in the world (see
    /// [`mud_rules::recovery_step`]): 1 HP back, and coming to above 0 HP
    /// or on a successful come-to roll.
    fn recover(&mut self, ctx: &mut Ctx) {
        let attr = self.pack.recovery.come_to_roll.clone();
        let ids: Vec<EntityId> = self
            .state
            .entities
            .values()
            .filter(|e| e.present())
            .map(|e| e.id.clone())
            .collect();
        for id in ids {
            let Some(e) = self.state.entities.get(&id) else {
                continue;
            };
            let (hp, hp_max, down) = (e.pools.hp, e.pools.hp_max, e.unconscious);
            let target = attr.as_deref().and_then(|a| e.attrs.get(a));
            let roll = match (target, &attr) {
                (Some(t), Some(a)) if recovery_rolls(hp, hp_max, down) => {
                    Some((a.clone(), success_roll(&mut self.dice, t)))
                }
                _ => None,
            };
            let ok = roll.as_ref().is_some_and(|(_, r)| r.outcome.is_success());
            let (next, _, came) = recovery_step(hp, hp_max, down, ok);
            if let Some((attr, roll)) = roll {
                self.audit.push(Audit::ComeToRoll {
                    entity: id.clone(),
                    attr,
                    roll,
                    hp: next,
                    came_to: came,
                    at_ms: ctx.now,
                });
            }
            if next != hp
                && let Some(e) = self.state.entities.get_mut(&id)
            {
                e.pools.hp = next;
                ctx.changes.push(StateChange::Hp {
                    entity: id.clone(),
                    delta: next - hp,
                    hp: next,
                });
            }
            if came {
                self.come_to(ctx, &id);
            }
        }
    }

    /// Locks again each opened exit whose lock has `relock_minutes` and
    /// whose time is up, on both sides, and tells the room. An exit open
    /// with no time on record (a save from before) starts its clock now.
    fn relock(&mut self, ctx: &mut Ctx) {
        let pack = Arc::clone(&self.pack);
        for (rid, def) in &pack.rooms {
            for (dir, lock) in &def.locks {
                let Some(minutes) = lock.relock_minutes else {
                    continue;
                };
                let Some(rs) = self.state.rooms.get_mut(rid) else {
                    continue;
                };
                if !rs.unlocked.contains(dir) {
                    continue;
                }
                let at = *rs.unlocked_at.entry(*dir).or_insert(ctx.now);
                if ctx.now.saturating_sub(at) < u64::from(minutes) * 60_000 {
                    continue;
                }
                self.set_exit_lock(ctx, rid, *dir, true);
                let spec = Emit::new(
                    EventKind::System,
                    Some(rid.clone()),
                    format!("The way {} swings shut, and its lock catches.", dir.name()),
                )
                .tags(["relock".to_owned()]);
                self.emit(ctx, spec);
            }
        }
    }

    /// Advances world time: respawns mobs, and runs a recovery step for
    /// every entity in the world each `[recovery] every_secs` (a minute
    /// by default).
    pub fn tick(&mut self, now: u64) -> TickOutcome {
        let mut ctx = Ctx {
            now,
            ..Ctx::default()
        };
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
                e.came_from = None;
                let (room, name) = (e.room.clone(), e.name.clone());
                let spec = Emit::new(
                    EventKind::Enter,
                    Some(room),
                    format!("A {name} crawls out of the dark."),
                )
                .actor(&id, "respawn");
                self.emit(&mut ctx, spec);
            }
        }
        if now.saturating_sub(self.state.last_regen_ms) >= self.pack.recovery.every_ms() {
            self.state.last_regen_ms = now;
            self.recover(&mut ctx);
        }
        self.relock(&mut ctx);
        TickOutcome {
            seqs: ctx.events,
            came_to: ctx.came_to,
            changes: ctx.changes,
        }
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

/// A refused command result.
#[must_use]
pub fn refusal(verb: Option<&str>, fail: &Fail) -> CommandResult {
    CommandResult {
        ok: false,
        verb: verb.map(str::to_owned),
        error_code: Some(fail.code),
        narrative: fail.message.clone(),
        resolution: Value::Null,
        state_changes: Vec::new(),
        events: Vec::new(),
        read_only: true,
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod downed_tests;

#[cfg(test)]
mod script_tests;

#[cfg(test)]
mod shops_tests;

#[cfg(test)]
mod retreat_tests;
