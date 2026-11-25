//! What a restart derives from the reloaded event log (ADR 0011, phase 4):
//! the addressed events no committed wake consumed come back as Immediate
//! wakes within `replay_max_age_ms`, the chatter counters are folded from
//! the log, and the floor every agent's MCP cursor starts at.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use mud_world::{EntityId, EventKind, Seq};

use super::persist_tests::{Disk, Run, T0, crash_after, marta, quiet, start};
use super::*;
use crate::mind::{BoxFuture, MindError};

/// One wake as the mind saw it: its reason and the events it carried.
type Seen = (String, Vec<(EventKind, Seq)>);

/// A mind that answers every wake aloud and records the event kinds and
/// seqs each wake carried.
#[derive(Debug, Default)]
struct Answerer {
    wakes: StdMutex<Vec<Seen>>,
}

impl Answerer {
    fn wakes(&self) -> Vec<Seen> {
        self.wakes.lock().unwrap().clone()
    }
}

impl Mind for Answerer {
    fn kind(&self) -> &'static str {
        "answerer"
    }

    fn wake(
        &self,
        ctx: WakeContext,
        tools: MudTools,
    ) -> BoxFuture<'_, Result<MindReply, MindError>> {
        Box::pin(async move {
            self.wakes.lock().unwrap().push((
                ctx.reason.clone(),
                ctx.events.iter().map(|e| (e.kind, e.seq)).collect(),
            ));
            tools.command("say aye").await;
            Ok(MindReply::default())
        })
    }
}

fn answering_marta() -> (Arc<Answerer>, HashMap<EntityId, Arc<dyn Mind>>) {
    let m = Arc::new(Answerer::default());
    let minds = HashMap::from([(marta(), Arc::clone(&m) as Arc<dyn Mind>)]);
    (m, minds)
}

/// Ada tells marta `text` and returns the seq of the tell.
fn ada_tells_marta(r: &Run, text: &str) -> Seq {
    let ada = r.game.login("ada").unwrap();
    let res = r.game.execute(&ada, &format!("tell marta {text}"));
    assert!(res.ok, "{res:?}");
    r.game.read(mud_world::World::last_seq)
}

/// Where the crash comes in the second half of the exchange.
#[derive(Debug, Clone, Copy)]
enum Kill {
    /// Before marta's wake was taken: only the router knew of the tell.
    BeforeTheWake,
    /// After the wake answered in memory, before that answer committed.
    BeforeTheAnswerCommits,
    /// After the answer committed with the wake's consume.
    AfterTheAnswer,
}

#[tokio::test]
async fn a_tell_just_before_a_crash_is_answered_once() {
    for kill in [
        Kill::BeforeTheWake,
        Kill::BeforeTheAnswerCommits,
        Kill::AfterTheAnswer,
    ] {
        let disk = Disk::new();
        let (first, minds) = answering_marta();
        let mut r = start(&disk, T0, quiet(), minds);
        // One whole exchange first: a tell, the answer, the commit.
        ada_tells_marta(&r, "evening");
        r.run_for(2_000, 500).await;
        assert_eq!(r.said(&marta()), ["aye"]);
        // The second tell, and the kill.
        let tell = ada_tells_marta(&r, "an ale please");
        match kill {
            Kill::BeforeTheWake => {}
            Kill::BeforeTheAnswerCommits => {
                crash_after(&r.game, &marta(), 0);
                r.run_for(2_000, 500).await;
            }
            Kill::AfterTheAnswer => {
                crash_after(&r.game, &marta(), 1);
                r.run_for(2_000, 500).await;
            }
        }
        let answered_before = match kill {
            Kill::BeforeTheWake => 1,
            Kill::BeforeTheAnswerCommits | Kill::AfterTheAnswer => 2,
        };
        assert_eq!(first.wakes().len(), answered_before, "{kill:?}");
        drop(r);

        let (second, minds) = answering_marta();
        let mut r = start(&disk, T0 + 10_000, quiet(), minds);
        r.run_for(10_000, 500).await;
        assert_eq!(
            r.said(&marta()),
            ["aye", "aye"],
            "{kill:?}: each tell is answered once, durably"
        );
        let replayed = second.wakes();
        match kill {
            Kill::AfterTheAnswer => {
                assert!(replayed.is_empty(), "{kill:?}: no replay: {replayed:?}");
            }
            Kill::BeforeTheWake | Kill::BeforeTheAnswerCommits => {
                assert_eq!(replayed.len(), 1, "{kill:?}: {replayed:?}");
                let (reason, events) = &replayed[0];
                assert_eq!(
                    events,
                    &[(EventKind::Tell, tell)],
                    "{kill:?}: only the unanswered tell"
                );
                assert_eq!(reason, "replayed tell by ada", "{kill:?}");
            }
        }
        let feed = r.feed();
        let replay_notes = feed
            .iter()
            .filter(|e| matches!(&***e, ObserverEvent::Route(RouteNote::Queued { seq, .. }) if *seq == tell))
            .count();
        let want = usize::from(!matches!(kill, Kill::AfterTheAnswer));
        assert_eq!(replay_notes, want, "{kill:?}: the boot report shows it");
        drop(r);

        // A third boot replays nothing: the replayed answer committed.
        let (third, minds) = answering_marta();
        let mut r = start(&disk, T0 + 20_000, quiet(), minds);
        r.run_for(10_000, 500).await;
        assert!(third.wakes().is_empty(), "{kill:?}");
        assert_eq!(r.said(&marta()), ["aye", "aye"], "{kill:?}");
    }
}

#[tokio::test]
async fn a_tell_older_than_the_window_is_not_replayed() {
    let disk = Disk::new();
    let (_, minds) = answering_marta();
    let r = start(&disk, T0, quiet(), minds);
    let tell = ada_tells_marta(&r, "are you there");
    drop(r);

    let cfg = quiet();
    let (late, minds) = answering_marta();
    let mut r = start(&disk, T0 + cfg.replay_max_age_ms + 1, cfg, minds);
    r.run_for(10_000, 500).await;
    assert!(late.wakes().is_empty(), "{:?}", late.wakes());
    assert_eq!(r.said(&marta()), Vec::<String>::new());
    assert_eq!(
        r.sup.seen_seq(&marta()),
        Some(Seq(0)),
        "the floor stays below the tell: mud_events still hands it out"
    );
    assert!(
        r.game.events(&marta(), Seq(0), 100)["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["seq"] == tell.0),
        "the tell is still in the log"
    );
}

#[tokio::test]
async fn a_tell_within_the_window_is_replayed_after_a_short_outage() {
    let disk = Disk::new();
    let (_, minds) = answering_marta();
    let r = start(&disk, T0, quiet(), minds);
    let tell = ada_tells_marta(&r, "quick question");
    drop(r);

    let cfg = quiet();
    let (soon, minds) = answering_marta();
    let mut r = start(&disk, T0 + cfg.replay_max_age_ms, cfg, minds);
    r.run_for(5_000, 500).await;
    let wakes = soon.wakes();
    assert_eq!(wakes.len(), 1, "{wakes:?}");
    assert_eq!(wakes[0].1, [(EventKind::Tell, tell)]);
    assert_eq!(r.said(&marta()), ["aye"]);
}

#[tokio::test]
async fn chatter_counters_are_rebuilt_at_boot() {
    // Vich and marta talk alone in the taproom until the room is held.
    // After a restart, the fold of the log holds it again: vich's tell
    // does not wake marta, as it would not have before the restart.
    let disk = Disk::new();
    let r = start(&disk, T0, quiet(), HashMap::new());
    let vich = EntityId::from("vich");
    for line in ["w", "n", "e", "say quiet night", "say very quiet"] {
        assert!(r.game.execute(&vich, line).ok, "{line}");
    }
    assert!(r.game.execute(&marta(), "say it is").ok);
    drop(r);

    let (m, minds) = answering_marta();
    let mut r = start(&disk, T0 + 1_000, quiet(), minds);
    r.run_for(2_000, 500).await;
    assert!(
        m.wakes().is_empty(),
        "nothing addressed her: {:?}",
        m.wakes()
    );
    assert!(r.game.execute(&vich, "tell marta psst").ok);
    r.run_for(5_000, 500).await;
    assert!(m.wakes().is_empty(), "held: {:?}", m.wakes());
    let feed = r.feed();
    assert!(
        feed.iter().any(|e| matches!(
            &**e,
            ObserverEvent::Route(RouteNote::Dropped { entity, why, .. })
                if *entity == marta() && why.starts_with("anti-chatter")
        )),
        "{feed:?}"
    );
    // A fresh world never held the room: the same tell wakes her.
    let disk = Disk::new();
    let (m, minds) = answering_marta();
    let mut r = start(&disk, T0, quiet(), minds);
    for line in ["w", "n", "e"] {
        assert!(r.game.execute(&vich, line).ok, "{line}");
    }
    assert!(r.game.execute(&vich, "tell marta psst").ok);
    r.run_for(5_000, 500).await;
    assert_eq!(m.wakes().len(), 1, "{:?}", m.wakes());
}

#[tokio::test]
async fn mcp_cursor_starts_at_seen_seq() {
    let disk = Disk::new();
    let (_, minds) = answering_marta();
    let mut r = start(&disk, T0, quiet(), minds);
    let tell = ada_tells_marta(&r, "evening");
    r.run_for(2_000, 500).await;
    assert_eq!(r.said(&marta()), ["aye"]);
    let last = r.game.read(mud_world::World::last_seq);
    drop(r);

    // What the server reads before it issues tokens.
    let (game, clock) = disk.game(T0 + 5_000);
    let side = game.take_restored().unwrap();
    let floor = crate::replay::boot_floor(&side, &marta(), last);
    assert_eq!(floor, tell, "the last event her committed wake consumed");
    let torin = EntityId::from("torin");
    assert_eq!(
        crate::replay::boot_floor(&side, &torin, last),
        last,
        "no row: the last seq"
    );
    let views = game.events(&marta(), floor, 100);
    let seqs: Vec<u64> = views["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["seq"].as_u64())
        .collect();
    assert!(seqs.iter().all(|s| *s > tell.0), "no repeat: {seqs:?}");
    assert!(!seqs.is_empty(), "her answer and later events follow");

    // The supervisor derives the same floor, and writes a row for an agent
    // that had none, so the next boot reads the same floor again.
    let (_, mut minds) = answering_marta();
    minds.insert(torin.clone(), Arc::new(Answerer::default()));
    let typing = Gate::new(Arc::clone(&game), 5, 0.0);
    let fallback = Arc::new(FallbackMind::from_cast(&disk.pack.cast));
    let sup = Supervisor::open(Arc::clone(&game), typing, minds, fallback, quiet(), side);
    assert_eq!(sup.seen_seq(&marta()), Some(tell));
    assert_eq!(sup.seen_seq(&torin), Some(last));
    drop((sup, game, clock));
    let rows = disk.sql(
        "SELECT entity, seen_seq FROM sup_entity ORDER BY entity",
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
    );
    let torin_row = rows.iter().find(|(e, _)| e == "torin");
    assert_eq!(
        torin_row.map(|(_, s)| u64::try_from(*s).unwrap()),
        Some(last.0),
        "{rows:?}"
    );
}
