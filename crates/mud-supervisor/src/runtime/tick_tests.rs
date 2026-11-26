//! Macro ticks after a bad restart (the owner's run of 2026-11-26): an
//! idle player no longer holds an NPC's tick back, and a tick whose wake
//! never reached the mind comes again in minutes, backing off, not hours
//! later.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32};

use mud_world::store::Store;
use mud_world::{EntityId, RoomId, World};

use super::persist_tests::{Disk, Run, T0, start, torin};
use super::*;
use crate::mind::{BoxFuture, MindError};

const HOUR: u64 = 3_600_000;

/// A mind that is up or down: down, every wake fails as an `lg serve`
/// that exits at start does.
#[derive(Debug, Default)]
struct Switch {
    down: AtomicBool,
    wakes: AtomicU32,
}

impl Mind for Switch {
    fn kind(&self) -> &'static str {
        "switch"
    }

    fn wake(&self, _: WakeContext, _: MudTools) -> BoxFuture<'_, Result<MindReply, MindError>> {
        self.wakes.fetch_add(1, Ordering::SeqCst);
        let down = self.down.load(Ordering::SeqCst);
        Box::pin(async move {
            if down {
                Err(MindError::Unavailable(
                    "lg serve for torin exited (exit status: 2)".into(),
                ))
            } else {
                Ok(MindReply {
                    text: Some("ok".into()),
                    ..MindReply::default()
                })
            }
        })
    }
}

fn ticking() -> SupervisorConfig {
    SupervisorConfig {
        first_macro_tick_secs: [1, 1],
        ..SupervisorConfig::default()
    }
}

fn only_torin(mind: &Arc<Switch>) -> HashMap<EntityId, Arc<dyn Mind>> {
    HashMap::from([(torin(), Arc::clone(mind) as Arc<dyn Mind>)])
}

fn entity(r: &Run, id: &str) -> Value {
    r.sup.snapshot()["entities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == id)
        .unwrap()
        .clone()
}

fn next_macro(r: &Run) -> u64 {
    entity(r, "torin")["next_macro_ms"].as_u64().unwrap()
}

fn stored_next_macro(disk: &Disk) -> u64 {
    disk.sql(
        "SELECT next_macro_ms FROM sup_entity WHERE entity = 'torin'",
        |row| row.get::<_, i64>(0),
    )[0]
    .try_into()
    .unwrap()
}

/// The time of the last wake of torin that ended.
fn last_wake_end(feed: &[Arc<ObserverEvent>], started: &mut u64) -> Option<bool> {
    let mut ok = None;
    for e in feed {
        match &**e {
            ObserverEvent::WakeStarted { entity, at_ms, .. } if *entity == torin() => {
                *started = *at_ms;
            }
            ObserverEvent::WakeFinished { entity, ok: o, .. } if *entity == torin() => {
                ok = Some(*o);
            }
            _ => {}
        }
    }
    ok
}

#[tokio::test]
async fn an_npc_beside_an_idle_agent_pc_gets_its_macro_tick() {
    // Regression: after a restart kestrel (a Turing PC, online, its mind
    // gone) and torin stood in the flooded cistern; torin's tick was
    // deferred every few minutes forever.
    let disk = Disk::new();
    let mut st = World::new(Arc::clone(&disk.pack), 5).snapshot();
    for who in ["pc_kestrel", "torin"] {
        let e = st.entities.get_mut(&EntityId::from(who)).unwrap();
        e.room = RoomId::from("flooded_cistern");
        e.online = true;
    }
    let mut store = Store::open(&disk.db(), 5, T0).unwrap();
    store
        .save(&disk.pack.world.name, &st, &[], &[], T0)
        .unwrap();
    drop(store);
    let mind = Arc::new(Switch::default());
    let mut r = start(&disk, T0, ticking(), only_torin(&mind));
    let kestrel = EntityId::from("pc_kestrel");
    assert!(r.game.read(|w| {
        let k = w.entity(&kestrel).unwrap();
        k.online && k.room == RoomId::from("flooded_cistern")
    }));
    r.run_for(5_000, 500).await;
    assert_eq!(entity(&r, "torin")["macro_ticks"], 1, "torin woke");
    assert_eq!(mind.wakes.load(Ordering::SeqCst), 1);
    assert!(
        next_macro(&r) >= T0 + 2 * HOUR,
        "the next one is on the normal schedule"
    );
}

#[tokio::test]
async fn a_tick_lost_to_an_unavailable_mind_retries_in_minutes_and_backs_off() {
    let disk = Disk::new();
    let mind = Arc::new(Switch::default());
    mind.down.store(true, Ordering::SeqCst);
    let mut r = start(&disk, T0, ticking(), only_torin(&mind));
    let mut at = 0;
    // The first tick fires at boot and its wake fails.
    r.run_for(2_000, 500).await;
    assert_eq!(last_wake_end(&r.feed(), &mut at), Some(false));
    let wait = next_macro(&r) - at;
    assert!((135_000..=180_000).contains(&wait), "first retry {wait}");
    assert_eq!(stored_next_macro(&disk), next_macro(&r), "persisted");
    // The next try fails too: it waits twice as long.
    let due = next_macro(&r);
    r.clock.advance(due - r.game.now_ms());
    r.run_for(1_000, 500).await;
    assert_eq!(last_wake_end(&r.feed(), &mut at), Some(false));
    let wait = next_macro(&r) - at;
    assert!((270_000..=360_000).contains(&wait), "second retry {wait}");
    assert_eq!(mind.wakes.load(Ordering::SeqCst), 2);
    // The mind recovers: the tick goes through, and the next one is 2 to
    // 6 hours out again.
    mind.down.store(false, Ordering::SeqCst);
    let due = next_macro(&r);
    r.clock.advance(due - r.game.now_ms());
    r.run_for(1_000, 500).await;
    assert_eq!(last_wake_end(&r.feed(), &mut at), Some(true));
    let wait = next_macro(&r) - at;
    assert!((2 * HOUR..=6 * HOUR).contains(&wait), "normal again {wait}");
    assert_eq!(stored_next_macro(&disk), next_macro(&r));
    // A new loss starts the backoff over.
    mind.down.store(true, Ordering::SeqCst);
    let due = next_macro(&r);
    r.clock.advance(due - r.game.now_ms());
    r.run_for(1_000, 500).await;
    assert_eq!(last_wake_end(&r.feed(), &mut at), Some(false));
    let wait = next_macro(&r) - at;
    assert!(
        (135_000..=180_000).contains(&wait),
        "first retry again {wait}"
    );
}

#[tokio::test]
async fn retries_never_come_sooner_than_the_last_and_stop_at_the_cast_range() {
    let disk = Disk::new();
    let mind = Arc::new(Switch::default());
    mind.down.store(true, Ordering::SeqCst);
    let mut r = start(&disk, T0, ticking(), only_torin(&mind));
    let mut at = 0;
    r.run_for(2_000, 500).await;
    let mut last = 0;
    for _ in 0..10 {
        assert_eq!(last_wake_end(&r.feed(), &mut at), Some(false));
        let wait = next_macro(&r) - at;
        assert!(wait <= 2 * HOUR, "never past the shortest interval: {wait}");
        assert!(wait * 4 >= last * 3, "backs off: {wait} after {last}");
        last = wait;
        let due = next_macro(&r);
        r.clock.advance(due - r.game.now_ms());
        r.run_for(1_000, 500).await;
    }
    assert!(last >= 90 * 60_000, "reached the cap: {last}");
    assert_eq!(mind.wakes.load(Ordering::SeqCst), 11);
}
