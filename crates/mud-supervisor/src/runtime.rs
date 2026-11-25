//! The supervisor runtime: the loop that turns world events into wakes.
//!
//! One task reads every world event and feeds the router. Another steps
//! the clock: world ticks (respawns, regeneration), Turing sessions and
//! distracted spells, macro-ticks, and due wakes. Each wake runs on its
//! own task, one at a time per entity, under a timeout, and its spend
//! goes into the entity's daily budget.
//!
//! What a restart must not lose lives in the world store (ADR 0011): the
//! bookings, the macro schedule and count, the plan of a scripted wake with
//! its cursor, the scripted rule counters, and each day's spend. A wake's
//! obligations are staged on its turn and commit with the mind's first
//! effectful command, or at the turn's close; [`Supervisor::open`] restores
//! the rows at boot.
//!
//! The spend is split by source (phase 3): what the supervisor metered and
//! what littleguy's ledger reported. The close of each wake stores the
//! day's tallies (`Spend`, joined by the proved [`crate::persist`]
//! kernels); at boot the stored tallies come back, and each ledger mind
//! primes its meter, so a restart neither bills a ledger row twice nor
//! forgets today's spend, and the breaker stands where it stood.
//!
//! What changes at event rate is derived, not stored (phase 4,
//! [`crate::replay`]): at boot the router folds the reloaded event log into
//! its chatter counters and queues an Immediate wake for each addressed
//! event that no committed wake of its recipient consumed and that is at
//! most `replay_max_age_ms` old.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mud_world::pack::Tier;
use mud_world::side::{
    BookingRow, EntityRow, MacroRow, PlanRow, SideState, macro_consume, macro_queue,
};
use mud_world::{EntityId, Event, EventKind, Game, Seq, SideOp};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::budget::{Budget, BudgetLevel, DAY_MS, SpendSource};
use crate::gate::Gate;
use crate::held::{assert_none_held, lock};
use crate::mind::{Mind, MindReply, MudTools, Usage, UsageSink, WakeContext};
use crate::persist::{DaySpend, macro_open, plan_resume, restore_macro, wake_id_key};
use crate::replay::replay_floor;
use crate::router::{EntityRuntime, Priority, Resume, RouteNote, Router, RouterConfig, Wake};
use crate::scripted::{FallbackMind, PlanMind};
use crate::turing::{Afk, in_session, mix, pick};
use crate::wakes::WakeBook;

/// The supervisor settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SupervisorConfig {
    /// The router timings.
    pub router: RouterConfig,
    /// The seed of every schedule.
    pub seed: u64,
    /// The default daily token budget of each agent. `None` is unlimited.
    pub daily_token_budget: Option<u64>,
    /// Per-entity budgets that override the default. `None` is unlimited.
    pub budgets: HashMap<EntityId, Option<u64>>,
    /// Scales macro-tick intervals (1.0: the cast's hours are hours).
    pub macro_tick_scale: f64,
    /// When the first macro-tick comes after start, in seconds.
    pub first_macro_tick_secs: [u64; 2],
    /// How often the world ticks, in ms.
    pub world_tick_ms: u64,
    /// The bound on one wake, in ms.
    pub wake_timeout_ms: u64,
    /// A reactive plan (one a wake made that carried no macro tick, booking
    /// or login) older than this at boot is abandoned, not resumed, and an
    /// addressed event older than this is not replayed (phase 4). An
    /// errand resumes however long the server was down.
    #[serde(default = "default_replay_max_age_ms")]
    pub replay_max_age_ms: u64,
    /// A booking overdue by more than this at boot is dropped, not fired.
    #[serde(default = "default_booking_max_late_ms")]
    pub booking_max_late_ms: u64,
    /// A macro tick missed while the server was down fires once, within
    /// this many ms of boot.
    #[serde(default = "default_catchup_spread_ms")]
    pub catchup_spread_ms: u64,
}

const fn default_replay_max_age_ms() -> u64 {
    120_000
}

const fn default_booking_max_late_ms() -> u64 {
    86_400_000
}

const fn default_catchup_spread_ms() -> u64 {
    120_000
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            router: RouterConfig::default(),
            seed: 0x6c69_7474_6c65,
            daily_token_budget: Some(400_000),
            budgets: HashMap::new(),
            macro_tick_scale: 1.0,
            first_macro_tick_secs: [30, 120],
            world_tick_ms: 5_000,
            wake_timeout_ms: 120_000,
            replay_max_age_ms: default_replay_max_age_ms(),
            booking_max_late_ms: default_booking_max_late_ms(),
            catchup_spread_ms: default_catchup_spread_ms(),
        }
    }
}

/// One entry of the observer feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObserverEvent {
    /// A routing decision.
    Route(RouteNote),
    /// A wake started.
    WakeStarted {
        /// Who.
        entity: EntityId,
        /// Its id, unique across restarts and world resets.
        wake_id: String,
        /// How urgent.
        priority: Priority,
        /// Why.
        reason: String,
        /// Which mind runs it.
        mind: String,
        /// When.
        at_ms: u64,
    },
    /// A wake ended.
    WakeFinished {
        /// Who.
        entity: EntityId,
        /// Whether the mind succeeded.
        ok: bool,
        /// Its private note or the error.
        note: Option<String>,
        /// What it spent.
        usage: Usage,
        /// Its budget level afterwards.
        budget: BudgetLevel,
        /// How long it took, in ms.
        took_ms: u64,
    },
    /// A hidden PC logged in or out.
    Session {
        /// Who.
        entity: EntityId,
        /// Whether it is online now.
        online: bool,
    },
    /// A hidden PC stepped away.
    Afk {
        /// Who.
        entity: EntityId,
        /// Until when.
        until_ms: u64,
    },
    /// A held line was dropped because combat broke out.
    Interrupted {
        /// Who.
        entity: EntityId,
    },
    /// A booking restored at boot was overdue past `booking_max_late_ms`
    /// and was dropped. The note stays out of the feed.
    BookingDropped {
        /// Whose.
        entity: EntityId,
        /// The booking id.
        id: u64,
        /// When it was due.
        due_ms: u64,
    },
    /// A reactive plan found at boot was older than `replay_max_age_ms`
    /// and was abandoned, not resumed.
    PlanAbandoned {
        /// Whose.
        entity: EntityId,
        /// The wake that made it.
        wake_id: String,
        /// The lines that had taken effect.
        cursor: u32,
        /// Its length.
        len: u32,
    },
}

#[derive(Debug, Clone, Default)]
struct EntityState {
    budget: Budget,
    afk: Afk,
    afk_until: Option<u64>,
    /// The macro counters, as the store keeps them (`sup_entity`).
    macro_row: MacroRow,
    /// A tick owed at boot (`queued > cleared`): the first macro tick after
    /// boot delivers it, under its number, without queuing another.
    owed_at_boot: bool,
    next_macro_ms: u64,
    /// The replay floor at boot ([`replay_floor`]), then the last event
    /// seq a wake of this process carried.
    seen_seq: u64,
    last_wake: Option<(Priority, String, u64)>,
    wakes: u64,
}

/// The supervisor.
///
/// Lock order: `world → (router | state | wakebook, one at a time) → store
/// cell`. The world (`Game::read`, `Game::write` and every other `Game`
/// call) comes first. The supervisor's own `router`, `state` and wake-book
/// locks may be taken inside a world closure, one at a time, but are never
/// held while calling into `Game`, never nested in each other, and the
/// gate's lock is a leaf. The store cell inside `Game` is the last leaf:
/// `Game::open_turn`, `Game::stage`, `Game::persist` and `Game::retain`
/// take only it, while `Game::act` and `Game::close_turn` take the world
/// first, so they are never called with `router`, `state` or the wake
/// book held (ADR 0002, ADR 0011). Breaking this order deadlocks the whole
/// world; a debug build checks it (`crate::held`).
#[derive(Debug)]
pub struct Supervisor {
    game: Arc<Game>,
    gate: Arc<Gate>,
    cfg: SupervisorConfig,
    minds: HashMap<EntityId, Arc<dyn Mind>>,
    fallback: Arc<FallbackMind>,
    router: Mutex<Router>,
    state: Mutex<HashMap<EntityId, EntityState>>,
    observer: broadcast::Sender<Arc<ObserverEvent>>,
    /// Spend minds could not put in a reply (failed or timed-out wakes).
    usage: Arc<UsageSink>,
    last_world_tick: AtomicU64,
    /// The wakes each agent booked for itself.
    wakes: Arc<WakeBook>,
    /// Each entity's wakes so far in this boot: the `n` of its next wake
    /// id (see [`wake_id_key`]). Counted per entity, so an id, and the
    /// dice of a Rhai mind seeded by it, never depend on other entities'
    /// wakes or on which task the scheduler runs first.
    wake_n: Mutex<HashMap<EntityId, u64>>,
    /// What the boot reconcile dropped, abandoned or replayed, kept for observers
    /// that subscribe after [`Supervisor::open`] (see
    /// [`Supervisor::boot_report`]).
    boot_report: Mutex<Vec<Arc<ObserverEvent>>>,
}

impl Supervisor {
    /// A supervisor over `game`, with a mind for each agent entity. It
    /// restores what `game` loaded from its store, if it has not been
    /// taken (see [`Supervisor::open`]).
    #[must_use]
    pub fn new(
        game: Arc<Game>,
        typing: Arc<Gate>,
        minds: HashMap<EntityId, Arc<dyn Mind>>,
        fallback: Arc<FallbackMind>,
        cfg: SupervisorConfig,
    ) -> Arc<Self> {
        let restored = game.take_restored().unwrap_or_default();
        Self::open(game, typing, minds, fallback, cfg, restored)
    }

    /// A supervisor over `game` that restores `restored`, the side rows
    /// [`Game::take_restored`] handed over: the bookings, the macro
    /// schedule and counters, the scripted rule counters and the plans in
    /// progress (ADR 0011). Nothing is redelivered: each obligation
    /// re-enters the normal paths, which wait for its entity to be ready.
    #[must_use]
    pub fn open(
        game: Arc<Game>,
        typing: Arc<Gate>,
        minds: HashMap<EntityId, Arc<dyn Mind>>,
        fallback: Arc<FallbackMind>,
        cfg: SupervisorConfig,
        restored: SideState,
    ) -> Arc<Self> {
        let sup = Self::build(game, typing, minds, fallback, cfg);
        sup.reconcile(restored);
        sup
    }

    fn build(
        game: Arc<Game>,
        typing: Arc<Gate>,
        minds: HashMap<EntityId, Arc<dyn Mind>>,
        fallback: Arc<FallbackMind>,
        cfg: SupervisorConfig,
    ) -> Arc<Self> {
        let now = game.now_ms();
        let mut state = HashMap::new();
        for id in minds.keys() {
            let limit = cfg
                .budgets
                .get(id)
                .copied()
                .unwrap_or(cfg.daily_token_budget);
            let h = mix(cfg.seed, &[id.as_str().as_bytes(), b"first-macro"]);
            let first = pick(
                h,
                cfg.first_macro_tick_secs[0],
                cfg.first_macro_tick_secs[1],
            ) * 1_000;
            state.insert(
                id.clone(),
                EntityState {
                    budget: Budget::new(limit),
                    next_macro_ms: now + first,
                    ..EntityState::default()
                },
            );
        }
        let (observer, _) = broadcast::channel(4_096);
        let wakes = Arc::new(WakeBook::persisted(Arc::clone(&game)));
        Arc::new(Self {
            router: Mutex::new(Router::new(cfg.router.clone())),
            game,
            gate: typing,
            cfg,
            minds,
            fallback,
            state: Mutex::new(state),
            observer,
            last_world_tick: AtomicU64::new(now),
            usage: Arc::default(),
            wakes,
            wake_n: Mutex::new(HashMap::new()),
            boot_report: Mutex::new(Vec::new()),
        })
    }

    /// The longest macro interval of each agent, in ms: a restored next
    /// tick further away than this is clamped.
    fn macro_spans(&self) -> HashMap<EntityId, u64> {
        self.game.read(|w| {
            self.minds
                .keys()
                .map(|id| {
                    let hi = w
                        .pack()
                        .cast
                        .get(id)
                        .and_then(|c| c.mind.macro_tick_hours)
                        .map_or(6.0, |[_, hi]| hi);
                    (id.clone(), self.hours_ms(hi))
                })
                .collect()
        })
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "hours in ms fit; a negative or NaN scale reads as 0"
    )]
    fn hours_ms(&self, hours: f64) -> u64 {
        (hours * 3_600_000.0 * self.cfg.macro_tick_scale) as u64
    }

    /// Boot reconcile (ADR 0011): restores the rows and writes back what
    /// it changed (a clamped or caught-up macro time, dropped bookings,
    /// finished or abandoned plans, the counts of rules that are gone).
    /// Those writes may be lost to a crash: the next boot redoes them.
    /// Each dropped booking and abandoned plan is logged and kept in the
    /// boot report, since nobody can subscribe to the feed before `open`
    /// returns.
    fn reconcile(&self, restored: SideState) {
        let now = self.game.now_ms();
        let SideState {
            next_booking_id,
            entities,
            bookings,
            plans,
            rule_counts,
            ..
        } = restored;
        let mut ops = Vec::new();
        for (e, rows) in &rule_counts {
            if let Some(m) = self.minds.get(e) {
                let stale = m.restore(e, rows);
                if !stale.is_empty() {
                    ops.push(SideOp::RuleCountsDrop {
                        entity: e.clone(),
                        keys: stale.iter().map(|r| (r.digest, r.dup)).collect(),
                    });
                }
            }
        }
        let spans = self.macro_spans();
        // Each ledger meter is read before the state lock: a file read.
        let primed: Vec<(EntityId, Usage)> = self
            .minds
            .iter()
            .filter_map(|(e, m)| m.prime(e, now).map(|u| (e.clone(), u)))
            .collect();
        for (e, row) in &entities {
            if let (Some(m), Some(seen)) = (self.minds.get(e), row.harvest_tokens_seen) {
                m.restore_tokens_seen(e, seen);
            }
        }
        {
            let mut st = lock(&self.state);
            for (e, s) in st.iter_mut() {
                s.budget.roll(now);
                if let Some(row) = entities.get(e) {
                    let stored = DaySpend {
                        day: row.budget_day,
                        local: row.local,
                        lg: row.lg,
                    };
                    if s.budget.restore(stored, now) {
                        tracing::warn!(
                            entity = %e,
                            stored_day = row.budget_day,
                            today = now / DAY_MS,
                            "a budget stored more than a day ahead of the clock was reset"
                        );
                        // The store is reset too: its stale day would drop
                        // today's spend and come back on that day.
                        ops.push(SideOp::SpendReset {
                            entity: e.clone(),
                            day: s.budget.spend.day,
                            local: s.budget.spend.local,
                            lg: s.budget.spend.lg,
                        });
                    }
                }
            }
            for (e, u) in &primed {
                if let Some(s) = st.get_mut(e) {
                    s.budget.prime(now, u.tally());
                    // The ledger may drop the rows it was primed from (a
                    // littleguy purge): the stored tally keeps the floor.
                    ops.push(spend_op(e, &s.budget, None));
                }
            }
            for (e, row) in &entities {
                let Some(s) = st.get_mut(e) else { continue };
                s.macro_row = row.macro_row;
                s.owed_at_boot = row.macro_row.queued > row.macro_row.cleared;
                if let Some(next) = row.next_macro_ms {
                    let h = mix(
                        self.cfg.seed,
                        &[e.as_str().as_bytes(), b"catch-up", &next.to_le_bytes()],
                    );
                    let hi = spans.get(e).copied().unwrap_or(0);
                    let spread = pick(h, 0, self.cfg.catchup_spread_ms);
                    let mut at = restore_macro(next, now, hi, spread);
                    if s.owed_at_boot {
                        // The owed tick goes out within the catch-up
                        // spread, not hours later at the stored time.
                        at = at.min(now.saturating_add(spread));
                    }
                    s.next_macro_ms = at;
                    if at != next {
                        ops.push(SideOp::MacroSchedule {
                            entity: e.clone(),
                            next_ms: at,
                        });
                    }
                }
            }
            drop(st);
        }
        self.reconcile_bookings(bookings.into_values(), next_booking_id, now, &mut ops);
        self.reconcile_plans(plans, now, &mut ops);
        self.replay(&entities, now, &mut ops);
        if !ops.is_empty() {
            self.game.persist(ops);
        }
    }

    /// Derives the router from the reloaded event log (ADR 0011, phase 4):
    /// the chatter counters, and an Immediate wake for each addressed event
    /// no committed wake of its recipient consumed (after the recipient's
    /// [`replay_floor`]) that is at most `replay_max_age_ms` old. The
    /// decisions are published and kept in the boot report.
    ///
    /// An agent with no stored row gets one at the world's last seq, so
    /// the floor its MCP token was issued at ([`crate::replay::boot_floor`])
    /// stands at the next boot too. Nothing else raises a floor here: an
    /// event left unreplayed is still handed out by `mud_events`.
    fn replay(&self, entities: &BTreeMap<EntityId, EntityRow>, now: u64, ops: &mut Vec<SideOp>) {
        let last = self.game.read(|w| w.last_seq().0);
        let floors: HashMap<EntityId, u64> = self
            .minds
            .keys()
            .map(|e| {
                let row = entities.get(e);
                let floor = replay_floor(row.is_some(), row.map_or(0, |r| r.seen_seq), last);
                if row.is_none() {
                    ops.push(SideOp::SeenSeq {
                        entity: e.clone(),
                        seq: floor,
                    });
                }
                (e.clone(), floor)
            })
            .collect();
        {
            let mut st = lock(&self.state);
            for (e, f) in &floors {
                if let Some(s) = st.get_mut(e) {
                    s.seen_seq = *f;
                }
            }
        }
        let rt = self.runtimes(now);
        let max_age = self.cfg.replay_max_age_ms;
        // An entity without a mind has nothing to wake: its floor is the
        // last seq.
        let notes = self.game.read(|w| {
            lock(&self.router).replay(
                w,
                &rt,
                now,
                |e| floors.get(e).copied().unwrap_or(last),
                max_age,
            )
        });
        for n in notes {
            if let RouteNote::Queued { entity, seq, .. } = &n {
                tracing::info!(entity = %entity, seq = seq.0, "an addressed event was replayed at boot");
            }
            self.report(ObserverEvent::Route(n));
        }
    }

    /// The seen seq of `entity`: its replay floor at boot (where its MCP
    /// cursor started), then the last event a wake of this process carried.
    #[must_use]
    pub fn seen_seq(&self, entity: &EntityId) -> Option<Seq> {
        lock(&self.state).get(entity).map(|s| Seq(s.seen_seq))
    }

    /// Restores the bookings: each waits, fires when its entity is ready,
    /// or is dropped (deleted, logged and reported) when it is too late.
    fn reconcile_bookings(
        &self,
        bookings: impl IntoIterator<Item = BookingRow>,
        next_booking_id: u64,
        now: u64,
        ops: &mut Vec<SideOp>,
    ) {
        let dropped =
            self.wakes
                .restore(bookings, next_booking_id, now, self.cfg.booking_max_late_ms);
        if !dropped.is_empty() {
            ops.push(SideOp::BookingDelete {
                ids: dropped.iter().map(|b| b.id).collect(),
            });
        }
        for b in dropped {
            // The note is the agent's own text: it stays out of the log.
            tracing::warn!(
                entity = %b.entity,
                id = b.id,
                due_ms = b.due_ms,
                now_ms = now,
                "a booked wake was too late at boot and was dropped"
            );
            self.report(ObserverEvent::BookingDropped {
                entity: b.entity,
                id: b.id,
                due_ms: b.due_ms,
            });
        }
    }

    /// Queues a resume for each unfinished plan that may still run (an
    /// errand, or a reactive plan within `replay_max_age_ms`); the rest are
    /// done, and an abandoned one is logged and reported.
    fn reconcile_plans(&self, plans: BTreeMap<EntityId, PlanRow>, now: u64, ops: &mut Vec<SideOp>) {
        for (e, plan) in plans {
            let len = u32::try_from(plan.lines.len()).unwrap_or(u32::MAX);
            let start = plan_resume(plan.cursor, len);
            let fresh = plan.started_ms.saturating_add(self.cfg.replay_max_age_ms) >= now;
            if start < len && (plan.errand || fresh) && self.minds.contains_key(&e) {
                lock(&self.router).resume(
                    &e,
                    Resume {
                        wake_id: plan.wake_id,
                        lines: plan.lines,
                        cursor: start,
                    },
                );
                continue;
            }
            if start < len {
                tracing::warn!(
                    entity = %e,
                    wake_id = %plan.wake_id,
                    cursor = plan.cursor,
                    len,
                    started_ms = plan.started_ms,
                    "a plan found at boot was abandoned, not resumed"
                );
                self.report(ObserverEvent::PlanAbandoned {
                    entity: e.clone(),
                    wake_id: plan.wake_id.clone(),
                    cursor: plan.cursor,
                    len,
                });
            }
            ops.push(SideOp::PlanDone {
                entity: e,
                wake_id: plan.wake_id,
            });
        }
    }

    /// The game.
    #[must_use]
    pub const fn game(&self) -> &Arc<Game> {
        &self.game
    }

    /// The gate.
    #[must_use]
    pub const fn gate(&self) -> &Arc<Gate> {
        &self.gate
    }

    /// The wakes agents booked for themselves (`mud_wake_me`).
    #[must_use]
    pub const fn wakes(&self) -> &Arc<WakeBook> {
        &self.wakes
    }

    /// The agents that are in the world.
    fn online(&self) -> HashSet<EntityId> {
        self.game.read(|w| {
            self.minds
                .keys()
                .filter(|id| w.entity(id).is_some_and(|e| e.online))
                .cloned()
                .collect()
        })
    }

    /// Queues each booked wake that came due, for an agent that is ready:
    /// in the world and not stopped by its budget. One that is not (a
    /// Turing PC between sessions, an agent at its hard cap) keeps its
    /// booking until it is ready.
    fn booked_wakes(&self, now: u64, online: &HashSet<EntityId>) {
        let stopped: HashSet<EntityId> = lock(&self.state)
            .iter_mut()
            .filter_map(|(id, s)| (s.budget.level(now) == BudgetLevel::Hard).then(|| id.clone()))
            .collect();
        let ready = |e: &EntityId| online.contains(e) && !stopped.contains(e);
        for (entity, bookings) in self.wakes.take_due(now, ready) {
            let note = lock(&self.router).deliver_booked(&entity, bookings, now);
            self.publish(ObserverEvent::Route(note));
        }
    }

    /// Subscribes to the observer feed. It carries what happens from now
    /// on; what the boot did is in [`Supervisor::boot_report`].
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<ObserverEvent>> {
        self.observer.subscribe()
    }

    /// What the boot reconcile dropped (bookings too late to fire),
    /// abandoned (reactive plans too old to resume) and replayed (the
    /// routing of addressed events, phase 4), in order. It happened
    /// inside [`Supervisor::open`], before anyone could subscribe, so an
    /// observer shows it ahead of the live feed.
    #[must_use]
    pub fn boot_report(&self) -> Vec<Arc<ObserverEvent>> {
        lock(&self.boot_report).clone()
    }

    fn publish(&self, e: ObserverEvent) {
        let _ = self.observer.send(Arc::new(e));
    }

    /// Publishes a boot event and keeps it in the boot report.
    fn report(&self, e: ObserverEvent) {
        let e = Arc::new(e);
        lock(&self.boot_report).push(Arc::clone(&e));
        let _ = self.observer.send(e);
    }

    fn runtimes(&self, now: u64) -> HashMap<EntityId, EntityRuntime> {
        lock(&self.state)
            .iter_mut()
            .map(|(id, s)| {
                (
                    id.clone(),
                    EntityRuntime {
                        budget: Some(s.budget.level(now)),
                        afk_until_ms: s.afk_until,
                    },
                )
            })
            .collect()
    }

    /// Feeds one world event to the router.
    pub fn observe(&self, ev: &Event) {
        let now = self.game.now_ms();
        let rt = self.runtimes(now);
        // Combat near a hidden PC that is typing drops the line and wakes it.
        if matches!(ev.kind, EventKind::Combat | EventKind::Death) {
            for r in &ev.recipients {
                if self.gate.is_holding(r) && self.gate.interrupt(r) {
                    lock(&self.router).force(r, Priority::Immediate, "interrupted by combat", now);
                    self.publish(ObserverEvent::Interrupted { entity: r.clone() });
                }
            }
        }
        let notes = self
            .game
            .read(|w| lock(&self.router).observe(ev, w, &rt, now));
        for n in notes {
            self.publish(ObserverEvent::Route(n));
        }
    }

    /// Fires a macro-tick for `entity` now (the admin page and tests).
    ///
    /// A queued tick raises the entity's queued count (persisted); the
    /// wake that carries it numbers it when it opens (`run_wake`), so two
    /// ticks can merge into one wake. A tick owed at boot goes out first,
    /// without queuing another.
    pub fn fire_macro_tick(&self, entity: &EntityId) -> RouteNote {
        let now = self.game.now_ms();
        let note = self
            .game
            .read(|w| lock(&self.router).macro_tick(entity, w, now));
        if matches!(note, RouteNote::Queued { .. }) {
            let op = lock(&self.state).get_mut(entity).map(|s| {
                if s.owed_at_boot {
                    s.owed_at_boot = false;
                } else {
                    s.macro_row = macro_queue(s.macro_row, s.macro_row.queued.saturating_add(1));
                }
                SideOp::MacroQueued {
                    entity: entity.clone(),
                    queued: s.macro_row.queued,
                    next_ms: s.next_macro_ms,
                }
            });
            if let Some(op) = op {
                self.game.persist(vec![op]);
            }
        }
        self.publish(ObserverEvent::Route(note.clone()));
        note
    }

    fn turing_sessions(&self, now: u64) {
        let pcs: Vec<(EntityId, bool, bool, Option<mud_world::pack::TuringSpec>)> =
            self.game.read(|w| {
                self.minds
                    .keys()
                    .filter_map(|id| {
                        let e = w.entity(id)?;
                        (e.tier == Tier::Turing).then(|| {
                            let safe = w.pack().rooms.get(&e.room).is_some_and(|r| r.safe);
                            let spec = w.pack().cast.get(id).and_then(|c| c.mind.turing.clone());
                            (id.clone(), e.online, safe, spec)
                        })
                    })
                    .collect()
            });
        for (id, online, in_town, spec) in pcs {
            let Some(spec) = spec else { continue };
            let want = in_session(&spec, self.cfg.seed, id.as_str(), now);
            if want && !online {
                self.game.bring_online(&id);
                lock(&self.router).force(&id, Priority::Immediate, "login", now);
                self.publish(ObserverEvent::Session {
                    entity: id.clone(),
                    online: true,
                });
            } else if !want && online {
                self.game.take_offline(&id);
                let booked = lock(&self.router).forget(&id);
                self.wakes.requeue(&booked);
                self.publish(ObserverEvent::Session {
                    entity: id.clone(),
                    online: false,
                });
            }
            if want {
                // A spell starts only while idle: in town, with no wake in
                // flight or waiting and no line being typed.
                let idle = lock(&self.router).is_idle(&id) && !self.gate.is_holding(&id);
                let mut st = lock(&self.state);
                if let Some(s) = st.get_mut(&id) {
                    let was = s.afk_until;
                    let away =
                        s.afk
                            .update(&spec, self.cfg.seed, id.as_str(), now, in_town && idle);
                    s.afk_until = if away { s.afk.until_ms } else { None };
                    if let (Some(until), None) = (s.afk_until, was) {
                        drop(st);
                        self.publish(ObserverEvent::Afk {
                            entity: id.clone(),
                            until_ms: until,
                        });
                    }
                }
            }
        }
    }

    fn macro_ticks(&self, now: u64) {
        let due: Vec<(EntityId, Option<[f64; 2]>)> = self.game.read(|w| {
            let st = lock(&self.state);
            self.minds
                .keys()
                .filter_map(|id| {
                    let e = w.entity(id)?;
                    let s = st.get(id)?;
                    let cast = w.pack().cast.get(id)?;
                    let ticking = matches!(e.tier, Tier::Persistent | Tier::Turing) && e.online;
                    (ticking && now >= s.next_macro_ms)
                        .then(|| (id.clone(), cast.mind.macro_tick_hours))
                })
                .collect()
        });
        for (id, hours) in due {
            let note = self.fire_macro_tick(&id);
            let [lo, hi] = hours.unwrap_or([2.0, 6.0]);
            let h = mix(
                self.cfg.seed,
                &[id.as_str().as_bytes(), &now.to_le_bytes(), b"macro"],
            );
            // ±20% jitter is inside the cast's range already; scale for tests.
            let (lo_ms, hi_ms) = (self.hours_ms(lo), self.hours_ms(hi));
            let wait = match note {
                RouteNote::Queued { .. } => pick(h, lo_ms, hi_ms),
                // A human was there: try again in a few minutes.
                RouteNote::Dropped { .. } => pick(h, 120_000, 600_000),
            };
            let next_ms = now.saturating_add(wait.max(1));
            if let Some(s) = lock(&self.state).get_mut(&id) {
                s.next_macro_ms = next_ms;
            }
            // A lost schedule leaves an older, past time: it fires once at
            // the next boot's catch-up.
            self.game.persist(vec![SideOp::MacroSchedule {
                entity: id,
                next_ms,
            }]);
        }
    }

    /// One pass of the clock. Returns the wake tasks it started.
    pub fn step(self: &Arc<Self>) -> Vec<JoinHandle<()>> {
        let now = self.game.now_ms();
        if now.saturating_sub(self.last_world_tick.load(Ordering::Relaxed))
            >= self.cfg.world_tick_ms
        {
            self.last_world_tick.store(now, Ordering::Relaxed);
            self.game.tick();
        }
        self.charge_late_spend(now);
        self.turing_sessions(now);
        self.macro_ticks(now);
        let online = self.online();
        self.booked_wakes(now, &online);
        let mut wakes = lock(&self.router).resumes_due(|e| online.contains(e));
        wakes.extend(lock(&self.router).due(now));
        let mut tasks: Vec<JoinHandle<()>> = wakes
            .into_iter()
            .map(|w| {
                let me = Arc::clone(self);
                tokio::spawn(async move { me.run_wake(w).await })
            })
            .collect();
        if self.game.retention_due(now) {
            // Archive, then trim, old events off the async workers; each
            // batch holds only the store cell, so commands go on.
            let game = Arc::clone(&self.game);
            tasks.push(tokio::task::spawn_blocking(move || {
                // `retain` logs and counts its own failures.
                let _ = game.retain(now);
            }));
        }
        tasks
    }

    fn context(&self, wake: &Wake, wake_id: String, macro_ticks: u32) -> WakeContext {
        let now_ms = self.game.now_ms();
        self.game.read(|w| {
            let e = w.entity(&wake.entity);
            let events: Vec<Event> = w
                .events_since(&wake.entity, Seq(0), usize::MAX)
                .into_iter()
                .filter(|ev| wake.seqs.contains(&ev.seq))
                .cloned()
                .collect();
            WakeContext {
                entity: wake.entity.clone(),
                name: e.map_or_else(String::new, |e| e.name.clone()),
                priority: wake.priority,
                reason: wake.reason.clone(),
                event_views: events.iter().map(|ev| ev.view_for(&wake.entity)).collect(),
                events,
                look: w.look_view(&wake.entity),
                macro_tick: wake.macro_tick,
                macro_ticks,
                login: wake.reason == "login",
                wake_id,
                now_ms,
                booked: wake.booked.clone(),
            }
        })
    }

    /// Charges spend that came in after its wake closed (a model that
    /// kept going, ambient chatter).
    ///
    /// The spend of an entity whose wake is still open stays in the sink
    /// for that wake's close. A harvest job raises its token mark before
    /// it hands its spend over, and only the close stores the mark: a
    /// tally stored here, ahead of its mark, would be charged again after
    /// a kill before the close (ADR 0011). Once the close is done no job
    /// of the entity runs, so the stored mark already covers what is
    /// left.
    ///
    /// The new tallies are stored through [`Game::persist`]: a lost late
    /// `Spend` is a bounded under-charge, and the next close stores the
    /// day again.
    fn charge_late_spend(&self, now: u64) {
        let late = {
            let router = lock(&self.router);
            self.usage.take_where(|e| !router.is_busy(e))
        };
        if late.is_empty() {
            return;
        }
        let ops = self.charge(now, late);
        if !ops.is_empty() {
            self.game.persist(ops);
        }
    }

    /// Where `entity`'s spend is metered: its own mind's source (the
    /// fallback and the plan runner spend nothing).
    fn source(&self, entity: &EntityId) -> SpendSource {
        self.minds
            .get(entity)
            .map_or(SpendSource::Local, |m| m.spend_source())
    }

    /// Charges each spend to its entity's budget and returns the `Spend`
    /// ops that store the new tallies. They carry no harvest mark: only a
    /// wake's close, after its mind settled, knows the mark matches what
    /// was charged.
    fn charge(&self, now: u64, spends: Vec<(EntityId, Usage)>) -> Vec<SideOp> {
        let sources: Vec<SpendSource> = spends.iter().map(|(e, _)| self.source(e)).collect();
        let mut st = lock(&self.state);
        let ops = spends
            .into_iter()
            .zip(sources)
            .filter_map(|((id, u), source)| {
                let s = st.get_mut(&id)?;
                s.budget.spend(now, source, u.tally());
                Some(spend_op(&id, &s.budget, None))
            })
            .collect();
        drop(st);
        ops
    }

    /// The mind that runs `wake`, and the bookings it sees: none for a
    /// wake whose entity is offline (its bookings and plan go back), the
    /// plan runner for a resume, the fallback at the hard budget cap (its
    /// bookings go back: the fallback never sees them), else the entity's
    /// own mind.
    fn choose_mind(
        &self,
        wake: &Wake,
        start: u64,
    ) -> Option<(Arc<dyn Mind>, Vec<crate::wakes::Booking>)> {
        let ids: Vec<u64> = wake.booked.iter().map(|b| b.id).collect();
        let online = self
            .game
            .read(|w| w.entity(&wake.entity).is_some_and(|e| e.online));
        if !online {
            self.wakes.requeue(&ids);
            if let Some(plan) = &wake.resume {
                lock(&self.router).resume(&wake.entity, plan.clone());
            }
            return None;
        }
        if let Some(plan) = &wake.resume {
            // A resume calls no model: it runs even at the hard cap.
            return Some((Arc::new(PlanMind::new(plan.clone())), Vec::new()));
        }
        let level = lock(&self.state)
            .get_mut(&wake.entity)
            .map_or(BudgetLevel::Normal, |s| s.budget.level(start));
        if level == BudgetLevel::Hard {
            self.wakes.requeue(&ids);
            return Some((Arc::clone(&self.fallback) as Arc<dyn Mind>, Vec::new()));
        }
        let Some(mind) = self.minds.get(&wake.entity).cloned() else {
            self.wakes.requeue(&ids);
            return None;
        };
        Some((mind, wake.booked.clone()))
    }

    /// Opens the wake's turn (ADR 0011): its consume (the macro tick it
    /// carries, the bookings its mind sees, the last event seq) is staged,
    /// to commit with the mind's first effectful command or at close. The
    /// in-memory counters move now, so no other wake in this process takes
    /// the same obligations; the store's rows move with the commit, so a
    /// crash before it leaves them for the next boot. Returns the turn's
    /// key (a resume keeps its plan's) and the macro count the mind sees.
    fn open_wake(
        &self,
        wake: &Wake,
        wake_id: &str,
        booked: &[crate::wakes::Booking],
    ) -> (String, u32) {
        let seen = wake.seqs.iter().map(|s| s.0).max().unwrap_or(0);
        let (open, ticks) = lock(&self.state).get_mut(&wake.entity).map_or_else(
            || (macro_open(MacroRow::default(), false), 0),
            |s| {
                let row = s.macro_row;
                let open = macro_open(row, wake.macro_tick && wake.resume.is_none());
                s.macro_row = macro_consume(row, open.macro_no, open.cover);
                s.seen_seq = s.seen_seq.max(seen);
                let ticks = if open.macro_no > 0 {
                    open.macro_no
                } else {
                    row.ticks
                };
                (open, ticks)
            },
        );
        let ids: Vec<u64> = booked.iter().map(|b| b.id).collect();
        let turn = wake
            .resume
            .as_ref()
            .map_or_else(|| wake_id.to_owned(), |p| p.wake_id.clone());
        self.game.open_turn(
            &wake.entity,
            &turn,
            vec![SideOp::Consume {
                entity: wake.entity.clone(),
                macro_no: open.macro_no,
                macro_cover: open.cover,
                booked: ids.clone(),
                seen_seq: seen,
            }],
        );
        self.wakes.consumed(&wake.entity, &ids);
        (turn, ticks)
    }

    /// Runs one wake to the end and records it.
    ///
    /// The order is open, run, settle, close: the entity stays busy until
    /// its mind has really finished (a littleguy op read after a timeout, a
    /// harvest job), so a late command lands in this wake's turn and never
    /// consumes the next one's obligations. The close commits what the
    /// turn still has staged and ends its plan.
    pub async fn run_wake(self: Arc<Self>, wake: Wake) {
        let start = self.game.now_ms();
        let Some((mind, booked)) = self.choose_mind(&wake, start) else {
            lock(&self.router).finish(&wake.entity, start);
            return;
        };
        let n = *lock(&self.wake_n)
            .entry(wake.entity.clone())
            .and_modify(|n| *n += 1)
            .or_insert(1);
        let wake_id = wake_id_key(self.game.epoch(), self.game.boot(), n).id(&wake.entity);
        self.publish(ObserverEvent::WakeStarted {
            entity: wake.entity.clone(),
            wake_id: wake_id.clone(),
            priority: wake.priority,
            reason: wake.reason.clone(),
            mind: mind.kind().to_owned(),
            at_ms: start,
        });
        let (turn, macro_ticks) = self.open_wake(&wake, &wake_id, &booked);
        let mut ctx = self.context(&wake, wake_id, macro_ticks);
        ctx.booked = booked;
        let tools = MudTools::new(
            Arc::clone(&self.game),
            Arc::clone(&self.gate),
            wake.entity.clone(),
        )
        .with_usage_sink(Arc::clone(&self.usage))
        .with_wakes(Arc::clone(&self.wakes));
        let res = tokio::time::timeout(
            Duration::from_millis(self.cfg.wake_timeout_ms),
            mind.wake(ctx, tools),
        )
        .await;
        let (ok, reply) = match res {
            Ok(Ok(r)) => (true, r),
            Ok(Err(e)) => (
                false,
                MindReply {
                    text: Some(e.to_string()),
                    ..MindReply::default()
                },
            ),
            Err(_) => (
                false,
                MindReply {
                    text: Some("timed out".into()),
                    ..MindReply::default()
                },
            ),
        };
        let end = self.game.now_ms();
        // The reply's spend, plus what the mind could not put in a reply: a
        // failed wake, or one dropped by the timeout above.
        let usage = reply.usage.plus(self.usage.take(&wake.entity));
        let source = self.source(&wake.entity);
        let budget = {
            let mut st = lock(&self.state);
            st.get_mut(&wake.entity).map_or(BudgetLevel::Normal, |s| {
                s.budget.spend(end, source, usage.tally());
                s.last_wake = Some((wake.priority, wake.reason.clone(), start));
                s.wakes += 1;
                s.budget.level(end)
            })
        };
        self.publish(ObserverEvent::WakeFinished {
            entity: wake.entity.clone(),
            ok,
            note: reply.text,
            usage,
            budget,
            took_ms: end.saturating_sub(start),
        });
        mind.settled(&wake.entity).await;
        // What the mind spent after the timeout (a job or an op read late)
        // is in the sink by now: charge it, so the close stores the day's
        // tallies and the harvest mark together, and they agree.
        let late = self.usage.take(&wake.entity);
        let mark = self
            .minds
            .get(&wake.entity)
            .and_then(|m| m.tokens_seen(&wake.entity));
        let now = self.game.now_ms();
        self.charge(now, vec![(wake.entity.clone(), late)]);
        let spend = lock(&self.state)
            .get(&wake.entity)
            .map(|s| spend_op(&wake.entity, &s.budget, mark));
        assert_none_held("closing a turn");
        let mut ops: Vec<SideOp> = spend.into_iter().collect();
        ops.push(SideOp::PlanDone {
            entity: wake.entity.clone(),
            wake_id: turn.clone(),
        });
        let closed = self.game.close_turn(&wake.entity, &turn, ops);
        if let Err(e) = closed {
            // The store logged and counted it; the close's ops wait in its
            // backlog for the next commit.
            tracing::warn!(entity = %wake.entity, error = %e, "a wake's turn did not close");
        }
        lock(&self.router).finish(&wake.entity, self.game.now_ms());
    }

    /// Runs the supervisor: an event task and a clock task. Abort the
    /// returned handles to stop it.
    #[must_use]
    pub fn spawn(self: &Arc<Self>, every: Duration) -> Vec<JoinHandle<()>> {
        let me = Arc::clone(self);
        let mut rx = self.game.subscribe();
        let events = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => me.observe(&ev),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(missed = n, "the supervisor fell behind the world");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        let me = Arc::clone(self);
        let clock = tokio::spawn(async move {
            let mut t = tokio::time::interval(every);
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                t.tick().await;
                drop(me.step());
            }
        });
        vec![events, clock]
    }

    /// The observer dashboard view: every agent, its private goal, its
    /// last wake, its spend. Admin eyes only.
    #[must_use]
    pub fn snapshot(&self) -> Value {
        let now = self.game.now_ms();
        let pending: HashMap<EntityId, (Priority, u64)> = lock(&self.router)
            .pending()
            .into_iter()
            .map(|(e, p, d)| (e, (p, d)))
            .collect();
        // The world first, with no supervisor lock held (see the lock order).
        let mut ids: Vec<&EntityId> = self.minds.keys().collect();
        ids.sort();
        let seen: Vec<(&EntityId, Value)> = self.game.read(|w| {
            ids.into_iter()
                .filter_map(|id| {
                    let e = w.entity(id)?;
                    let cast = w.pack().cast.get(id)?;
                    Some((
                        id,
                        json!({
                            "id": id,
                            "name": e.name,
                            "tier": e.tier,
                            "mind": self.minds.get(id).map(|m| m.kind()),
                            "online": e.online,
                            "room": e.room,
                            "hp": e.pools.hp,
                            "currency": e.currency,
                            "goal": cast.goal,
                        }),
                    ))
                })
                .collect()
        });
        let typing: HashMap<&EntityId, bool> = seen
            .iter()
            .map(|(id, _)| (*id, self.gate.is_holding(id)))
            .collect();
        let booked = self.wakes.all();
        let mut st = lock(&self.state);
        let entities: Vec<Value> = seen
            .into_iter()
            .filter_map(|(id, mut v)| {
                let s = st.get_mut(id)?;
                let level = s.budget.level(now);
                let (local, lg) = (s.budget.spend.local, s.budget.spend.lg);
                let extra = json!({
                    "budget": {
                        "level": level,
                        "tokens": s.budget.tokens(),
                        "limit": s.budget.limit,
                        "cost_usd_micros": s.budget.cost_usd_micros(),
                        "calls": s.budget.calls(),
                        "local": { "tokens": local.tokens, "cost_usd_micros": local.cost_usd_micros, "calls": local.calls },
                        "lg": { "tokens": lg.tokens, "cost_usd_micros": lg.cost_usd_micros, "calls": lg.calls },
                    },
                    "wakes": s.wakes,
                    "last_wake": s.last_wake.as_ref().map(|(p, r, at)| json!({ "priority": p, "reason": r, "at_ms": at })),
                    "pending": pending.get(id).map(|(p, d)| json!({ "priority": p, "due_ms": d })),
                    "macro_ticks": s.macro_row.ticks,
                    "next_macro_ms": s.next_macro_ms,
                    "afk_until_ms": s.afk_until,
                    "typing": typing.get(id).copied().unwrap_or(false),
                    "booked": booked.get(id).map_or(&[][..], Vec::as_slice),
                });
                if let (Some(obj), Value::Object(more)) = (v.as_object_mut(), extra) {
                    obj.extend(more);
                }
                Some(v)
            })
            .collect();
        let tokens: u64 = st.values().map(|s| s.budget.tokens()).sum();
        let cost: u64 = st.values().map(|s| s.budget.cost_usd_micros()).sum();
        drop(st);
        json!({ "now_ms": now, "entities": entities, "tokens_today": tokens, "cost_usd_micros_today": cost })
    }

    /// Records spend from outside a wake (tests, external minds), metered
    /// by the supervisor, and stores the day's tallies.
    pub fn record_spend(&self, entity: &EntityId, usage: Usage) {
        let now = self.game.now_ms();
        let op = lock(&self.state).get_mut(entity).map(|s| {
            s.budget.spend(now, SpendSource::Local, usage.tally());
            spend_op(entity, &s.budget, None)
        });
        if let Some(op) = op {
            self.game.persist(vec![op]);
        }
    }

    /// The day's budget of `entity`: its level and the tallies by source
    /// (tests and the admin page).
    #[must_use]
    pub fn budget_of(&self, entity: &EntityId) -> Option<(BudgetLevel, DaySpend)> {
        let now = self.game.now_ms();
        lock(&self.state)
            .get_mut(entity)
            .map(|s| (s.budget.level(now), s.budget.spend))
    }
}

/// The `Spend` op that stores `budget`'s day: absolute tallies, joined in
/// the store by the proved [`crate::persist::budget_join`], so a retried,
/// reordered or lost op never stores more than memory holds.
fn spend_op(entity: &EntityId, budget: &Budget, harvest_tokens_seen: Option<u64>) -> SideOp {
    SideOp::Spend {
        entity: entity.clone(),
        day: budget.spend.day,
        local: budget.spend.local,
        lg: budget.spend.lg,
        harvest_tokens_seen,
    }
}

#[cfg(test)]
mod budget_tests;
#[cfg(test)]
mod persist_tests;
#[cfg(test)]
mod replay_tests;
#[cfg(test)]
mod tests;
