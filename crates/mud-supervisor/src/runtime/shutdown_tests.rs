//! The stop: a wake still running when the server stops ends, and its
//! close reaches the store, before the store closes.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;

use mud_world::EntityId;

use super::persist_tests::{Disk, Run, T0, start, torin};
use super::*;
use crate::mind::{BoxFuture, MindError};

/// A mind that answers after `ms` of real time, having spent; with
/// `ms == u64::MAX` it never answers.
#[derive(Debug)]
struct Slow {
    ms: u64,
    wakes: AtomicU32,
}

impl Mind for Slow {
    fn kind(&self) -> &'static str {
        "slow"
    }

    fn wake(&self, _: WakeContext, _: MudTools) -> BoxFuture<'_, Result<MindReply, MindError>> {
        self.wakes.fetch_add(1, Ordering::SeqCst);
        let ms = self.ms;
        Box::pin(async move {
            if ms == u64::MAX {
                std::future::pending::<()>().await;
            }
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Ok(MindReply {
                text: Some("done".into()),
                usage: Usage {
                    calls: 1,
                    input_tokens: 100,
                    ..Usage::default()
                },
                ..MindReply::default()
            })
        })
    }
}

/// Starts torin's boot tick and returns once its wake is in flight; the
/// wake task is left running, as the clock task leaves it.
async fn wake_in_flight(ms: u64) -> (Disk, Run, Arc<Slow>) {
    let disk = Disk::new();
    let mind = Arc::new(Slow {
        ms,
        wakes: AtomicU32::new(0),
    });
    let cfg = SupervisorConfig {
        first_macro_tick_secs: [1, 1],
        ..SupervisorConfig::default()
    };
    let minds = HashMap::from([(torin(), Arc::clone(&mind) as Arc<dyn Mind>)]);
    let r = start(&disk, T0, cfg, minds);
    r.clock.advance(2_000);
    let mut started = Vec::new();
    for _ in 0..5 {
        started.extend(r.sup.step());
        if !started.is_empty() {
            break;
        }
    }
    drop(started);
    tokio::time::timeout(Duration::from_secs(5), async {
        while mind.wakes.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    (disk, r, mind)
}

fn stored_tokens(disk: &Disk) -> i64 {
    disk.sql(
        "SELECT local_tokens FROM sup_entity WHERE entity = 'torin'",
        |row| row.get::<_, i64>(0),
    )
    .first()
    .copied()
    .unwrap_or(0)
}

#[tokio::test]
async fn a_wake_in_flight_at_the_stop_closes_before_the_store_does() {
    let (disk, r, _mind) = wake_in_flight(300).await;
    let errors = r.game.store_errors();
    r.sup.close_minds().await;
    assert!(r.sup.drain(Duration::from_secs(5)).await, "the wake ended");
    r.game.close_store().unwrap();
    assert_eq!(r.game.store_errors(), errors, "no write after the close");
    assert!(
        stored_tokens(&disk) >= 100,
        "the wake's spend is in the file"
    );
}

#[tokio::test]
async fn the_drain_gives_up_on_a_wake_that_never_ends() {
    let (_disk, r, _mind) = wake_in_flight(u64::MAX).await;
    let t = tokio::time::Instant::now();
    assert!(!r.sup.drain(Duration::from_millis(100)).await);
    assert!(t.elapsed() < Duration::from_secs(5), "bounded");
}

/// A mind that counts its closes.
#[derive(Debug, Default)]
struct Closes(AtomicU32);

impl Mind for Closes {
    fn kind(&self) -> &'static str {
        "closes"
    }

    fn wake(&self, _: WakeContext, _: MudTools) -> BoxFuture<'_, Result<MindReply, MindError>> {
        Box::pin(async { Ok(MindReply::default()) })
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::ready(()))
    }
}

#[tokio::test]
async fn each_mind_is_closed_once_however_many_entities_share_it() {
    let disk = Disk::new();
    let shared = Arc::new(Closes::default());
    let own = Arc::new(Closes::default());
    let minds = HashMap::from([
        (torin(), Arc::clone(&shared) as Arc<dyn Mind>),
        (
            EntityId::from("marta"),
            Arc::clone(&shared) as Arc<dyn Mind>,
        ),
        (EntityId::from("vich"), Arc::clone(&own) as Arc<dyn Mind>),
    ]);
    let r = start(&disk, T0, SupervisorConfig::default(), minds);
    r.sup.close_minds().await;
    assert_eq!(shared.0.load(Ordering::SeqCst), 1);
    assert_eq!(own.0.load(Ordering::SeqCst), 1);
}
