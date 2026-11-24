//! The supervisor runtime: the loop that turns world events into wakes.
//!
//! One task reads every world event and feeds the router. Another steps
//! the clock: world ticks (respawns, regeneration), Turing sessions and
//! distracted spells, macro-ticks, and due wakes. Each wake runs on its
//! own task, one at a time per entity, under a timeout, and its spend
//! goes into the entity's daily budget.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use mud_world::pack::Tier;
use mud_world::{EntityId, Event, EventKind, Game, Seq};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::budget::{Budget, BudgetLevel};
use crate::gate::Gate;
use crate::mind::{Mind, MindReply, MudTools, Usage, WakeContext};
use crate::router::{EntityRuntime, Priority, RouteNote, Router, RouterConfig, Wake};
use crate::scripted::FallbackMind;
use crate::turing::{Afk, in_session, mix, pick};

/// The supervisor settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SupervisorConfig {
    /// The router timings.
    pub router: RouterConfig,
    /// The seed of every schedule.
    pub seed: u64,
    /// The default daily token budget of each agent. `None` is unlimited.
    pub daily_token_budget: Option<u64>,
    /// Per-entity budgets that override the default.
    pub budgets: HashMap<EntityId, u64>,
    /// Scales macro-tick intervals (1.0: the cast's hours are hours).
    pub macro_tick_scale: f64,
    /// When the first macro-tick comes after start, in seconds.
    pub first_macro_tick_secs: [u64; 2],
    /// How often the world ticks, in ms.
    pub world_tick_ms: u64,
    /// The bound on one wake, in ms.
    pub wake_timeout_ms: u64,
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
}

#[derive(Debug, Clone, Default)]
struct EntityState {
    budget: Budget,
    afk: Afk,
    afk_until: Option<u64>,
    macro_ticks: u32,
    next_macro_ms: u64,
    last_wake: Option<(Priority, String, u64)>,
    wakes: u64,
}

/// The supervisor.
///
/// Lock order: the world (`Game::read`, `Game::write` and every other
/// `Game` call) comes first. The supervisor's own `router` and `state`
/// locks may be taken inside a world closure, one at a time, but are never
/// held while calling into `Game`, never nested in each other, and the
/// gate's lock is a leaf. Breaking this order deadlocks the whole world.
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
    last_world_tick: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Supervisor {
    /// A supervisor over `game`, with a mind for each agent entity.
    #[must_use]
    pub fn new(
        game: Arc<Game>,
        typing: Arc<Gate>,
        minds: HashMap<EntityId, Arc<dyn Mind>>,
        fallback: Arc<FallbackMind>,
        cfg: SupervisorConfig,
    ) -> Arc<Self> {
        let now = game.now_ms();
        let mut state = HashMap::new();
        for id in minds.keys() {
            let limit = cfg.budgets.get(id).copied().or(cfg.daily_token_budget);
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
        })
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

    /// Subscribes to the observer feed.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<ObserverEvent>> {
        self.observer.subscribe()
    }

    fn publish(&self, e: ObserverEvent) {
        let _ = self.observer.send(Arc::new(e));
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
    pub fn fire_macro_tick(&self, entity: &EntityId) -> RouteNote {
        let now = self.game.now_ms();
        // The tick is counted when its wake is delivered (`run_wake`), not
        // here: two ticks can merge into one wake.
        let note = self
            .game
            .read(|w| lock(&self.router).macro_tick(entity, w, now));
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
                lock(&self.router).forget(&id);
                self.publish(ObserverEvent::Session {
                    entity: id.clone(),
                    online: false,
                });
            }
            if want {
                let mut st = lock(&self.state);
                if let Some(s) = st.get_mut(&id) {
                    let was = s.afk_until;
                    let away = s
                        .afk
                        .update(&spec, self.cfg.seed, id.as_str(), now, in_town);
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
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "hours in ms fit"
            )]
            let (lo_ms, hi_ms) = (
                (lo * 3_600_000.0 * self.cfg.macro_tick_scale) as u64,
                (hi * 3_600_000.0 * self.cfg.macro_tick_scale) as u64,
            );
            let wait = match note {
                RouteNote::Queued { .. } => pick(h, lo_ms, hi_ms),
                // A human was there: try again in a few minutes.
                RouteNote::Dropped { .. } => pick(h, 120_000, 600_000),
            };
            if let Some(s) = lock(&self.state).get_mut(&id) {
                s.next_macro_ms = now + wait.max(1);
            }
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
        self.turing_sessions(now);
        self.macro_ticks(now);
        let wakes = lock(&self.router).due(now);
        wakes
            .into_iter()
            .map(|w| {
                let me = Arc::clone(self);
                tokio::spawn(async move { me.run_wake(w).await })
            })
            .collect()
    }

    fn context(&self, wake: &Wake) -> WakeContext {
        let macro_ticks = lock(&self.state)
            .get(&wake.entity)
            .map_or(0, |s| s.macro_ticks);
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
                wake_id: format!(
                    "wake-{}-{}-{}",
                    wake.entity,
                    wake.seqs.last().map_or(0, |s| s.0),
                    self.game.now_ms()
                ),
            }
        })
    }

    /// Runs one wake to the end and records it.
    pub async fn run_wake(self: Arc<Self>, wake: Wake) {
        let start = self.game.now_ms();
        let level = lock(&self.state)
            .get_mut(&wake.entity)
            .map_or(BudgetLevel::Normal, |s| s.budget.level(start));
        let online = self
            .game
            .read(|w| w.entity(&wake.entity).is_some_and(|e| e.online));
        let mind: Option<Arc<dyn Mind>> = if !online {
            None
        } else if level == BudgetLevel::Hard {
            Some(Arc::clone(&self.fallback) as Arc<dyn Mind>)
        } else {
            self.minds.get(&wake.entity).cloned()
        };
        let Some(mind) = mind else {
            lock(&self.router).finish(&wake.entity, start);
            return;
        };
        self.publish(ObserverEvent::WakeStarted {
            entity: wake.entity.clone(),
            priority: wake.priority,
            reason: wake.reason.clone(),
            mind: mind.kind().to_owned(),
            at_ms: start,
        });
        if wake.macro_tick
            && let Some(s) = lock(&self.state).get_mut(&wake.entity)
        {
            s.macro_ticks += 1;
        }
        let ctx = self.context(&wake);
        let tools = MudTools::new(
            Arc::clone(&self.game),
            Arc::clone(&self.gate),
            wake.entity.clone(),
        );
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
        let budget = {
            let mut st = lock(&self.state);
            st.get_mut(&wake.entity).map_or(BudgetLevel::Normal, |s| {
                s.budget.spend(
                    end,
                    reply.usage.tokens(),
                    reply.usage.cost_usd_micros,
                    reply.usage.calls,
                );
                s.last_wake = Some((wake.priority, wake.reason.clone(), start));
                s.wakes += 1;
                s.budget.level(end)
            })
        };
        lock(&self.router).finish(&wake.entity, end);
        self.publish(ObserverEvent::WakeFinished {
            entity: wake.entity,
            ok,
            note: reply.text,
            usage: reply.usage,
            budget,
            took_ms: end.saturating_sub(start),
        });
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
        let mut st = lock(&self.state);
        let entities: Vec<Value> = seen
            .into_iter()
            .filter_map(|(id, mut v)| {
                let s = st.get_mut(id)?;
                let level = s.budget.level(now);
                let extra = json!({
                    "budget": {
                        "level": level,
                        "tokens": s.budget.tokens,
                        "limit": s.budget.limit,
                        "cost_usd_micros": s.budget.cost_usd_micros,
                        "calls": s.budget.calls,
                    },
                    "wakes": s.wakes,
                    "last_wake": s.last_wake.as_ref().map(|(p, r, at)| json!({ "priority": p, "reason": r, "at_ms": at })),
                    "pending": pending.get(id).map(|(p, d)| json!({ "priority": p, "due_ms": d })),
                    "macro_ticks": s.macro_ticks,
                    "next_macro_ms": s.next_macro_ms,
                    "afk_until_ms": s.afk_until,
                    "typing": typing.get(id).copied().unwrap_or(false),
                });
                if let (Some(obj), Value::Object(more)) = (v.as_object_mut(), extra) {
                    obj.extend(more);
                }
                Some(v)
            })
            .collect();
        let tokens: u64 = st.values().map(|s| s.budget.tokens).sum();
        let cost: u64 = st.values().map(|s| s.budget.cost_usd_micros).sum();
        drop(st);
        json!({ "now_ms": now, "entities": entities, "tokens_today": tokens, "cost_usd_micros_today": cost })
    }

    /// Records spend from outside a wake (tests, external minds).
    pub fn record_spend(&self, entity: &EntityId, usage: Usage) {
        let now = self.game.now_ms();
        if let Some(s) = lock(&self.state).get_mut(entity) {
            s.budget
                .spend(now, usage.tokens(), usage.cost_usd_micros, usage.calls);
        }
    }
}

#[cfg(test)]
mod tests;
