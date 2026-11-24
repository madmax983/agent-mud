//! [`Game`]: a world shared by many sessions.
//!
//! The world itself is single-threaded and synchronous; a command takes
//! microseconds. `Game` serializes commands behind a mutex, persists each
//! one, and broadcasts its events to every subscriber: web terminals, the
//! attention router, the observer dashboard.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;
use tokio::sync::broadcast;

use crate::engine::{Audit, CommandResult, LoginError, World};
use crate::event::Event;
use crate::ids::{EntityId, Seq};
use crate::pack::WorldPack;
use crate::state::NameOwner;
use crate::store::{ScriptMemory, Store, StoreError};

/// How many events the store reloads into memory at start.
const RELOAD_EVENTS: usize = 2_000;

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

/// A shared, persistent, broadcasting world.
#[derive(Debug)]
pub struct Game {
    world: Mutex<World>,
    store: Option<Mutex<Store>>,
    events: broadcast::Sender<Arc<Event>>,
    audit: broadcast::Sender<Arc<Audit>>,
    clock: Arc<dyn Clock>,
    store_errors: AtomicU64,
    memory: Mutex<MemoryCache>,
}

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
    /// Wraps a world.
    #[must_use]
    pub fn new(world: World, store: Option<Store>, clock: Arc<dyn Clock>) -> Arc<Self> {
        Self::with_memory(world, store, clock, ScriptMemory::new())
    }

    fn with_memory(
        world: World,
        store: Option<Store>,
        clock: Arc<dyn Clock>,
        memory: ScriptMemory,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(4_096);
        let (audit, _) = broadcast::channel(4_096);
        Arc::new(Self {
            world: Mutex::new(world),
            store: store.map(Mutex::new),
            events,
            audit,
            clock,
            store_errors: AtomicU64::new(0),
            memory: Mutex::new(MemoryCache {
                values: memory,
                resets: BTreeMap::new(),
            }),
        })
    }

    /// Opens the world in `db`, or starts a fresh one from `pack`.
    ///
    /// # Errors
    /// The store fails, or it holds another pack.
    pub fn open(
        pack: Arc<WorldPack>,
        db: Option<&Path>,
        seed: u64,
        clock: Arc<dyn Clock>,
    ) -> Result<Arc<Self>, GameError> {
        let Some(path) = db else {
            return Ok(Self::new(World::new(pack, seed), None, clock));
        };
        let mut store = Store::open(path)?;
        let world = match store.load(RELOAD_EVENTS)? {
            Some(loaded) if loaded.pack != pack.world.name => {
                return Err(GameError::WrongPack {
                    found: loaded.pack,
                    wanted: pack.world.name.clone(),
                });
            }
            Some(loaded) => World::from_state(pack, loaded.state, loaded.recent),
            None => {
                let w = World::new(pack, seed);
                store.save(&w.pack().world.name, &w.snapshot(), &[], clock.now_ms())?;
                w
            }
        };
        let memory = store.load_script_memory()?;
        Ok(Self::with_memory(world, Some(store), clock, memory))
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
        if let Some(store) = &self.store
            && let Err(e) = lock(store).save_script_memory(entity, schema, body, self.now_ms())
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

    /// Forgets a mind's script memory, and records who asked in the audit.
    pub fn reset_script_memory(&self, entity: &EntityId) {
        {
            // The memory lock covers the store too, so no save interleaves.
            let mut m = lock(&self.memory);
            if let Some(store) = &self.store
                && let Err(e) = lock(store).clear_script_memory(entity)
            {
                self.store_errors.fetch_add(1, Ordering::Relaxed);
                tracing::error!(error = %e, "clearing script memory failed");
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

    /// Reads the world.
    pub fn read<R>(&self, f: impl FnOnce(&World) -> R) -> R {
        f(&lock(&self.world))
    }

    /// Changes the world through `f`, then persists and broadcasts.
    pub fn write<R>(&self, f: impl FnOnce(&mut World, u64) -> R) -> R {
        let now = self.now_ms();
        let mut w = lock(&self.world);
        let out = f(&mut w, now);
        self.commit(&mut w, now);
        drop(w);
        out
    }

    fn commit(&self, w: &mut World, now: u64) {
        let fresh = w.drain_fresh();
        let audit = w.drain_audit();
        if let Some(store) = &self.store {
            let snapshot = w.snapshot();
            let saved = lock(store).save(&w.pack().world.name, &snapshot, &fresh, now);
            if let Err(e) = saved {
                self.store_errors.fetch_add(1, Ordering::Relaxed);
                tracing::error!(error = %e, "saving the world failed");
            }
        }
        for e in fresh {
            let _ = self.events.send(Arc::new(e));
        }
        for a in audit {
            let _ = self.audit.send(Arc::new(a));
        }
    }

    /// Runs one line for `actor`.
    pub fn execute(&self, actor: &EntityId, line: &str) -> CommandResult {
        self.write(|w, now| w.execute(actor, line, now))
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

    /// `mud_events` for `viewer`: the events after `since`, as views, and
    /// the cursor to pass next time.
    #[must_use]
    pub fn events(&self, viewer: &EntityId, since: Seq, limit: usize) -> serde_json::Value {
        self.read(|w| {
            let evs = w.events_since(viewer, since, limit);
            let cursor = evs.last().map_or(since, |e| e.seq);
            let room = w.entity(viewer).map(|e| e.room.clone());
            serde_json::json!({
                "cursor": cursor,
                "room_id": room,
                "events": evs.iter().map(|e| e.view_for(viewer)).collect::<Vec<_>>(),
            })
        })
    }
}

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
        let mut s = Store::open(&db).unwrap();
        let state = crate::state::WorldState::from_pack(&pack(), 1);
        s.save("other-world", &state, &[], 0).unwrap();
        drop(s);
        let err = Game::open(pack(), Some(&db), 1, Arc::new(SystemClock)).unwrap_err();
        assert!(err.to_string().contains("other-world"));
    }

    #[test]
    fn the_store_pages_events() {
        let mut s = Store::open_in_memory().unwrap();
        let mut w = World::new(pack(), 1);
        let ada = w.login("ada", 5).unwrap();
        w.execute(&ada, "say one", 6);
        w.execute(&ada, "say two", 7);
        let fresh = w.drain_fresh();
        s.save("crypt-v01", &w.snapshot(), &fresh, 8).unwrap();
        assert_eq!(s.event_count().unwrap(), 3);
        let after = s.events_after(1, 10).unwrap();
        assert_eq!(after.len(), 2);
        assert_eq!(after[1].untrusted_text.as_deref(), Some("two"));
        assert!(SystemClock.now_ms() > 1_600_000_000_000);
    }
}
