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
//! | `MacroTick` | the slow timer, only with no human in the room | none |
//!
//! It also runs the per-room anti-chatter counter, the budget breakers,
//! the Turing attention filter and distracted spells.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use mud_world::EntityId;
use mud_world::pack::{EntityKind, Tier};
use mud_world::{Event, EventKind, RoomId, Seq, World};
use serde::{Deserialize, Serialize};

use crate::budget::BudgetLevel;
use crate::chatter::{ChatterInput, MAX_AGENT_ONLY_TURNS, RoomChatter};
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
        self.rooms.get(room).map_or(0, |r| r.agent_only_turns)
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
        match ev.kind {
            EventKind::System | EventKind::BoardRead | EventKind::Gear => None,
            _ => Some(Priority::Ambient),
        }
    }

    /// Reads one event. Returns what was decided for each recipient.
    #[expect(
        clippy::too_many_lines,
        reason = "one pass over the decision table reads best"
    )]
    pub fn observe(
        &mut self,
        ev: &Event,
        world: &World,
        rt: &HashMap<EntityId, EntityRuntime>,
        now: u64,
    ) -> Vec<RouteNote> {
        let actor = ev.actor.as_ref().and_then(|a| world.entity(&a.id));
        // An agent here is an agent-played NPC. A player character counts
        // as a player whoever plays it, so an agent PC resets the room and
        // is answered exactly as a human would be.
        let actor_is_agent = !ev.player_actor
            && actor.is_some_and(|a| {
                !a.human && matches!(a.tier, Tier::Ambient | Tier::Persistent | Tier::Turing)
            });
        let allow_cross = if let Some(room) = &ev.room
            && ev.kind != EventKind::SelfControlNudge
        {
            let input = if ev.player_actor {
                ChatterInput::Human
            } else if actor_is_agent {
                if world.player_present(room) {
                    ChatterInput::AgentWithHuman
                } else {
                    ChatterInput::AgentAlone
                }
            } else {
                ChatterInput::World
            };
            self.rooms
                .entry(room.clone())
                .or_default()
                .step(input, self.cfg.max_agent_only_turns)
        } else if ev.room.is_none()
            && actor_is_agent
            && let Some(a) = actor
        {
            // A roomless agent event (a tell) still counts, in the room of
            // the actor and of each party it reaches: agent-only unless a
            // human is with one of them.
            let rooms: BTreeSet<&RoomId> = std::iter::once(&a.room)
                .chain(
                    ev.recipients
                        .iter()
                        .filter(|r| !ev.is_by(r))
                        .filter_map(|r| world.entity(r))
                        .map(|t| &t.room),
                )
                .collect();
            let input = if rooms.iter().any(|room| world.player_present(room)) {
                ChatterInput::AgentWithHuman
            } else {
                ChatterInput::AgentAlone
            };
            let max = self.cfg.max_agent_only_turns;
            rooms.into_iter().fold(true, |allow, room| {
                let ok = self.rooms.entry(room.clone()).or_default().step(input, max);
                allow && ok
            })
        } else {
            true
        };
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
            let Some(mut priority) = Self::priority_for(ev, who) else {
                continue;
            };
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
            let p = entry.or_insert_with(|| Pending {
                priority,
                seqs: Vec::new(),
                first_ms: now,
                due_ms: due.max(floor),
                reason: format!(
                    "{} by {}",
                    ev.kind.name(),
                    ev.actor.as_ref().map_or("world", |a| a.name.as_str())
                ),
                macro_tick: false,
                cooldown_ms: 0,
                afk_until_ms: None,
                booked: Vec::new(),
            });
            p.seqs.push(ev.seq);
            if priority < Priority::Immediate {
                p.cooldown_ms = p.cooldown_ms.max(cooldown_ms);
                p.afk_until_ms = p.afk_until_ms.max(state.afk_until_ms);
            }
            if priority > p.priority {
                p.priority = priority;
                p.due_ms = p.due_ms.min(due);
                p.reason = format!(
                    "{} by {}",
                    ev.kind.name(),
                    ev.actor.as_ref().map_or("world", |a| a.name.as_str())
                );
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

    /// A macro-tick for `entity`. It fires only with no player in its room.
    pub fn macro_tick(&mut self, entity: &EntityId, world: &World, now: u64) -> RouteNote {
        let seq = world.last_seq();
        let Some(e) = world.entity(entity) else {
            return RouteNote::Dropped {
                entity: entity.clone(),
                seq,
                why: "unknown entity".into(),
            };
        };
        if world.player_present(&e.room) {
            return RouteNote::Dropped {
                entity: entity.clone(),
                seq,
                why: "macro-tick deferred: a player is present".into(),
            };
        }
        self.rooms
            .entry(e.room.clone())
            .or_default()
            .step(ChatterInput::MacroTick, self.cfg.max_agent_only_turns);
        let p = self
            .pending
            .entry(entity.clone())
            .or_insert_with(|| Pending {
                priority: Priority::MacroTick,
                seqs: Vec::new(),
                first_ms: now,
                due_ms: now,
                reason: "macro-tick".into(),
                macro_tick: true,
                cooldown_ms: 0,
                afk_until_ms: None,
                booked: Vec::new(),
            });
        // The tick keeps its identity even inside a more urgent wake.
        p.macro_tick = true;
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
            .or_insert_with(|| Pending {
                priority,
                seqs: Vec::new(),
                first_ms: now,
                due_ms: now,
                reason: reason.to_owned(),
                macro_tick: false,
                cooldown_ms: 0,
                afk_until_ms: None,
                booked: Vec::new(),
            });
        if priority >= p.priority {
            p.priority = priority;
            reason.clone_into(&mut p.reason);
        }
        p.due_ms = p.due_ms.min(now);
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

    /// Takes the plan resumes of the entities that `ready` accepts (online)
    /// and that have no wake in flight. A resume waits for its entity:
    /// it runs before any other wake of that entity.
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
    /// or a plan resume waiting (see [`Router::resumes_due`]).
    pub fn due(&mut self, now: u64) -> Vec<Wake> {
        let ready: Vec<EntityId> = self
            .pending
            .iter()
            .filter(|(e, p)| {
                p.due_ms <= now && !self.busy.contains(*e) && !self.resumes.contains_key(*e)
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
