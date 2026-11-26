//! The attention router: which agent wakes, when, and why.
//!
//! Agents spend no tokens in empty rooms. The router reads every world
//! event and decides, for each agent that perceived it, whether to wake
//! it and how soon:
//!
//! | Priority  | Triggers | Debounce |
//! |-----------|----------|----------|
//! | Immediate | attack on it, a tell, a nudge, speech naming it | 0–500 ms |
//! | `RoomBatch` | human speech, emotes, arrivals, drops | 2–4 s rolling |
//! | Ambient   | agent activity while a human is present | 5–10 s + cooldown |
//! | `MacroTick` | the slow timer, unless another player acted in the room within `macro_defer_ms` | none |
//!
//! It also runs the per-room anti-chatter counter, the budget breakers,
//! the Turing attention filter and distracted spells.
//!
//! A wake waits while its entity cannot act (it is unconscious or dead):
//! [`Router::due_where`] holds it ([`crate::admit::wake_admit`]), and what
//! comes meanwhile merges into it. When the entity comes to, one
//! Immediate "came to" wake ([`Router::came_to`]) delivers everything.
//!
//! A wake that waited while its entity was busy no longer carries the
//! events the busy wake's mind read ([`Router::finish_seen`], ADR 0014).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use mud_world::EntityId;
use mud_world::pack::{EntityKind, Tier};
use mud_world::{CAME_TO_TAG, Event, EventKind, RoomId, Seq, World};
use serde::{Deserialize, Serialize};

use crate::admit::{Admit, CAME_TO, wake_admit};
use crate::budget::BudgetLevel;
use crate::chatter::{ChatterInput, MAX_AGENT_ONLY_TURNS, RoomChatter, chatter_input_for};
use crate::replay::replay_due;
use crate::ticks::tick_deferred;
use crate::turing::{Salience, has_keyword, mix, notices, pick};

/// How urgent a wake is. Higher wins when wakes merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// The slow world timer.
    MacroTick,
    /// A wake the entity booked for itself (`mud_wake_me`).
    Booked,
    /// Agent activity near a human.
    Ambient,
    /// Human activity in the room.
    RoomBatch,
    /// Addressed, attacked, or nudged.
    Immediate,
}

/// The timing knobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouterConfig {
    /// The delay of an immediate wake, in ms.
    pub immediate_ms: [u64; 2],
    /// A room batch waits at least the first and at most the second, in ms.
    pub room_batch_ms: [u64; 2],
    /// The delay of an ambient wake, in ms.
    pub ambient_ms: [u64; 2],
    /// The cooldown after a wake, when the cast names none, in ms.
    pub cooldown_ms: [u64; 2],
    /// The anti-chatter bound.
    pub max_agent_only_turns: u8,
    /// The seed of every jitter.
    pub seed: u64,
    /// A player other than the entity who acted in its room within this
    /// many game-clock ms defers its macro tick ([`tick_deferred`]). An
    /// idle player does not: after the window the tick fires as if the
    /// entity were alone.
    #[serde(default = "default_macro_defer_ms")]
    pub macro_defer_ms: u64,
}

/// The default of [`RouterConfig::macro_defer_ms`]: 10 minutes.
#[must_use]
pub const fn default_macro_defer_ms() -> u64 {
    600_000
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            immediate_ms: [0, 500],
            room_batch_ms: [2_000, 4_000],
            ambient_ms: [5_000, 10_000],
            cooldown_ms: [3_000, 8_000],
            max_agent_only_turns: MAX_AGENT_ONLY_TURNS,
            seed: 0x006d_7564,
            macro_defer_ms: default_macro_defer_ms(),
        }
    }
}

/// What the supervisor knows about an entity beyond the world.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EntityRuntime {
    /// Its budget level now.
    pub budget: Option<BudgetLevel>,
    /// A distracted spell that holds non-urgent wakes until it ends.
    pub afk_until_ms: Option<u64>,
}

/// One wake to deliver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wake {
    /// Who.
    pub entity: EntityId,
    /// How urgent.
    pub priority: Priority,
    /// The events that caused it, in order.
    pub seqs: Vec<Seq>,
    /// Why, for the observer.
    pub reason: String,
    /// Whether a macro-tick is part of it, whatever its priority (a tick
    /// can merge into a more urgent wake).
    #[serde(default)]
    pub macro_tick: bool,
    /// The wakes the entity booked that this wake delivers, with the notes
    /// it left itself.
    #[serde(default)]
    pub booked: Vec<crate::wakes::Booking>,
    /// A plan this wake resumes after a restart. Such a wake carries
    /// nothing else: no events, no macro tick, no booking.
    #[serde(default)]
    pub resume: Option<Resume>,
}

/// A deterministic plan to finish: its lines, from its durable cursor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resume {
    /// The wake that made the plan: the key of its cursor.
    pub wake_id: String,
    /// Every line of the plan.
    pub lines: Vec<String>,
    /// The first line to run.
    pub cursor: u32,
}

/// A routing decision, for the observer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum RouteNote {
    /// A wake was queued or merged.
    Queued {
        /// Who.
        entity: EntityId,
        /// How urgent.
        priority: Priority,
        /// The event.
        seq: Seq,
        /// When it is due.
        due_ms: u64,
    },
    /// A wake was dropped.
    Dropped {
        /// Who.
        entity: EntityId,
        /// The event.
        seq: Seq,
        /// Why.
        why: String,
    },
}

#[derive(Debug, Clone)]
struct Pending {
    priority: Priority,
    seqs: Vec<Seq>,
    first_ms: u64,
    due_ms: u64,
    reason: String,
    macro_tick: bool,
    /// The longest cooldown its non-urgent events asked for, in ms.
    cooldown_ms: u64,
    /// The latest distracted spell its non-urgent events saw.
    afk_until_ms: Option<u64>,
    /// Booked wakes it delivers.
    booked: Vec<crate::wakes::Booking>,
    /// Each event it carries with the priority and reason that event alone
    /// gave it, so a wake whose events the mind already saw falls back to
    /// what is left ([`Router::finish_seen`]).
    causes: Vec<(Seq, Priority, String)>,
    /// The most urgent cause outside the event stream (a session start, an
    /// interrupted line, a come-to, a booking, a macro tick, a wake given
    /// back), with its reason. Such a wake is never dropped as seen.
    forced: Option<(Priority, String)>,
}

impl Pending {
    /// A wake waiting from `now`, due at `due_ms`, carrying nothing yet.
    const fn new(priority: Priority, reason: String, now: u64, due_ms: u64) -> Self {
        Self {
            priority,
            seqs: Vec::new(),
            first_ms: now,
            due_ms,
            reason,
            macro_tick: false,
            cooldown_ms: 0,
            afk_until_ms: None,
            booked: Vec::new(),
            causes: Vec::new(),
            forced: None,
        }
    }

    /// Records a cause outside the event stream.
    fn force_cause(&mut self, priority: Priority, reason: &str) {
        if self.forced.as_ref().is_none_or(|(p, _)| priority >= *p) {
            self.forced = Some((priority, reason.to_owned()));
        }
    }
}

/// The router.
#[derive(Debug, Default)]
pub struct Router {
    cfg: RouterConfig,
    rooms: HashMap<RoomId, RoomChatter>,
    pending: BTreeMap<EntityId, Pending>,
    last_wake_end: HashMap<EntityId, u64>,
    busy: BTreeSet<EntityId>,
    /// Plans to resume, each delivered alone before the entity's next wake.
    resumes: BTreeMap<EntityId, Resume>,
    /// The chatter count of a room the router has not seen yet: 0, or the
    /// bound after a boot whose log does not reach the world's first event
    /// (see [`Router::replay`]).
    unseen_turns: u8,
    /// The newest come-to line each entity was woken for, so the tick's
    /// report and the line itself wake it once.
    came_to_seen: HashMap<EntityId, Seq>,
}

/// Where the chatter counter reads whether a player saw an agent's event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presence {
    /// The world as it is now: a live event.
    Live,
    /// The event's recipients: a reloaded event, whose players may be gone.
    Recipients,
}

/// Which recipients of a reloaded event [`Router::replay`] considers.
type ReplayFilter<'a> = dyn Fn(&EntityId, &Event) -> bool + 'a;

/// Why a wake was queued, for the observer.
fn reason(ev: &Event, replayed: bool) -> String {
    format!(
        "{}{} by {}",
        if replayed { "replayed " } else { "" },
        ev.kind.name(),
        ev.actor.as_ref().map_or("world", |a| a.name.as_str())
    )
}

/// Whether `ev` is the room line of someone coming to.
fn is_came_to(ev: &Event) -> bool {
    ev.tags.iter().any(|t| t == CAME_TO_TAG)
}

/// Whether `text` names the entity, as a whole word.
#[must_use]
pub fn names(text: &str, name: &str, aliases: &[String]) -> bool {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    let hit = |n: &str| {
        let parts: Vec<String> = n.split_whitespace().map(str::to_lowercase).collect();
        !parts.is_empty() && words.windows(parts.len()).any(|w| w == parts.as_slice())
    };
    hit(name) || aliases.iter().any(|a| hit(a))
}

impl Router {
    /// A router.
    #[must_use]
    pub fn new(cfg: RouterConfig) -> Self {
        Self {
            cfg,
            ..Self::default()
        }
    }

    /// The config.
    #[must_use]
    pub const fn config(&self) -> &RouterConfig {
        &self.cfg
    }

    /// The anti-chatter count of a room.
    #[must_use]
    pub fn agent_only_turns(&self, room: &RoomId) -> u8 {
        self.rooms
            .get(room)
            .map_or(self.unseen_turns, |r| r.agent_only_turns)
    }

    /// The entities with a wake waiting.
    #[must_use]
    pub fn pending(&self) -> Vec<(EntityId, Priority, u64)> {
        self.pending
            .iter()
            .map(|(e, p)| (e.clone(), p.priority, p.due_ms))
            .collect()
    }

    /// Whether `entity` has a wake in flight: from the moment it is taken
    /// as due until [`Router::finish`], after its close.
    #[must_use]
    pub fn is_busy(&self, entity: &EntityId) -> bool {
        self.busy.contains(entity)
    }

    /// Whether `entity` has no wake in flight and none waiting.
    #[must_use]
    pub fn is_idle(&self, entity: &EntityId) -> bool {
        !self.busy.contains(entity)
            && !self.pending.contains_key(entity)
            && !self.resumes.contains_key(entity)
    }

    /// Marks an idle `entity` busy for a short owner task of the Steward
    /// (a spend settle), so no wake starts meanwhile. Returns whether it
    /// was idle and is now claimed; a claim never starts a cooldown.
    pub fn claim(&mut self, entity: &EntityId) -> bool {
        if self.is_idle(entity) {
            self.busy.insert(entity.clone());
            true
        } else {
            false
        }
    }

    /// Ends a [`Router::claim`]: the entity is free, and no cooldown
    /// starts, since no wake ran.
    pub fn release(&mut self, entity: &EntityId) {
        self.busy.remove(entity);
    }

    /// Queues a plan resume for `entity`. It is delivered as a wake of its
    /// own as soon as the entity is free and online, ahead of any wake
    /// waiting ([`Router::resumes_due`]).
    pub fn resume(&mut self, entity: &EntityId, plan: Resume) {
        self.resumes.insert(entity.clone(), plan);
    }

    fn jitter(&self, range: [u64; 2], entity: &EntityId, seq: Seq, salt: &[u8]) -> u64 {
        pick(
            mix(
                self.cfg.seed,
                &[entity.as_str().as_bytes(), &seq.0.to_le_bytes(), salt],
            ),
            range[0],
            range[1],
        )
    }

    fn priority_for(ev: &Event, who: &mud_world::state::Entity) -> Option<Priority> {
        let r = &who.id;
        let text = ev.untrusted_text.as_deref().unwrap_or("");
        if is_came_to(ev) {
            // Its own come-to is handled by `Router::came_to`; bystanders
            // notice someone stir.
            return Some(if ev.targets(r) {
                Priority::Immediate
            } else {
                Priority::Ambient
            });
        }
        let immediate = match ev.kind {
            EventKind::SelfControlNudge | EventKind::Tell => true,
            EventKind::Combat | EventKind::GuardIntervene => {
                ev.targets(r) || (who.guard && ev.kind == EventKind::GuardIntervene)
            }
            EventKind::Speech | EventKind::Emote => names(text, &who.name, &who.aliases),
            EventKind::Give | EventKind::Trade | EventKind::Attempt => ev.targets(r),
            _ => false,
        };
        if immediate {
            return Some(Priority::Immediate);
        }
        if ev.player_actor {
            return Some(Priority::RoomBatch);
        }
        // Someone used an item on it: a draught that brought it round, a
        // bandage on its wounds.
        if ev.kind == EventKind::Gear && ev.targets(r) {
            return Some(Priority::RoomBatch);
        }
        match ev.kind {
            EventKind::System | EventKind::BoardRead | EventKind::Gear => None,
            _ => Some(Priority::Ambient),
        }
    }

    /// The chatter counter of `room`. A room the router has not seen
    /// starts at [`Router::replay`]'s guess for a history it cannot read:
    /// 0 in a fresh world, held (the bound) otherwise.
    fn room(&mut self, room: &RoomId) -> &mut RoomChatter {
        let unseen = self.unseen_turns;
        self.rooms.entry(room.clone()).or_insert(RoomChatter {
            agent_only_turns: unseen,
        })
    }

    /// Counts an agent event whose rooms are unknown (a reloaded tell from
    /// a log that predates `Event::party_rooms`). An agent-alone one may
    /// have counted in any room, so every room, seen or not, holds at the
    /// bound until a human or a macro-tick resets it, and the event wakes
    /// no other agent: the safe direction (`lemma_fold_holds_at_least`).
    fn hold_everywhere(&mut self, input: ChatterInput) -> bool {
        if input != ChatterInput::AgentAlone {
            return true;
        }
        let max = self.cfg.max_agent_only_turns;
        for room in self.rooms.values_mut() {
            room.agent_only_turns = room.agent_only_turns.max(max);
        }
        self.unseen_turns = self.unseen_turns.max(max);
        false
    }

    /// Steps the anti-chatter counters for `ev`. Returns whether its actor
    /// is an agent-played NPC and whether it may wake other agents.
    ///
    /// Whether a player saw an agent's event is read from the world as it
    /// is now (`Presence::Live`), or, for a reloaded event, from its
    /// recipients (`Presence::Recipients`): at boot no human is online, so
    /// the world would show no one. A player who was there but is not a
    /// recipient makes the event count as an agent alone: more holding, the
    /// safe direction (`lemma_fold_holds_at_least`).
    fn chatter(&mut self, ev: &Event, world: &World, presence: Presence) -> (bool, bool) {
        let actor = ev.actor.as_ref().and_then(|a| world.entity(&a.id));
        // The engine's line about a body (a collapse, a death, a respawn)
        // is the world's, not its actor's turn (`chatter_input_for`).
        let body_line = ev.is_body_line();
        // An agent here is an agent-played NPC. A player character counts
        // as a player whoever plays it, so an agent PC resets the room and
        // is answered exactly as a human would be.
        let actor_is_agent = !body_line
            && !ev.player_actor
            && actor.is_some_and(|a| {
                !a.human && matches!(a.tier, Tier::Ambient | Tier::Persistent | Tier::Turing)
            });
        let player_saw = || {
            ev.recipients
                .iter()
                .filter(|r| !ev.is_by(r))
                .filter_map(|r| world.entity(r))
                .any(|e| e.kind == EntityKind::Player)
        };
        let max = self.cfg.max_agent_only_turns;
        let allow_cross = if let Some(room) = &ev.room
            && ev.kind != EventKind::SelfControlNudge
        {
            let here = actor_is_agent
                && match presence {
                    Presence::Live => world.player_present(room),
                    Presence::Recipients => player_saw(),
                };
            let input = chatter_input_for(body_line, ev.player_actor, actor_is_agent, here);
            self.room(room).step(input, max)
        } else if ev.room.is_none()
            && actor_is_agent
            && let Some(a) = actor
        {
            // A roomless agent event (a tell) still counts, in the rooms
            // its parties stood in when it happened: agent-only unless a
            // human is with one of them. The engine records those rooms on
            // the event, so a boot counts it where the live counter did,
            // however far the parties have walked since.
            let rooms: Option<BTreeSet<RoomId>> = if !ev.party_rooms.is_empty() {
                Some(ev.party_rooms.iter().cloned().collect())
            } else if presence == Presence::Live {
                Some(
                    std::iter::once(&a.room)
                        .chain(
                            ev.recipients
                                .iter()
                                .filter(|r| !ev.is_by(r))
                                .filter_map(|r| world.entity(r))
                                .map(|t| &t.room),
                        )
                        .cloned()
                        .collect(),
                )
            } else {
                // A log written before the engine recorded the rooms: the
                // positions now say nothing about where the parties were.
                None
            };
            let here = match (presence, &rooms) {
                (Presence::Live, Some(rooms)) => {
                    rooms.iter().any(|room| world.player_present(room))
                }
                _ => player_saw(),
            };
            let input = if here {
                ChatterInput::AgentWithHuman
            } else {
                ChatterInput::AgentAlone
            };
            match rooms {
                Some(rooms) => rooms.iter().fold(true, |allow, room| {
                    let ok = self.room(room).step(input, max);
                    allow && ok
                }),
                None => self.hold_everywhere(input),
            }
        } else {
            true
        };
        (actor_is_agent, allow_cross)
    }

    /// Reads one event. Returns what was decided for each recipient.
    pub fn observe(
        &mut self,
        ev: &Event,
        world: &World,
        rt: &HashMap<EntityId, EntityRuntime>,
        now: u64,
    ) -> Vec<RouteNote> {
        let (actor_is_agent, allow_cross) = self.chatter(ev, world, Presence::Live);
        self.route(ev, world, rt, now, actor_is_agent, allow_cross, None)
    }

    /// Derives the router at boot from the reloaded event log (ADR 0011,
    /// phase 4).
    ///
    /// - **Chatter.** Every logged event is folded, oldest first, through
    ///   the counters ([`crate::chatter::chatter_step`]), reading presence
    ///   from its recipients. A macro-tick reset is not an event, so the
    ///   fold can only hold more than the live counters did. A roomless
    ///   event counts in the rooms recorded on it (`Event::party_rooms`),
    ///   where the live counter counted it; one from an older log, which
    ///   lacks them, holds every room. When the log does not reach back to
    ///   the world's first event, a room's count before its first logged
    ///   event is unknown and starts held (at the bound), and so does a
    ///   room the log never shows: only a human or a macro-tick opens it
    ///   again.
    /// - **Replay.** For each logged event that [`crate::replay::replay_due`]
    ///   accepts for a recipient (after `floor(recipient)`, at most
    ///   `max_age_ms` old, or of any age for a recipient that is out cold
    ///   at boot, or that has a come-to line after its floor and at or
    ///   after the event: its wake was held), the router queues the
    ///   recipient's wake exactly
    ///   as [`Router::observe`] would, but only an Immediate one (a tell,
    ///   an attack on it, speech naming it); the anti-chatter rule drops
    ///   what it would have dropped. No room batch and no ambient wake is
    ///   replayed.
    ///
    /// Returns the routing decisions of the replay.
    pub fn replay(
        &mut self,
        world: &World,
        rt: &HashMap<EntityId, EntityRuntime>,
        now: u64,
        floor: impl Fn(&EntityId) -> u64,
        max_age_ms: u64,
    ) -> Vec<RouteNote> {
        let whole = world
            .oldest_logged()
            .map_or_else(|| world.last_seq().0 == 0, |s| s.0 <= 1);
        self.unseen_turns = if whole {
            0
        } else {
            self.cfg.max_agent_only_turns
        };
        // An entity out cold at boot was held, not idle: everything after
        // its floor is still owed to it, however old, and goes out with
        // the wake it gets when it comes to. So is everything after its
        // floor up to an uncommitted come-to line of its own: that wake
        // carried it, and the crash lost the wake, not the obligation.
        let mut held_until: HashMap<EntityId, u64> = HashMap::new();
        for ev in world.logged().filter(|ev| is_came_to(ev)) {
            for r in &ev.recipients {
                if ev.targets(r) && ev.seq.0 > floor(r) {
                    held_until.insert(r.clone(), ev.seq.0);
                }
            }
        }
        let due = |r: &EntityId, ev: &Event| {
            let held = world
                .entity(r)
                .is_some_and(mud_world::state::Entity::incapacitated)
                || held_until.get(r).is_some_and(|&up| ev.seq.0 <= up);
            let age = if held { u64::MAX } else { max_age_ms };
            replay_due(ev.seq.0, ev.at_ms, floor(r), now, age)
        };
        let mut notes = Vec::new();
        for ev in world.logged() {
            let (actor_is_agent, allow_cross) = self.chatter(ev, world, Presence::Recipients);
            notes.extend(self.route(ev, world, rt, now, actor_is_agent, allow_cross, Some(&due)));
        }
        notes
    }

    /// Decides, for each recipient of `ev`, whether and when it wakes.
    /// With `replay`, only the recipients it accepts are considered, and
    /// only for an Immediate wake.
    #[expect(
        clippy::too_many_lines,
        clippy::too_many_arguments,
        reason = "one pass over the decision table reads best"
    )]
    fn route(
        &mut self,
        ev: &Event,
        world: &World,
        rt: &HashMap<EntityId, EntityRuntime>,
        now: u64,
        actor_is_agent: bool,
        allow_cross: bool,
        replay: Option<&ReplayFilter<'_>>,
    ) -> Vec<RouteNote> {
        let haystack = format!(
            "{} {}",
            ev.untrusted_text.as_deref().unwrap_or(""),
            ev.narrative
        );
        let mut notes = Vec::new();
        for r in &ev.recipients {
            if ev.is_by(r) {
                continue;
            }
            let Some(who) = world.entity(r) else { continue };
            if who.human
                || !who.online
                || matches!(who.tier, Tier::None | Tier::Mob)
                || who.kind == EntityKind::Mob
            {
                continue;
            }
            if replay.is_some_and(|due| !due(r, ev)) {
                continue;
            }
            let Some(mut priority) = Self::priority_for(ev, who) else {
                continue;
            };
            if replay.is_some() && priority != Priority::Immediate {
                continue;
            }
            if is_came_to(ev) && ev.targets(r) {
                notes.push(self.came_to(r, ev.seq, now));
                continue;
            }
            let drop = |why: &str| RouteNote::Dropped {
                entity: r.clone(),
                seq: ev.seq,
                why: why.to_owned(),
            };
            // Even naming each other cannot restart a quiet room: only a
            // human or a macro-tick does.
            if actor_is_agent && !allow_cross {
                notes.push(drop("anti-chatter: agent-only turns exhausted"));
                continue;
            }
            let state = rt.get(r).cloned().unwrap_or_default();
            match state.budget {
                Some(BudgetLevel::Hard) if priority < Priority::Immediate => {
                    notes.push(drop("budget: hard cap"));
                    continue;
                }
                Some(BudgetLevel::Soft) if priority == Priority::Ambient => {
                    notes.push(drop("budget: soft cap disables ambient wakes"));
                    continue;
                }
                _ => {}
            }
            if who.tier == Tier::Turing
                && priority < Priority::Immediate
                && let Some(spec) = world.pack().cast.get(r).and_then(|c| c.mind.turing.clone())
            {
                let s = Salience {
                    named: names(&haystack, &who.name, &who.aliases),
                    combat_or_loot: matches!(ev.kind, EventKind::Combat | EventKind::Death)
                        || ev.tags.iter().any(|t| t == "loot" || t == "combat"),
                    goal_keyword: has_keyword(&spec, &haystack),
                    arrival: ev.kind == EventKind::Enter,
                };
                let h = mix(
                    self.cfg.seed,
                    &[r.as_str().as_bytes(), &ev.seq.0.to_le_bytes(), b"attention"],
                );
                if !notices(&spec, s, h) {
                    notes.push(drop("selective attention"));
                    continue;
                }
                if s.named || s.combat_or_loot {
                    priority = priority.max(Priority::RoomBatch);
                }
            }
            let due = match priority {
                Priority::Immediate => now + self.jitter(self.cfg.immediate_ms, r, ev.seq, b"imm"),
                Priority::RoomBatch => now + self.cfg.room_batch_ms[0],
                Priority::Ambient => now + self.jitter(self.cfg.ambient_ms, r, ev.seq, b"amb"),
                Priority::MacroTick | Priority::Booked => now,
            };
            let mut floor = 0;
            let mut cooldown_ms = 0;
            if priority < Priority::Immediate {
                let range = world
                    .pack()
                    .cast
                    .get(r)
                    .and_then(|c| c.mind.cooldown_secs)
                    .map_or(self.cfg.cooldown_ms, |[a, b]| [a * 1_000, b * 1_000]);
                let mut cooldown = self.jitter(range, r, ev.seq, b"cool");
                if state.budget == Some(BudgetLevel::Soft) {
                    cooldown *= 2;
                }
                cooldown_ms = cooldown;
                if let Some(end) = self.last_wake_end.get(r) {
                    floor = end + cooldown;
                }
                if let Some(until) = state.afk_until_ms {
                    floor = floor.max(until);
                }
            }
            let entry = self.pending.entry(r.clone());
            let why = reason(ev, replay.is_some());
            let p =
                entry.or_insert_with(|| Pending::new(priority, why.clone(), now, due.max(floor)));
            p.seqs.push(ev.seq);
            p.causes.push((ev.seq, priority, why.clone()));
            if priority < Priority::Immediate {
                p.cooldown_ms = p.cooldown_ms.max(cooldown_ms);
                p.afk_until_ms = p.afk_until_ms.max(state.afk_until_ms);
            }
            if priority > p.priority {
                p.priority = priority;
                p.due_ms = p.due_ms.min(due);
                p.reason = why;
            } else if p.priority == Priority::RoomBatch && priority == Priority::RoomBatch {
                // Rolling window: each new line pushes the wake back, up to the cap.
                p.due_ms = due
                    .min(p.first_ms + self.cfg.room_batch_ms[1])
                    .max(p.due_ms);
            }
            // Cooldowns and distracted spells hold every non-urgent wake.
            if p.priority < Priority::Immediate {
                p.due_ms = p.due_ms.max(floor);
            }
            notes.push(RouteNote::Queued {
                entity: r.clone(),
                priority: p.priority,
                seq: ev.seq,
                due_ms: p.due_ms,
            });
        }
        notes
    }

    /// A macro-tick for `entity`. It waits while a player other than the
    /// entity acted in its room within `macro_defer_ms` ([`tick_deferred`]):
    /// that player's acts wake the entity anyway. An idle player, human or
    /// agent, does not hold it back past the window. A hidden PC is a
    /// player, and alone it still gets its quiet wakes.
    ///
    /// A tick resets its room's anti-chatter count, unless it is a
    /// `retry` of a tick whose wake never reached the mind: the tick it
    /// retries reset the room already, and a mind that stays down must
    /// not reopen a held room every few minutes.
    pub fn macro_tick(
        &mut self,
        entity: &EntityId,
        world: &World,
        now: u64,
        retry: bool,
    ) -> RouteNote {
        let seq = world.last_seq();
        let Some(e) = world.entity(entity) else {
            return RouteNote::Dropped {
                entity: entity.clone(),
                seq,
                why: "unknown entity".into(),
            };
        };
        let window = self.cfg.macro_defer_ms;
        let last = world.last_player_act(&e.room, entity, now.saturating_sub(window));
        if tick_deferred(last, now, window) {
            return RouteNote::Dropped {
                entity: entity.clone(),
                seq,
                why: "macro-tick deferred: a player acted here recently".into(),
            };
        }
        if !retry {
            let max = self.cfg.max_agent_only_turns;
            let room = e.room.clone();
            self.room(&room).step(ChatterInput::MacroTick, max);
        }
        let p = self
            .pending
            .entry(entity.clone())
            .or_insert_with(|| Pending::new(Priority::MacroTick, "macro-tick".into(), now, now));
        // The tick keeps its identity even inside a more urgent wake.
        p.macro_tick = true;
        p.force_cause(Priority::MacroTick, "macro-tick");
        p.due_ms = p.due_ms.min(now);
        RouteNote::Queued {
            entity: entity.clone(),
            priority: p.priority,
            seq,
            due_ms: p.due_ms,
        }
    }

    /// Queues a wake for a reason outside the event stream, such as a
    /// session start or an interrupted line.
    pub fn force(&mut self, entity: &EntityId, priority: Priority, reason: &str, now: u64) {
        let p = self
            .pending
            .entry(entity.clone())
            .or_insert_with(|| Pending::new(priority, reason.to_owned(), now, now));
        p.force_cause(priority, reason);
        if priority >= p.priority {
            p.priority = priority;
            reason.clone_into(&mut p.reason);
        }
        p.due_ms = p.due_ms.min(now);
    }

    /// Queues the one wake an entity gets when it comes to: Immediate,
    /// reason [`CAME_TO`], carrying its come-to line `seq`. It merges with
    /// everything held while the entity could not act. The tick's report
    /// and the line itself name the same `seq`; only the first wakes it.
    pub fn came_to(&mut self, entity: &EntityId, seq: Seq, now: u64) -> RouteNote {
        if self.came_to_seen.get(entity).is_some_and(|s| *s >= seq) {
            return RouteNote::Dropped {
                entity: entity.clone(),
                seq,
                why: "came to: already woken for it".into(),
            };
        }
        self.came_to_seen.insert(entity.clone(), seq);
        self.force(entity, Priority::Immediate, CAME_TO, now);
        let p = self.pending.get_mut(entity);
        let due_ms = p.map_or(now, |p| {
            if !p.seqs.contains(&seq) {
                p.seqs.push(seq);
            }
            p.due_ms
        });
        RouteNote::Queued {
            entity: entity.clone(),
            priority: Priority::Immediate,
            seq,
            due_ms,
        }
    }

    /// Gives a wake taken as due back to the router, as waiting: its
    /// entity could not act when the wake started. Everything it carries
    /// merges into the entity's waiting wake, which stays held until the
    /// entity comes to. The entity is no longer busy.
    pub fn hold(&mut self, wake: Wake) {
        self.busy.remove(&wake.entity);
        if let Some(plan) = wake.resume {
            self.resumes.insert(wake.entity, plan);
            return;
        }
        let p = self
            .pending
            .entry(wake.entity.clone())
            .or_insert_with(|| Pending::new(wake.priority, wake.reason.clone(), 0, 0));
        // A wake given back carries what its entity never saw.
        p.force_cause(wake.priority, &wake.reason);
        let mut seqs = wake.seqs;
        for s in std::mem::take(&mut p.seqs) {
            if !seqs.contains(&s) {
                seqs.push(s);
            }
        }
        seqs.sort_unstable();
        p.seqs = seqs;
        p.macro_tick |= wake.macro_tick;
        let mut booked = wake.booked;
        booked.append(&mut p.booked);
        p.booked = booked;
        if wake.priority >= p.priority {
            p.priority = wake.priority;
            p.reason = wake.reason;
        }
        p.due_ms = 0;
    }

    /// Queues the booked wakes that came due for `entity`, now. They merge
    /// into a wake already waiting, which then carries their notes.
    pub fn deliver_booked(
        &mut self,
        entity: &EntityId,
        bookings: Vec<crate::wakes::Booking>,
        now: u64,
    ) -> RouteNote {
        self.force(entity, Priority::Booked, "booked wake", now);
        let p = self.pending.get_mut(entity);
        let (priority, due_ms) = p.map_or((Priority::Booked, now), |p| {
            p.booked.extend(bookings);
            (p.priority, p.due_ms)
        });
        RouteNote::Queued {
            entity: entity.clone(),
            priority,
            due_ms,
            seq: Seq(0),
        }
    }

    /// Takes the plan resumes of the entities that `ready` accepts (online,
    /// and able to act) and that have no wake in flight. A resume waits
    /// for its entity: it runs before any other wake of that entity.
    pub fn resumes_due(&mut self, ready: impl Fn(&EntityId) -> bool) -> Vec<Wake> {
        let resuming: Vec<EntityId> = self
            .resumes
            .keys()
            .filter(|e| !self.busy.contains(*e) && ready(e))
            .cloned()
            .collect();
        let mut out = Vec::new();
        for e in resuming {
            if let Some(plan) = self.resumes.remove(&e) {
                self.busy.insert(e.clone());
                out.push(Wake {
                    entity: e,
                    priority: Priority::Booked,
                    seqs: Vec::new(),
                    reason: format!("resume plan at line {}", plan.cursor.saturating_add(1)),
                    macro_tick: false,
                    booked: Vec::new(),
                    resume: Some(plan),
                });
            }
        }
        out
    }

    /// Takes the wakes that are due, skipping entities with one in flight
    /// or a plan resume waiting (see [`Router::resumes_due`]). Every entity
    /// counts as able to act; the supervisor uses [`Router::due_where`].
    pub fn due(&mut self, now: u64) -> Vec<Wake> {
        self.due_where(now, |_| true)
    }

    /// Takes the wakes that are due, as [`Router::due`], holding the wake
    /// of every entity that `can_act` refuses (unconscious or dead): it
    /// stays waiting, gathers what comes meanwhile, and goes out when the
    /// entity can act again ([`wake_admit`]).
    pub fn due_where(&mut self, now: u64, can_act: impl Fn(&EntityId) -> bool) -> Vec<Wake> {
        let ready: Vec<EntityId> = self
            .pending
            .iter()
            .filter(|(e, p)| {
                let free = !self.busy.contains(*e) && !self.resumes.contains_key(*e);
                wake_admit(can_act(e), free, p.due_ms, now) == Admit::Take
            })
            .map(|(e, _)| e.clone())
            .collect();
        let mut out = Vec::new();
        for e in ready {
            if let Some(p) = self.pending.remove(&e) {
                self.busy.insert(e.clone());
                out.push(Wake {
                    entity: e,
                    priority: p.priority,
                    seqs: p.seqs,
                    reason: p.reason,
                    macro_tick: p.macro_tick,
                    booked: p.booked,
                    resume: None,
                });
            }
        }
        out.sort_by_key(|w| std::cmp::Reverse(w.priority));
        out
    }

    /// Marks a wake finished, as [`Router::finish`], once its mind saw the
    /// events up to `seen` (the newest it was handed, [`Game::read_mark`]
    /// of mud-world). The wake waiting for the entity drops the events at
    /// or below `seen`: the mind already read and answered them, and
    /// replaying them would feed it stale input. Its priority and reason
    /// fall back to what is left; a wake left with no event and no cause
    /// outside the event stream is dropped.
    ///
    /// [`Game::read_mark`]: mud_world::Game::read_mark
    pub fn finish_seen(&mut self, entity: &EntityId, now: u64, seen: Seq) {
        if let Some(p) = self.pending.get_mut(entity)
            && p.seqs.iter().any(|s| *s <= seen)
        {
            p.seqs.retain(|s| *s > seen);
            p.causes.retain(|(s, _, _)| *s > seen);
            let best = p
                .causes
                .iter()
                .map(|(_, pr, why)| (*pr, why))
                .chain(p.forced.as_ref().map(|(pr, why)| (*pr, why)))
                .fold(None::<(Priority, &String)>, |best, c| match best {
                    Some(b) if b.0 >= c.0 => Some(b),
                    _ => Some(c),
                })
                .map(|(pr, why)| (pr, why.clone()));
            match best {
                Some((priority, why)) => {
                    p.priority = priority;
                    p.reason = why;
                }
                None if p.seqs.is_empty() => {
                    self.pending.remove(entity);
                }
                None => {}
            }
        }
        self.finish(entity, now);
    }

    /// Marks a wake finished; its cooldown starts now. A non-urgent wake
    /// queued meanwhile waits out that cooldown (and any distracted spell).
    pub fn finish(&mut self, entity: &EntityId, now: u64) {
        self.busy.remove(entity);
        self.last_wake_end.insert(entity.clone(), now);
        if let Some(p) = self.pending.get_mut(entity)
            && p.priority < Priority::Immediate
        {
            let floor = (now + p.cooldown_ms).max(p.afk_until_ms.unwrap_or(0));
            p.due_ms = p.due_ms.max(floor);
        }
    }

    /// Forgets every waiting wake of an entity (it logged out). Returns
    /// the ids of the booked wakes it carried, for the wake book to give
    /// back: they come due again when the entity returns.
    pub fn forget(&mut self, entity: &EntityId) -> Vec<u64> {
        self.pending
            .remove(entity)
            .map(|p| p.booked.iter().map(|b| b.id).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests;
