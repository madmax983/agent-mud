//! [`Game`]: a world shared by many sessions.
//!
//! The world itself is single-threaded and synchronous; a command takes
//! microseconds. `Game` serializes commands behind a mutex, persists each
//! one, and broadcasts its events to every subscriber: web terminals, the
//! attention router, the observer dashboard.
//!
//! # Side lanes (ADR 0011)
//!
//! The supervisor keeps its own durable rows (bookings, macro counters,
//! plans, spend) in the same file, through four lanes:
//!
//! - **Turns.** [`Game::open_turn`] records a wake's turn with the ops that
//!   consume its obligations; the mind adds plan ops with [`Game::stage`].
//!   The staged ops commit in the transaction of the mind's first
//!   **effectful** command ([`Game::execute`] by that entity that parsed,
//!   is not read-only and worked), or at [`Game::close_turn`]. A look, a
//!   typo, a refusal, a login, a tick or a session change drains nothing.
//! - **Acts.** [`Game::act`] writes ops at once with `synchronous = FULL`,
//!   for a promise made outside the process (a booking's "ok").
//! - **Persist.** [`Game::persist`] writes ops whose loss is a safe
//!   regression, or defers them behind a failed save.
//! - **Retention.** [`Game::retain`] archives, then trims, old events.
//!
//! Lock order: the world lock, then the store cell (a leaf). `open_turn`,
//! `stage`, `persist` and `retain` take only the cell; `act` and
//! `close_turn` take the world first, so they are never called while a
//! supervisor lock is held.

mod cell;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;
use tokio::sync::broadcast;

pub use cell::{Durability, Retention, SideError};

use cell::{StoreCell, Turn};

use crate::durability::{events_truncated, retention_cutoff};
use crate::engine::{Audit, CommandResult, EVENT_LOG_CAP, LoginError, World};
use crate::event::Event;
use crate::hash::epoch;
use crate::ids::{EntityId, Seq};
use crate::pack::WorldPack;
use crate::side::{PlanRow, SideOp, SideState};
use crate::state::NameOwner;
use crate::store::{ScriptMemory, Store, StoreError};

/// How many events the store reloads into memory at start: the whole
/// in-memory log, so `mud_events` reads the same after a restart.
pub const RELOAD_EVENTS: usize = EVENT_LOG_CAP;

/// A source of time, in ms since the Unix epoch.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Now.
    fn now_ms(&self) -> u64;
}

/// The wall clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }
}

/// A clock that moves only when told to, for tests.
#[derive(Debug, Default)]
pub struct ManualClock(AtomicU64);

impl ManualClock {
    /// A clock at `ms`.
    #[must_use]
    pub const fn new(ms: u64) -> Self {
        Self(AtomicU64::new(ms))
    }

    /// Moves it forward.
    pub fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }

    /// Sets it.
    pub fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// A failure opening a game.
#[derive(Debug, Error)]
pub enum GameError {
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The database belongs to another pack.
    #[error("the database holds world {found:?}, not {wanted:?}")]
    WrongPack {
        /// In the database.
        found: String,
        /// Loaded.
        wanted: String,
    },
}

/// What one [`Game::retain`] run did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Retained {
    /// Rows trimmed from `events`.
    pub trimmed: usize,
    /// Lines written to the archive.
    pub archived: usize,
    /// Batches (transactions) run.
    pub batches: usize,
}

/// A shared, persistent, broadcasting world.
#[derive(Debug)]
pub struct Game {
    world: Mutex<World>,
    store: Option<Mutex<StoreCell>>,
    events: broadcast::Sender<Arc<Event>>,
    audit: broadcast::Sender<Arc<Audit>>,
    clock: Arc<dyn Clock>,
    store_errors: AtomicU64,
    memory: Mutex<MemoryCache>,
    /// The epoch and boot of this process's wake ids.
    ids: (u64, u64),
    /// The side rows loaded at open, until the supervisor takes them.
    restored: Mutex<Option<SideState>>,
    /// When retention runs next, by the game clock.
    next_retain_ms: AtomicU64,
    /// The fault-drill probe ([`Game::set_commit_probe`]).
    probe: ProbeSlot,
}

/// The probe, behind its own lock (a leaf, taken under the world lock).
#[derive(Default)]
struct ProbeSlot(Mutex<Option<CommitProbe>>);

impl std::fmt::Debug for ProbeSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let set = lock(&self.0).is_some();
        f.debug_tuple("ProbeSlot").field(&set).finish()
    }
}

/// A fault-drill probe: called with the actor of each effectful command
/// once it is committed ([`Game::set_commit_probe`]).
pub type CommitProbe = Arc<dyn Fn(&EntityId) + Send + Sync>;

/// Mind script memory, cached, with a reset count per mind.
#[derive(Debug, Default)]
struct MemoryCache {
    values: ScriptMemory,
    /// Bumped by every reset, so a wake that read the memory before a
    /// reset cannot save over it afterwards.
    resets: BTreeMap<EntityId, u64>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Game {
    /// Wraps a world. With a store, its epoch and boot name the wake ids;
    /// without one, the epoch is minted for this process and the boot is 0.
    #[must_use]
    pub fn new(world: World, store: Option<Store>, clock: Arc<dyn Clock>) -> Arc<Self> {
        let restored = store.as_ref().and_then(|s| s.load_side().ok());
        Self::build(
            world,
            store,
            clock,
            ScriptMemory::new(),
            restored,
            Retention::default(),
        )
    }

    fn build(
        world: World,
        store: Option<Store>,
        clock: Arc<dyn Clock>,
        memory: ScriptMemory,
        restored: Option<SideState>,
        retention: Retention,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(4_096);
        let (audit, _) = broadcast::channel(4_096);
        let ids = store.as_ref().map_or_else(
            || (epoch(world.state().dice_state, clock.now_ms()), 0),
            |s| (s.epoch(), s.boot()),
        );
        Arc::new(Self {
            world: Mutex::new(world),
            store: store.map(|s| Mutex::new(StoreCell::new(s, retention.clamped()))),
            events,
            audit,
            clock,
            store_errors: AtomicU64::new(0),
            memory: Mutex::new(MemoryCache {
                values: memory,
                resets: BTreeMap::new(),
            }),
            ids,
            restored: Mutex::new(restored),
            next_retain_ms: AtomicU64::new(0),
            probe: ProbeSlot::default(),
        })
    }

    /// Opens the world in `db`, or starts a fresh one from `pack`. The
    /// store is upgraded and its boot bumped; side rows of entities the
    /// pack no longer has are dropped. The events archive goes in
    /// `archive/` beside `db` (see [`Game::set_retention`]).
    ///
    /// # Errors
    /// The store fails, is newer than this code, or holds another pack.
    pub fn open(
        pack: Arc<WorldPack>,
        db: Option<&Path>,
        seed: u64,
        clock: Arc<dyn Clock>,
    ) -> Result<Arc<Self>, GameError> {
        let Some(path) = db else {
            return Ok(Self::new(World::new(pack, seed), None, clock));
        };
        let now = clock.now_ms();
        let mut store = Store::open(path, seed, now)?;
        let (world, mut side) = match store.load(RELOAD_EVENTS)? {
            Some(loaded) if loaded.pack != pack.world.name => {
                return Err(GameError::WrongPack {
                    found: loaded.pack,
                    wanted: pack.world.name.clone(),
                });
            }
            Some(loaded) => (
                World::from_state(pack, loaded.state, loaded.recent),
                loaded.side,
            ),
            None => {
                let w = World::new(pack, seed);
                store.save(&w.pack().world.name, &w.snapshot(), &[], &[], now)?;
                (w, store.load_side()?)
            }
        };
        // Rows of entities the pack dropped go now; a failure here is
        // redone at the next open.
        let dropped = side.retain(|e| world.entity(e).is_some());
        if !dropped.is_empty() {
            store.apply_side(&dropped, now, crate::store::Fsync::Normal)?;
        }
        let memory = store.load_script_memory()?;
        let retention = Retention {
            archive_dir: path
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .join("archive"),
            ..Retention::default()
        };
        Ok(Self::build(
            world,
            Some(store),
            clock,
            memory,
            Some(side),
            retention,
        ))
    }

    /// Replaces the retention policy (bounds applied, see
    /// [`Retention::clamped`]). No-op without a store.
    pub fn set_retention(&self, retention: Retention) {
        if let Some(cell) = &self.store {
            lock(cell).retention = retention.clamped();
        }
    }

    /// The retention policy, `None` without a store.
    #[must_use]
    pub fn retention(&self) -> Option<Retention> {
        self.store.as_ref().map(|c| lock(c).retention.clone())
    }

    /// The epoch of this world's wake ids: the store's, minted once per
    /// `world.db`, or this process's for an in-memory world.
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.ids.0
    }

    /// The boot number of this process: one more than the last open of
    /// the store; 0 for an in-memory world.
    #[must_use]
    pub const fn boot(&self) -> u64 {
        self.ids.1
    }

    /// The side rows loaded at open, handed over once (to the
    /// supervisor). `None` the second time, or for an in-memory world.
    pub fn take_restored(&self) -> Option<SideState> {
        lock(&self.restored).take()
    }

    /// A mind's script memory under `schema`: `{}` when it has none, or
    /// when what it has was saved under another schema.
    #[must_use]
    pub fn script_memory(&self, entity: &EntityId, schema: u32) -> serde_json::Value {
        self.script_memory_at(entity, schema).1
    }

    /// A mind's script memory, as [`Game::script_memory`], with the count
    /// of its resets so far, for [`Game::save_script_memory_if`].
    #[must_use]
    pub fn script_memory_at(&self, entity: &EntityId, schema: u32) -> (u64, serde_json::Value) {
        let m = lock(&self.memory);
        let value = m
            .values
            .get(entity)
            .filter(|(s, _)| *s == schema)
            .map_or_else(|| serde_json::json!({}), |(_, v)| v.clone());
        (m.resets.get(entity).copied().unwrap_or(0), value)
    }

    fn store_memory(&self, entity: &EntityId, schema: u32, body: &serde_json::Value) {
        if let Some(cell) = &self.store
            && let Err(e) = lock(cell)
                .store
                .save_script_memory(entity, schema, body, self.now_ms())
        {
            self.store_errors.fetch_add(1, Ordering::Relaxed);
            tracing::error!(error = %e, "saving script memory failed");
        }
    }

    /// Saves a mind's script memory, in memory and in the store. Mind
    /// memory is not world state: it never touches physics.
    pub fn save_script_memory(&self, entity: &EntityId, schema: u32, body: serde_json::Value) {
        let mut m = lock(&self.memory);
        self.store_memory(entity, schema, &body);
        m.values.insert(entity.clone(), (schema, body));
    }

    /// Saves a mind's script memory only if it was not reset since it was
    /// read with `resets` (from [`Game::script_memory_at`]). Returns
    /// whether it saved.
    pub fn save_script_memory_if(
        &self,
        entity: &EntityId,
        schema: u32,
        resets: u64,
        body: serde_json::Value,
    ) -> bool {
        let mut m = lock(&self.memory);
        if m.resets.get(entity).copied().unwrap_or(0) != resets {
            return false;
        }
        self.store_memory(entity, schema, &body);
        m.values.insert(entity.clone(), (schema, body));
        true
    }

    /// Saves a mind's script memory, as [`Game::save_script_memory_if`]
    /// does, on `entity`'s open turn of `wake_id`: the store's row changes
    /// with the turn's first effectful command, or at its close, so a
    /// crash before either leaves the memory the wake started from, and
    /// the obligations that come back decide again from it. With no such
    /// turn (a mind run outside a wake) the store is written now. Returns
    /// whether it saved.
    pub fn stage_script_memory_if(
        &self,
        entity: &EntityId,
        wake_id: &str,
        schema: u32,
        resets: u64,
        body: serde_json::Value,
    ) -> bool {
        let mut m = lock(&self.memory);
        if m.resets.get(entity).copied().unwrap_or(0) != resets {
            return false;
        }
        let op = SideOp::ScriptMemory {
            entity: entity.clone(),
            schema,
            body: body.clone(),
        };
        if self.stage(entity, wake_id, vec![op]).is_err() {
            self.store_memory(entity, schema, &body);
        }
        m.values.insert(entity.clone(), (schema, body));
        true
    }

    /// Forgets a mind's script memory, and records who asked in the audit.
    /// A save still staged on a turn, or waiting in the backlog, is dropped
    /// with it: the reset wins.
    pub fn reset_script_memory(&self, entity: &EntityId) {
        {
            // The memory lock covers the store too, so no save interleaves.
            let mut m = lock(&self.memory);
            if let Some(cell) = &self.store {
                let mut c = lock(cell);
                let other = |op: &SideOp| !matches!(op, SideOp::ScriptMemory { entity: e, .. } if e == entity);
                if let Some(t) = c.turns.get_mut(entity) {
                    t.staged.retain(other);
                }
                c.pending.ops.retain(other);
                if let Err(e) = c.store.clear_script_memory(entity) {
                    self.store_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(error = %e, "clearing script memory failed");
                }
            }
            m.values.remove(entity);
            let next = m.resets.get(entity).copied().unwrap_or(0).saturating_add(1);
            m.resets.insert(entity.clone(), next);
        }
        self.publish_audit(Audit::MindMemoryReset {
            entity: entity.clone(),
            at_ms: self.now_ms(),
        });
    }

    /// Now, by the game clock.
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// The clock.
    #[must_use]
    pub fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.clock)
    }

    /// Subscribes to every event, as emitted. Filter with
    /// [`Event::reaches`].
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.events.subscribe()
    }

    /// Subscribes to the observer audit.
    #[must_use]
    pub fn subscribe_audit(&self) -> broadcast::Receiver<Arc<Audit>> {
        self.audit.subscribe()
    }

    /// Publishes an observer entry from outside the world, such as a wake.
    pub fn publish_audit(&self, entry: Audit) {
        let _ = self.audit.send(Arc::new(entry));
    }

    /// How many saves failed.
    #[must_use]
    pub fn store_errors(&self) -> u64 {
        self.store_errors.load(Ordering::Relaxed)
    }

    /// Makes every store write fail from now on, or lets writes through
    /// again, as a full disk would: for fault drills and tests of the
    /// retry path. No-op without a store.
    ///
    /// # Errors
    /// SQLite fails.
    pub fn refuse_store_writes(&self, on: bool) -> Result<(), StoreError> {
        self.store
            .as_ref()
            .map_or(Ok(()), |cell| lock(cell).store.refuse_writes(on))
    }

    /// Reads the world.
    pub fn read<R>(&self, f: impl FnOnce(&World) -> R) -> R {
        f(&lock(&self.world))
    }

    /// Changes the world through `f`, then persists and broadcasts. No
    /// turn is drained: this is not a command of an entity's mind.
    pub fn write<R>(&self, f: impl FnOnce(&mut World, u64) -> R) -> R {
        let now = self.now_ms();
        let mut w = lock(&self.world);
        let out = f(&mut w, now);
        self.commit(&mut w, now, None);
        drop(w);
        out
    }

    fn commit(&self, w: &mut World, now: u64, drain: Option<&EntityId>) {
        let fresh = w.drain_fresh();
        let audit = w.drain_audit();
        // The ledger's rows go to the store with their events; the
        // snapshot keeps only the totals. Without a store they are told
        // in the events and the audit, and not kept.
        let mints = w.drain_mints();
        let mut backlog = 0;
        if let Some(cell) = &self.store {
            let snapshot = w.snapshot();
            let mut c = lock(cell);
            let ops = drain
                .and_then(|a| c.turns.get_mut(a))
                .map(|t| std::mem::take(&mut t.staged))
                .unwrap_or_default();
            let saved = c.commit_world((&w.pack().world.name, &snapshot), &fresh, mints, ops, now);
            drop(c);
            if let Err(failed) = saved {
                self.store_errors.fetch_add(1, Ordering::Relaxed);
                tracing::error!(error = %failed.error, "saving the world failed; kept for the next save");
                backlog = failed.dropped;
            }
        }
        for e in fresh {
            let _ = self.events.send(Arc::new(e));
        }
        for a in audit {
            let _ = self.audit.send(Arc::new(a));
        }
        if backlog > 0 {
            tracing::error!(
                dropped = backlog,
                "the store backlog is full: its oldest events were dropped"
            );
            self.publish_audit(Audit::StoreBacklog {
                dropped: u64::try_from(backlog).unwrap_or(u64::MAX),
                at_ms: now,
            });
        }
    }

    /// Runs one line for `actor`. When the line parsed, is not read-only
    /// and worked, it is effectful: the ops `actor`'s open turn staged
    /// commit in its transaction.
    pub fn execute(&self, actor: &EntityId, line: &str) -> CommandResult {
        let now = self.now_ms();
        let mut w = lock(&self.world);
        let r = w.execute(actor, line, now);
        let effectful = r.ok && !r.read_only;
        self.commit(&mut w, now, effectful.then_some(actor));
        if effectful {
            let probe = lock(&self.probe.0).clone();
            if let Some(p) = probe {
                p(actor);
            }
        }
        drop(w);
        r
    }

    /// Opens `entity`'s turn for `wake_id`, staging `ops` (the wake's
    /// consume). A turn still open for the entity is replaced: its staged
    /// ops are dropped, so its obligations stay unconsumed. No-op for an
    /// in-memory world.
    pub fn open_turn(&self, entity: &EntityId, wake_id: &str, ops: Vec<SideOp>) {
        if let Some(cell) = &self.store {
            lock(cell).turns.insert(
                entity.clone(),
                Turn {
                    wake_id: wake_id.to_owned(),
                    staged: ops,
                },
            );
        }
    }

    /// The wake id of `entity`'s open turn.
    #[must_use]
    pub fn open_turn_of(&self, entity: &EntityId) -> Option<String> {
        let cell = self.store.as_ref()?;
        lock(cell).turns.get(entity).map(|t| t.wake_id.clone())
    }

    /// Stages more ops on `entity`'s open turn of `wake_id`.
    ///
    /// # Errors
    /// [`SideError::StaleTurn`] when the entity has no open turn of that
    /// wake; nothing is staged then.
    pub fn stage(
        &self,
        entity: &EntityId,
        wake_id: &str,
        ops: Vec<SideOp>,
    ) -> Result<(), SideError> {
        let Some(cell) = &self.store else {
            return Ok(());
        };
        match lock(cell).turns.get_mut(entity) {
            Some(t) if t.wake_id == wake_id => {
                t.staged.extend(ops);
                Ok(())
            }
            _ => Err(SideError::StaleTurn {
                entity: entity.clone(),
                wake_id: wake_id.to_owned(),
            }),
        }
    }

    fn act_locked(&self, w: &World, c: &mut StoreCell, ops: &[SideOp]) -> Result<(), SideError> {
        let now = self.now_ms();
        c.act(&w.pack().world.name, || w.snapshot(), ops, now)
            .map_err(|e| {
                self.store_errors.fetch_add(1, Ordering::Relaxed);
                tracing::error!(error = %e, "a durable side write failed");
                SideError::Store(e)
            })
    }

    /// Writes `ops` now, durably (`synchronous = FULL`), with any backlog a
    /// failed save left. Call it only with no supervisor lock held: it
    /// takes the world lock.
    ///
    /// # Errors
    /// The store failed: nothing of `ops` was written, and never will be.
    pub fn act(&self, ops: &[SideOp]) -> Result<(), SideError> {
        let w = lock(&self.world);
        let Some(cell) = &self.store else {
            return Ok(());
        };
        let mut c = lock(cell);
        self.act_locked(&w, &mut c, ops)
    }

    /// Closes `entity`'s turn of `wake_id`: writes what it still has
    /// staged (a mind that never acted consumes here) and `ops`, as
    /// [`Game::act`] does, and forgets the turn.
    ///
    /// Unlike an act, a close makes no promise to anyone, so a failed one
    /// loses nothing: its ops wait in the backlog and go with the next
    /// commit that works, as a failed world save's do. The supervisor has
    /// already moved on from the obligations they consume; the store
    /// follows it then (or, after a crash first, the obligations come
    /// back at boot).
    ///
    /// # Errors
    /// [`SideError::StaleTurn`] when the open turn is another wake's (it
    /// is left as it is, and nothing is written); [`SideError::Store`]
    /// when the store failed (the turn is gone, and its ops wait in the
    /// backlog).
    pub fn close_turn(
        &self,
        entity: &EntityId,
        wake_id: &str,
        ops: Vec<SideOp>,
    ) -> Result<(), SideError> {
        let w = lock(&self.world);
        let Some(cell) = &self.store else {
            return Ok(());
        };
        let mut c = lock(cell);
        let mut all = match c.turns.get(entity) {
            Some(t) if t.wake_id == wake_id => {
                c.turns.remove(entity).map(|t| t.staged).unwrap_or_default()
            }
            _ => {
                return Err(SideError::StaleTurn {
                    entity: entity.clone(),
                    wake_id: wake_id.to_owned(),
                });
            }
        };
        all.extend(ops);
        let out = self.act_locked(&w, &mut c, &all);
        if out.is_err() {
            c.pending.ops.extend(all);
        }
        drop(c);
        drop(w);
        out
    }

    /// Writes `ops` whose loss is a safe regression (a macro schedule, a
    /// late spend, a boot-time cleanup): now when nothing waits ahead of
    /// them, else behind the backlog. Takes only the store cell.
    pub fn persist(&self, ops: Vec<SideOp>) -> Durability {
        let Some(cell) = &self.store else {
            return Durability::Durable;
        };
        let (d, err) = lock(cell).persist(ops, self.now_ms());
        if let Some(e) = err {
            self.store_errors.fetch_add(1, Ordering::Relaxed);
            tracing::error!(error = %e, "a side write failed; kept for the next save");
        }
        d
    }

    /// The stored plan of `entity`: `None` without one, for an in-memory
    /// world, or when the store fails (the failure is counted and logged).
    #[must_use]
    pub fn plan_of(&self, entity: &EntityId) -> Option<PlanRow> {
        let cell = self.store.as_ref()?;
        let plan = lock(cell).store.plan(entity);
        match plan {
            Ok(p) => p,
            Err(e) => {
                self.store_errors.fetch_add(1, Ordering::Relaxed);
                tracing::error!(error = %e, "reading a plan failed");
                None
            }
        }
    }

    /// Sets a fault-drill probe, called with the actor after every
    /// effectful command has been committed, under the world lock, before
    /// the next command can run. A probe that never returns freezes the
    /// world right after that commit: the kill-and-restart drills use it to
    /// stop a process at an exact point. `None` removes it.
    pub fn set_commit_probe(&self, probe: Option<CommitProbe>) {
        *lock(&self.probe.0) = probe;
    }

    /// Whether retention is due at `now`; claims the run when it is. False
    /// for an in-memory world.
    pub fn retention_due(&self, now: u64) -> bool {
        let Some(cell) = &self.store else {
            return false;
        };
        let every = lock(cell).retention.every_ms;
        let next = self.next_retain_ms.load(Ordering::Relaxed);
        now >= next
            && self
                .next_retain_ms
                .compare_exchange(
                    next,
                    now.saturating_add(every),
                    Ordering::SeqCst,
                    Ordering::Relaxed,
                )
                .is_ok()
    }

    /// Archives, then trims, the events older than the retention window,
    /// in batches of one transaction each under the store cell only. The
    /// newest `keep_min` rows are never touched. Publishes an
    /// [`Audit::EventsArchived`] when it trimmed anything.
    ///
    /// # Errors
    /// The store or the archive failed; what earlier batches did stands.
    pub fn retain(&self, now: u64) -> Result<Retained, StoreError> {
        let Some(cell) = &self.store else {
            return Ok(Retained::default());
        };
        let mut done = Retained::default();
        let mut through = None;
        loop {
            let batch = {
                let mut c = lock(cell);
                let r = c.retention.clone();
                let cutoff =
                    retention_cutoff(now, r.keep_days.min(crate::durability::MAX_KEEP_DAYS));
                c.store
                    .trim_events(cutoff, r.keep_min, r.batch, &r.archive_dir)
                    .map(|t| (t, r.batch))
            };
            let (t, size) = match batch {
                Ok(b) => b,
                Err(e) => {
                    self.store_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(error = %e, "events retention failed; nothing more was trimmed");
                    return Err(e);
                }
            };
            if t.rows > 0 {
                done.batches += 1;
                done.trimmed += t.rows;
                done.archived += t.archived;
                through = t.through.or(through);
            }
            if t.rows < usize::try_from(size).unwrap_or(usize::MAX) {
                break;
            }
        }
        if let Some(seq) = through {
            self.publish_audit(Audit::EventsArchived {
                trimmed: u64::try_from(done.trimmed).unwrap_or(u64::MAX),
                archived: u64::try_from(done.archived).unwrap_or(u64::MAX),
                through: Seq(seq),
                at_ms: now,
            });
        }
        Ok(done)
    }

    /// Who holds the login name `name`. Server-internal: the login
    /// dialogue reads it; no view, event or MCP result carries it.
    ///
    /// # Errors
    /// The name is invalid or reserved.
    pub fn name_owner(&self, name: &str) -> Result<NameOwner, LoginError> {
        self.read(|w| w.name_owner(name))
    }

    /// Every human character's id.
    #[must_use]
    pub fn human_ids(&self) -> Vec<EntityId> {
        self.read(|w| w.state().human_ids())
    }

    /// Logs a human in.
    ///
    /// # Errors
    /// The name is invalid or taken.
    pub fn login(&self, name: &str) -> Result<EntityId, LoginError> {
        self.write(|w, now| w.login(name, now))
    }

    /// Brings an entity online.
    pub fn bring_online(&self, id: &EntityId) -> Option<Seq> {
        self.write(|w, now| w.bring_online(id, now))
    }

    /// Takes an entity offline.
    pub fn take_offline(&self, id: &EntityId) -> Option<Seq> {
        self.write(|w, now| w.take_offline(id, now))
    }

    /// Advances world time.
    pub fn tick(&self) -> Vec<Seq> {
        self.write(World::tick)
    }

    /// `mud_look` for `viewer`.
    #[must_use]
    pub fn look(&self, viewer: &EntityId) -> serde_json::Value {
        self.read(|w| w.look_view(viewer))
    }

    /// `mud_sheet` for `viewer`.
    #[must_use]
    pub fn sheet(&self, viewer: &EntityId) -> serde_json::Value {
        self.read(|w| w.sheet_view(viewer))
    }

    /// `mud_events` for `viewer`: the events after `since`, as views, the
    /// cursor to pass next time, and `truncated`: whether some event after
    /// `since` is no longer in the in-memory log (it was older than the
    /// newest [`EVENT_LOG_CAP`]).
    #[must_use]
    pub fn events(&self, viewer: &EntityId, since: Seq, limit: usize) -> serde_json::Value {
        self.read(|w| {
            let evs = w.events_since(viewer, since, limit);
            let cursor = evs.last().map_or(since, |e| e.seq);
            let room = w.entity(viewer).map(|e| e.room.clone());
            let truncated =
                events_truncated(since.0, w.oldest_logged().map(|s| s.0), w.last_seq().0);
            serde_json::json!({
                "cursor": cursor,
                "room_id": room,
                "events": evs.iter().map(|e| e.view_for(viewer)).collect::<Vec<_>>(),
                "truncated": truncated,
            })
        })
    }
}

#[cfg(test)]
mod side_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn pack() -> Arc<WorldPack> {
        Arc::new(WorldPack::crypt_v01().unwrap())
    }

    #[tokio::test]
    async fn commands_broadcast_and_persist_and_resume() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("world.db");
        let clock = Arc::new(ManualClock::new(1_000));
        let game = Game::open(pack(), Some(&db), 7, clock.clone()).unwrap();
        let mut rx = game.subscribe();
        let ada = game.login("ada").unwrap();
        let r = game.execute(&ada, "say hello there");
        assert!(r.ok, "{r:?}");
        let mut kinds = Vec::new();
        while let Ok(e) = rx.try_recv() {
            kinds.push(e.kind);
        }
        assert_eq!(
            kinds,
            vec![crate::EventKind::Enter, crate::EventKind::Speech]
        );
        let view = game.events(&EntityId::from("marta"), Seq(0), 10);
        assert_eq!(
            view["events"].as_array().map(Vec::len),
            Some(0),
            "marta is not in the square"
        );
        let mine = game.events(&ada, Seq(0), 10);
        assert_eq!(mine["events"][1]["untrusted_text"], "hello there");
        let dice_before = game.read(World::snapshot).dice_state;
        drop(game);

        let again = Game::open(pack(), Some(&db), 999, clock).unwrap();
        let ada_again = again.read(|w| w.entity(&ada).cloned()).unwrap();
        assert_eq!(ada_again.name, "ada");
        assert_eq!(again.read(World::snapshot).dice_state, dice_before);
        let mine = again.events(&ada, Seq(0), 10);
        assert_eq!(mine["events"].as_array().map(Vec::len), Some(2));
        assert_eq!(again.store_errors(), 0);
    }

    #[test]
    fn a_crash_leaves_no_ghost_humans() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("world.db");
        let clock = Arc::new(ManualClock::new(1_000));
        let game = Game::open(pack(), Some(&db), 7, clock.clone()).unwrap();
        let ada = game.login("ada").unwrap();
        let square = crate::ids::RoomId::from("town_square");
        assert!(game.read(|w| w.human_present(&square)));
        // A crash: the game goes away without the session logging ada out.
        drop(game);

        let again = Game::open(pack(), Some(&db), 7, clock).unwrap();
        assert!(!again.read(|w| w.entity(&ada).unwrap().online));
        assert!(!again.read(|w| w.human_present(&square)));
        let who = again.read(|w| w.who().0);
        assert!(!who.contains("ada"), "{who}");
        // She comes back when she logs in again.
        assert_eq!(again.login("ada").unwrap(), ada);
        assert!(again.read(|w| w.human_present(&square)));
    }

    #[test]
    fn script_memory_round_trips_with_its_schema() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("world.db");
        let clock = Arc::new(ManualClock::new(1_000));
        let hale = EntityId::from("guard_01");
        let game = Game::open(pack(), Some(&db), 7, clock.clone()).unwrap();
        assert_eq!(game.script_memory(&hale, 1), serde_json::json!({}));
        game.save_script_memory(&hale, 1, serde_json::json!({ "day": 3 }));
        assert_eq!(game.script_memory(&hale, 1)["day"], 3);
        drop(game);
        let again = Game::open(pack(), Some(&db), 7, clock.clone()).unwrap();
        assert_eq!(
            again.script_memory(&hale, 1)["day"],
            3,
            "it survives a restart"
        );
        assert_eq!(
            again.script_memory(&hale, 2),
            serde_json::json!({}),
            "a new schema starts fresh"
        );
        let mut audit = again.subscribe_audit();
        again.reset_script_memory(&hale);
        assert!(matches!(
            &*audit.try_recv().unwrap(),
            Audit::MindMemoryReset { entity, .. } if *entity == hale
        ));
        drop(again);
        let third = Game::open(pack(), Some(&db), 7, clock).unwrap();
        assert_eq!(third.script_memory(&hale, 1), serde_json::json!({}));
        assert_eq!(third.store_errors(), 0);
    }

    #[test]
    fn a_reset_wins_over_a_wake_that_read_the_memory_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("world.db");
        let clock = Arc::new(ManualClock::new(1_000));
        let hale = EntityId::from("guard_01");
        let game = Game::open(pack(), Some(&db), 7, clock.clone()).unwrap();
        game.save_script_memory(&hale, 1, serde_json::json!({ "n": 1 }));
        // A wake reads the memory, and runs its script...
        let (generation, before) = game.script_memory_at(&hale, 1);
        assert_eq!(before["n"], 1);
        // ...while the operator resets it.
        game.reset_script_memory(&hale);
        // The wake's save, built from the old memory, does not undo it.
        assert!(!game.save_script_memory_if(&hale, 1, generation, serde_json::json!({ "n": 2 })));
        assert_eq!(game.script_memory(&hale, 1), serde_json::json!({}));
        // A wake that read after the reset saves as usual.
        let (generation, now) = game.script_memory_at(&hale, 1);
        assert_eq!(now, serde_json::json!({}));
        assert!(game.save_script_memory_if(&hale, 1, generation, serde_json::json!({ "n": 9 })));
        assert_eq!(game.script_memory(&hale, 1)["n"], 9);
        drop(game);
        let again = Game::open(pack(), Some(&db), 7, clock).unwrap();
        assert_eq!(again.script_memory(&hale, 1)["n"], 9, "the store agrees");
    }

    #[test]
    fn a_database_of_another_pack_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("w.db");
        let mut s = Store::open(&db, 1, 0).unwrap();
        let state = crate::state::WorldState::from_pack(&pack(), 1);
        s.save("other-world", &state, &[], &[], 0).unwrap();
        drop(s);
        let err = Game::open(pack(), Some(&db), 1, Arc::new(SystemClock)).unwrap_err();
        assert!(err.to_string().contains("other-world"));
    }

    #[test]
    fn the_store_pages_events() {
        let mut s = Store::open_in_memory(1, 0).unwrap();
        let mut w = World::new(pack(), 1);
        let ada = w.login("ada", 5).unwrap();
        w.execute(&ada, "say one", 6);
        w.execute(&ada, "say two", 7);
        let fresh = w.drain_fresh();
        s.save("crypt-v01", &w.snapshot(), &fresh, &[], 8).unwrap();
        assert_eq!(s.event_count().unwrap(), 3);
        let after = s.events_after(1, 10).unwrap();
        assert_eq!(after.len(), 2);
        assert_eq!(after[1].untrusted_text.as_deref(), Some("two"));
        assert!(SystemClock.now_ms() > 1_600_000_000_000);
    }
}
