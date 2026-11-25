//! The side lanes of [`Game`]: turns, acts, persist, the backlog and
//! retention (ADR 0011).

use std::sync::Arc;

use super::*;
use crate::side::{BookingRow, MetaKey};

fn pack() -> Arc<WorldPack> {
    Arc::new(WorldPack::crypt_v01().unwrap())
}

struct Rig {
    _dir: tempfile::TempDir,
    db: std::path::PathBuf,
    clock: Arc<ManualClock>,
    game: Arc<Game>,
}

impl Rig {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("world.db");
        let clock = Arc::new(ManualClock::new(1_000));
        let game = Game::open(pack(), Some(&db), 7, clock.clone()).unwrap();
        Self {
            _dir: dir,
            db,
            clock,
            game,
        }
    }

    /// The side rows in the file, as a fresh open sees them.
    fn side_on_disk(&self) -> SideState {
        Store::open(&self.db, 7, 0).unwrap().load_side().unwrap()
    }

    fn events_on_disk(&self) -> u64 {
        Store::open(&self.db, 7, 0).unwrap().event_count().unwrap()
    }

    /// Reopens the file, as a restart does (the old game is dropped
    /// without any flush).
    fn restart(&mut self) {
        self.game = Game::open(pack(), Some(&self.db), 7, self.clock.clone()).unwrap();
    }
}

fn seen(entity: &EntityId, seq: u64) -> SideOp {
    SideOp::SeenSeq {
        entity: entity.clone(),
        seq,
    }
}

fn consume(entity: &EntityId, booked: Vec<u64>) -> SideOp {
    SideOp::Consume {
        entity: entity.clone(),
        macro_no: 1,
        macro_cover: 1,
        booked,
        seen_seq: 5,
    }
}

fn seen_seq(side: &SideState, e: &EntityId) -> u64 {
    side.entities.get(e).map_or(0, |r| r.seen_seq)
}

#[test]
fn staged_ops_commit_only_with_an_effectful_command_of_the_actor() {
    let r = Rig::new();
    let ada = r.game.login("ada").unwrap();
    r.game.open_turn(&ada, "w1", vec![seen(&ada, 3)]);
    r.game.stage(&ada, "w1", vec![seen(&ada, 4)]).unwrap();
    assert_eq!(seen_seq(&r.side_on_disk(), &ada), 0, "nothing yet");
    let before = r.events_on_disk();
    let said = r.game.execute(&ada, "say hello");
    assert!(said.ok);
    // The ops and the command's events are in the file together.
    assert_eq!(seen_seq(&r.side_on_disk(), &ada), 4);
    assert_eq!(r.events_on_disk(), before + 1);
    // Drained once: the turn stays open with nothing staged.
    assert_eq!(r.game.open_turn_of(&ada).as_deref(), Some("w1"));
    r.game.stage(&ada, "w1", vec![seen(&ada, 9)]).unwrap();
    assert!(r.game.execute(&ada, "emote waves").ok);
    assert_eq!(seen_seq(&r.side_on_disk(), &ada), 9);
}

#[test]
fn a_look_or_refused_command_does_not_drain_the_turn() {
    let r = Rig::new();
    let ada = r.game.login("ada").unwrap();
    r.game.open_turn(&ada, "w1", vec![seen(&ada, 3)]);
    for line in [
        "look",
        "sheet",
        "inventory",
        "frobnicate",
        "take moon",
        "move up",
        "",
    ] {
        let res = r.game.execute(&ada, line);
        assert!(!res.ok || res.read_only, "{line}: {res:?}");
        assert_eq!(
            seen_seq(&r.side_on_disk(), &ada),
            0,
            "{line} drained the turn"
        );
    }
    assert!(r.game.execute(&ada, "move n").ok);
    assert_eq!(seen_seq(&r.side_on_disk(), &ada), 3);
}

#[test]
fn login_tick_bring_online_take_offline_never_drain_a_turn() {
    let r = Rig::new();
    let ada = r.game.login("ada").unwrap();
    let torin = EntityId::from("pc_torin");
    r.game.open_turn(&ada, "w1", vec![seen(&ada, 3)]);
    r.game.open_turn(&torin, "w2", vec![seen(&torin, 3)]);
    r.game.login("bob").unwrap();
    r.game.tick();
    r.game.bring_online(&torin);
    r.game.take_offline(&torin);
    r.game.take_offline(&ada);
    r.game.write(World::tick);
    let side = r.side_on_disk();
    assert_eq!(seen_seq(&side, &ada), 0);
    assert_eq!(seen_seq(&side, &torin), 0);
    assert!(r.game.open_turn_of(&torin).is_some());
}

#[test]
fn another_actors_command_does_not_drain_my_turn() {
    let r = Rig::new();
    let ada = r.game.login("ada").unwrap();
    let bob = r.game.login("bob").unwrap();
    r.game.open_turn(&ada, "w1", vec![seen(&ada, 3)]);
    assert!(r.game.execute(&bob, "say hi").ok);
    assert_eq!(seen_seq(&r.side_on_disk(), &ada), 0);
    // Humans have no turn: bob's commands drain nothing of his own either.
    assert!(r.game.open_turn_of(&bob).is_none());
}

#[test]
fn stage_with_a_stale_wake_id_writes_nothing() {
    let r = Rig::new();
    let ada = r.game.login("ada").unwrap();
    assert!(matches!(
        r.game.stage(&ada, "w1", vec![seen(&ada, 3)]),
        Err(SideError::StaleTurn { .. })
    ));
    r.game.open_turn(&ada, "w2", vec![]);
    let err = r.game.stage(&ada, "w1", vec![seen(&ada, 3)]).unwrap_err();
    assert!(err.to_string().contains("w1"), "{err}");
    assert!(r.game.execute(&ada, "say hi").ok);
    assert_eq!(seen_seq(&r.side_on_disk(), &ada), 0);
    // Closing another wake's turn is refused and leaves it open.
    assert!(matches!(
        r.game.close_turn(&ada, "w1", vec![]),
        Err(SideError::StaleTurn { .. })
    ));
    assert_eq!(r.game.open_turn_of(&ada).as_deref(), Some("w2"));
}

#[test]
fn close_turn_consumes_what_a_silent_mind_left_staged() {
    let r = Rig::new();
    let ada = r.game.login("ada").unwrap();
    r.game
        .act(&[SideOp::BookingInsert {
            row: BookingRow {
                id: 1,
                entity: ada.clone(),
                due_ms: 2_000,
                note: "n".into(),
                booked_ms: 1_000,
            },
        }])
        .unwrap();
    r.game.open_turn(&ada, "w1", vec![consume(&ada, vec![1])]);
    assert!(r.game.execute(&ada, "look").ok);
    assert!(r.side_on_disk().bookings.contains_key(&1));
    r.game.close_turn(&ada, "w1", vec![seen(&ada, 8)]).unwrap();
    let side = r.side_on_disk();
    assert!(side.bookings.is_empty(), "consumed at close");
    assert_eq!(seen_seq(&side, &ada), 8);
    assert!(r.game.open_turn_of(&ada).is_none());
    // A new turn replaces an old one, dropping what it staged.
    r.game.open_turn(&ada, "w2", vec![seen(&ada, 50)]);
    r.game.open_turn(&ada, "w3", vec![]);
    r.game.close_turn(&ada, "w3", vec![]).unwrap();
    assert_eq!(seen_seq(&r.side_on_disk(), &ada), 8);
}

#[test]
fn staged_ops_survive_races_between_commands_and_closes() {
    let r = Rig::new();
    let ada = r.game.login("ada").unwrap();
    let bob = r.game.login("bob").unwrap();
    let game = Arc::clone(&r.game);
    let n: u64 = 1_000;
    std::thread::scope(|s| {
        // One thread opens turns and closes them with a booking id each;
        // ada's commands race it, each draining whatever is staged.
        s.spawn(|| {
            for i in 0..n {
                let wake = format!("w{i}");
                game.open_turn(
                    &ada,
                    &wake,
                    vec![SideOp::MetaMax {
                        key: MetaKey::NextBookingId,
                        value: i,
                    }],
                );
                game.stage(&ada, &wake, vec![seen(&ada, i)]).unwrap();
                game.close_turn(&ada, &wake, vec![]).unwrap();
            }
        });
        s.spawn(|| {
            for i in 0..n {
                let line = if i % 2 == 0 { "say tick" } else { "look" };
                game.execute(&ada, line);
            }
        });
        s.spawn(|| {
            for _ in 0..n / 4 {
                game.execute(&bob, "emote hums");
                game.act(&[seen(&bob, 1)]).unwrap();
            }
        });
    });
    let side = r.side_on_disk();
    assert_eq!(side.next_booking_id, n - 1);
    assert_eq!(seen_seq(&side, &ada), n - 1);
    assert!(r.game.open_turn_of(&ada).is_none());
    assert_eq!(r.game.store_errors(), 0);
}

#[test]
fn a_failed_save_keeps_events_and_ops_for_the_next_save() {
    let r = Rig::new();
    let ada = r.game.login("ada").unwrap();
    let before = r.events_on_disk();
    r.game.open_turn(&ada, "w1", vec![seen(&ada, 6)]);
    r.game.refuse_store_writes(true).unwrap();
    let mut rx = r.game.subscribe();
    assert!(r.game.execute(&ada, "say one").ok);
    assert!(r.game.execute(&ada, "say two").ok);
    assert_eq!(r.game.store_errors(), 2);
    assert!(rx.try_recv().is_ok(), "the events were still broadcast");
    assert_eq!(r.events_on_disk(), before);
    assert_eq!(seen_seq(&r.side_on_disk(), &ada), 0);
    r.game.refuse_store_writes(false).unwrap();
    // The next commit, a session change, carries the backlog.
    r.game.tick();
    assert_eq!(r.events_on_disk(), before + 2);
    assert_eq!(
        seen_seq(&r.side_on_disk(), &ada),
        6,
        "the op came with its command"
    );
    assert_eq!(r.game.store_errors(), 2);
}

#[test]
fn pending_events_are_capped_with_an_audit() {
    let r = Rig::new();
    let ada = r.game.login("ada").unwrap();
    let mut audit = r.game.subscribe_audit();
    r.game.refuse_store_writes(true).unwrap();
    let extra = 25;
    r.game.write(|w, now| {
        for i in 0..EVENT_LOG_CAP + extra {
            assert!(w.execute(&ada, &format!("say {i}"), now).ok);
        }
    });
    let mut dropped = None;
    loop {
        match audit.try_recv() {
            Ok(a) => {
                if let Audit::StoreBacklog { dropped: d, .. } = &*a {
                    dropped = Some(*d);
                }
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(_) => break,
        }
    }
    let last = r.game.read(|w| w.last_seq().0);
    assert!(last > 1 + (EVENT_LOG_CAP + extra) as u64 - 1);
    // Everything after the login's event waited; past the cap it went.
    assert_eq!(dropped, Some(last - 1 - EVENT_LOG_CAP as u64));
    r.game.refuse_store_writes(false).unwrap();
    r.game.tick();
    let s = Store::open(&r.db, 7, 0).unwrap();
    let (lo, hi) = s.event_range().unwrap().unwrap();
    assert_eq!(hi, r.game.read(|w| w.last_seq().0));
    // The login's event was saved before the failure; the oldest pending
    // ones after it were dropped.
    assert_eq!(s.event_count().unwrap(), 1 + EVENT_LOG_CAP as u64);
    assert_eq!(lo, 1);
}

#[test]
fn persist_defers_while_a_save_is_pending() {
    let r = Rig::new();
    let ada = r.game.login("ada").unwrap();
    assert_eq!(r.game.persist(vec![seen(&ada, 2)]), Durability::Durable);
    assert_eq!(seen_seq(&r.side_on_disk(), &ada), 2);
    r.game.refuse_store_writes(true).unwrap();
    // A failing store: the op waits.
    assert_eq!(r.game.persist(vec![seen(&ada, 3)]), Durability::Deferred);
    r.game.refuse_store_writes(false).unwrap();
    // Only ops wait: the next persist writes them all.
    assert_eq!(r.game.persist(vec![seen(&ada, 1)]), Durability::Durable);
    assert_eq!(seen_seq(&r.side_on_disk(), &ada), 3);
    // Events wait (a failed command): persist goes behind them.
    r.game.refuse_store_writes(true).unwrap();
    assert!(r.game.execute(&ada, "say x").ok);
    r.game.refuse_store_writes(false).unwrap();
    assert_eq!(r.game.persist(vec![seen(&ada, 7)]), Durability::Deferred);
    assert_eq!(
        seen_seq(&r.side_on_disk(), &ada),
        3,
        "not ahead of its snapshot"
    );
    r.game.tick();
    assert_eq!(seen_seq(&r.side_on_disk(), &ada), 7);
    // A world change with no event (an effectful command may be silent)
    // that failed to save: persist still goes behind its snapshot.
    r.game.refuse_store_writes(true).unwrap();
    r.game.write(|_, _| ());
    r.game.refuse_store_writes(false).unwrap();
    assert_eq!(r.game.persist(vec![seen(&ada, 9)]), Durability::Deferred);
    assert_eq!(seen_seq(&r.side_on_disk(), &ada), 7);
    r.game.tick();
    assert_eq!(seen_seq(&r.side_on_disk(), &ada), 9);
}

#[test]
fn act_fails_while_the_store_fails() {
    let r = Rig::new();
    let ada = r.game.login("ada").unwrap();
    r.game.refuse_store_writes(true).unwrap();
    assert!(r.game.execute(&ada, "say x").ok);
    let book = SideOp::BookingInsert {
        row: BookingRow {
            id: 9,
            entity: ada.clone(),
            due_ms: 5_000,
            note: "later".into(),
            booked_ms: 1_000,
        },
    };
    let err = r.game.act(std::slice::from_ref(&book)).unwrap_err();
    assert!(matches!(err, SideError::Store(_)), "{err:?}");
    r.game.open_turn(&ada, "w1", vec![seen(&ada, 4)]);
    assert!(matches!(
        r.game.close_turn(&ada, "w1", vec![]),
        Err(SideError::Store(_))
    ));
    r.game.refuse_store_writes(false).unwrap();
    // The refused act keeps nothing, now or later; the failed close
    // promised nothing, and its ops go with the next commit.
    r.game.tick();
    let side = r.side_on_disk();
    assert!(side.bookings.is_empty(), "the agent was told no");
    assert_eq!(seen_seq(&side, &ada), 4, "the close went with the tick");
    // While a backlog waits, a working act writes it with its own ops.
    r.game.refuse_store_writes(true).unwrap();
    assert!(r.game.execute(&ada, "say y").ok);
    r.game.refuse_store_writes(false).unwrap();
    let before = r.events_on_disk();
    r.game.act(&[book]).unwrap();
    assert_eq!(r.events_on_disk(), before + 1);
    assert!(r.side_on_disk().bookings.contains_key(&9));
}

#[test]
fn in_memory_lanes_are_no_ops() {
    let game = Game::new(
        World::new(pack(), 1),
        None,
        Arc::new(ManualClock::new(1_000)),
    );
    let ada = game.login("ada").unwrap();
    game.open_turn(&ada, "w", vec![seen(&ada, 1)]);
    assert!(game.open_turn_of(&ada).is_none());
    game.stage(&ada, "x", vec![]).unwrap();
    game.act(&[seen(&ada, 1)]).unwrap();
    game.close_turn(&ada, "x", vec![]).unwrap();
    assert_eq!(game.persist(vec![seen(&ada, 1)]), Durability::Durable);
    assert_eq!(game.boot(), 0);
    assert_eq!(game.epoch(), crate::hash::epoch(1, 1_000));
    assert!(game.take_restored().is_none());
    assert!(!game.retention_due(u64::MAX));
    assert_eq!(game.retain(u64::MAX).unwrap(), Retained::default());
    assert!(game.retention().is_none());
    game.set_retention(Retention::default());
    game.refuse_store_writes(true).unwrap();
}

#[test]
fn each_open_bumps_boot_and_hands_the_rows_over_once() {
    let mut r = Rig::new();
    let epoch = r.game.epoch();
    assert_eq!(r.game.boot(), 1);
    let ada = r.game.login("ada").unwrap();
    r.game.act(&[seen(&ada, 12)]).unwrap();
    r.restart();
    assert_eq!(r.game.epoch(), epoch);
    assert_eq!(r.game.boot(), 2);
    let side = r.game.take_restored().unwrap();
    assert_eq!((side.epoch, side.boot), (epoch, 2));
    assert_eq!(seen_seq(&side, &ada), 12);
    assert!(r.game.take_restored().is_none());
}

#[test]
fn rows_of_entities_removed_from_the_pack_are_dropped_on_open() {
    let mut r = Rig::new();
    let ada = r.game.login("ada").unwrap();
    let ghost = EntityId::from("npc_gone");
    r.game
        .act(&[
            seen(&ada, 3),
            seen(&ghost, 3),
            SideOp::PlanSet {
                entity: ghost.clone(),
                wake_id: "w".into(),
                lines: vec!["look".into()],
                errand: true,
                started_ms: 1,
            },
        ])
        .unwrap();
    r.restart();
    let side = r.game.take_restored().unwrap();
    assert!(side.entities.contains_key(&ada));
    assert!(!side.entities.contains_key(&ghost));
    assert!(side.plans.is_empty());
    let disk = r.side_on_disk();
    assert!(
        !disk.entities.contains_key(&ghost),
        "dropped in the file too"
    );
    assert!(disk.entities.contains_key(&ada));
}

#[test]
fn reload_brings_back_event_log_cap_events() {
    let mut r = Rig::new();
    let ada = r.game.login("ada").unwrap();
    r.game.write(|w, now| {
        for i in 0..EVENT_LOG_CAP + 10 {
            w.execute(&ada, &format!("say {i}"), now);
        }
    });
    let before = r.game.read(World::oldest_logged);
    r.restart();
    assert_eq!(RELOAD_EVENTS, EVENT_LOG_CAP);
    assert_eq!(r.game.read(World::oldest_logged), before);
    let view = r.game.events(&ada, Seq(0), 10);
    assert_eq!(view["truncated"], true);
    let last = r.game.read(World::last_seq);
    let view = r.game.events(&ada, Seq(last.0 - 5), 10);
    assert_eq!(view["events"].as_array().map(Vec::len), Some(5));
    assert_eq!(view["truncated"], false);
}

#[test]
fn mud_events_reports_truncated_below_the_in_memory_log() {
    let game = Game::new(
        World::new(pack(), 1),
        None,
        Arc::new(ManualClock::new(1_000)),
    );
    let ada = game.login("ada").unwrap();
    assert_eq!(game.events(&ada, Seq(0), 10)["truncated"], false);
    game.write(|w, now| {
        for i in 0..EVENT_LOG_CAP + 3 {
            w.execute(&ada, &format!("say {i}"), now);
        }
    });
    // The first events left the log.
    let oldest = game.read(World::oldest_logged).unwrap().0;
    let last = game.read(World::last_seq).0;
    assert_eq!(oldest, last - EVENT_LOG_CAP as u64 + 1);
    assert!(oldest >= 4);
    assert_eq!(game.events(&ada, Seq(0), 10)["truncated"], true);
    assert_eq!(game.events(&ada, Seq(oldest - 2), 10)["truncated"], true);
    assert_eq!(game.events(&ada, Seq(oldest - 1), 10)["truncated"], false);
    assert_eq!(game.events(&ada, Seq(last), 10)["truncated"], false);
}

#[test]
fn retention_archives_then_trims_and_reports() {
    let r = Rig::new();
    let ada = r.game.login("ada").unwrap();
    let dir = r.db.parent().unwrap().join("archive");
    assert_eq!(r.game.retention().unwrap().archive_dir, dir);
    // keep_min below the log size is raised to it.
    r.game.set_retention(Retention {
        keep_days: 1,
        keep_min: 0,
        every_ms: 10_000,
        batch: 4_000,
        archive_dir: dir.clone(),
    });
    assert_eq!(r.game.retention().unwrap().keep_min, EVENT_LOG_CAP as u64);
    let n = EVENT_LOG_CAP + 9_000;
    r.game.write(|w, now| {
        for i in 0..n {
            w.execute(&ada, &format!("say {i}"), now);
        }
    });
    let total = r.events_on_disk();
    assert_eq!(total, r.game.read(|w| w.last_seq().0));
    let old = total - EVENT_LOG_CAP as u64;
    assert!(old > 8_000);
    let mut audit = r.game.subscribe_audit();
    // Not old enough yet.
    assert_eq!(
        r.game.retain(r.clock.now_ms()).unwrap(),
        Retained::default()
    );
    let later = r.clock.now_ms() + 2 * crate::rewards::DAY_MS;
    assert!(r.game.retention_due(later));
    assert!(
        !r.game.retention_due(later + 1),
        "claimed until every_ms passes"
    );
    assert!(r.game.retention_due(later + 10_000));
    let done = r.game.retain(later).unwrap();
    let old_n = usize::try_from(old).unwrap();
    assert_eq!(done.trimmed, old_n);
    assert_eq!(done.archived, old_n);
    assert_eq!(done.batches, old_n.div_ceil(4_000));
    assert_eq!(r.events_on_disk(), EVENT_LOG_CAP as u64);
    assert_eq!(
        crate::archive::seqs(&dir).unwrap(),
        (1..=old).collect::<Vec<_>>()
    );
    assert!(matches!(
        &*audit.try_recv().unwrap(),
        Audit::EventsArchived { trimmed, through, .. } if *trimmed == old && *through == Seq(old)
    ));
    // A second run finds nothing.
    assert_eq!(r.game.retain(later).unwrap(), Retained::default());
    // An archive that cannot be written fails the run.
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::write(&dir, b"x").unwrap();
    r.game.write(|w, _| {
        w.execute(&ada, "say late", later);
    });
    assert!(r.game.retain(later + 5 * crate::rewards::DAY_MS).is_err());
    assert_eq!(r.game.store_errors(), 1);
}

#[test]
fn a_capped_backlog_never_leaves_a_gap_inside_the_reloaded_log() {
    // Review finding 7, the backlog path: the cap keeps EVENT_LOG_CAP
    // events after the gap it makes, and the reload reads RELOAD_EVENTS ==
    // EVENT_LOG_CAP rows, so the reloaded log starts after the gap.
    let mut r = Rig::new();
    let ada = r.game.login("ada").unwrap();
    r.game.refuse_store_writes(true).unwrap();
    r.game.write(|w, now| {
        for i in 0..EVENT_LOG_CAP + 25 {
            assert!(w.execute(&ada, &format!("say {i}"), now).ok);
        }
    });
    r.game.refuse_store_writes(false).unwrap();
    r.game.tick();
    r.restart();
    let last = r.game.read(World::last_seq).0;
    let oldest = r.game.read(World::oldest_logged).unwrap().0;
    assert_eq!(oldest, last - EVENT_LOG_CAP as u64 + 1);
    assert!(oldest > 2, "the gap is below the log");
    // Since the login's event: events 2.. are gone, and it says so.
    assert_eq!(r.game.events(&ada, Seq(1), 10)["truncated"], true);
    assert_eq!(r.game.events(&ada, Seq(oldest - 1), 10)["truncated"], false);
}

#[test]
fn a_gap_inside_the_stored_tail_is_reported_after_a_reload() {
    // Review finding 7: a file whose last rows have a hole (a version 0
    // store dropped the events of a failed save). The reloaded log keeps
    // only the run that ends at the last seq, so `truncated` stays exact.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("world.db");
    let mut w = World::new(pack(), 7);
    let ada = w.login("ada", 1_000).unwrap();
    for i in 2..=10 {
        assert!(w.execute(&ada, &format!("say {i}"), 1_000 + i).ok);
    }
    let kept: Vec<Event> = w
        .drain_fresh()
        .into_iter()
        .filter(|e| e.seq.0 != 6 && e.seq.0 != 7)
        .collect();
    let mut s = Store::open(&db, 7, 0).unwrap();
    s.save("crypt-v01", &w.snapshot(), &kept, &[], 1_000)
        .unwrap();
    drop(s);
    let game = Game::open(pack(), Some(&db), 7, Arc::new(ManualClock::new(2_000))).unwrap();
    assert_eq!(game.read(World::last_seq).0, 10);
    assert_eq!(game.events(&ada, Seq(5), 10)["truncated"], true);
    assert_eq!(game.events(&ada, Seq(1), 10)["truncated"], true);
    assert_eq!(game.events(&ada, Seq(7), 10)["truncated"], false);
    assert_eq!(game.read(World::oldest_logged), Some(Seq(8)));
}

#[test]
fn a_failed_close_waits_in_the_backlog_for_the_next_write() {
    let r = Rig::new();
    let ada = r.game.login("ada").unwrap();
    r.game.open_turn(&ada, "w1", vec![seen(&ada, 6)]);
    r.game.refuse_store_writes(true).unwrap();
    assert!(r.game.close_turn(&ada, "w1", vec![seen(&ada, 7)]).is_err());
    assert_eq!(r.game.open_turn_of(&ada), None, "the turn is gone");
    r.game.refuse_store_writes(false).unwrap();
    // Only ops wait: a persist writes them without a snapshot.
    assert_eq!(r.game.persist(vec![seen(&ada, 1)]), Durability::Durable);
    assert_eq!(seen_seq(&r.side_on_disk(), &ada), 7);
}

#[test]
fn script_memory_rides_on_the_turn_and_a_reset_drops_it() {
    let mut r = Rig::new();
    let hale = EntityId::from("guard_01");
    let (resets, before) = r.game.script_memory_at(&hale, 1);
    assert_eq!(before, serde_json::json!({}));
    r.game.open_turn(&hale, "w1", Vec::new());
    assert!(
        r.game
            .stage_script_memory_if(&hale, "w1", 1, resets, serde_json::json!({ "n": 1 }))
    );
    assert_eq!(
        r.game.script_memory(&hale, 1),
        serde_json::json!({ "n": 1 })
    );
    r.restart();
    assert_eq!(
        r.game.script_memory(&hale, 1),
        serde_json::json!({}),
        "staged, not written"
    );
    r.game.open_turn(&hale, "w2", Vec::new());
    assert!(
        r.game
            .stage_script_memory_if(&hale, "w2", 1, 0, serde_json::json!({ "n": 2 }))
    );
    assert!(r.game.execute(&hale, "say hi").ok);
    r.restart();
    assert_eq!(
        r.game.script_memory(&hale, 1),
        serde_json::json!({ "n": 2 }),
        "written with the say"
    );
    // A reset drops a staged save, and a stale read cannot save after it.
    r.game.open_turn(&hale, "w3", Vec::new());
    let (resets, _) = r.game.script_memory_at(&hale, 1);
    assert!(
        r.game
            .stage_script_memory_if(&hale, "w3", 1, resets, serde_json::json!({ "n": 3 }))
    );
    r.game.reset_script_memory(&hale);
    assert!(
        !r.game
            .stage_script_memory_if(&hale, "w3", 1, resets, serde_json::json!({ "n": 4 }))
    );
    r.game.close_turn(&hale, "w3", Vec::new()).unwrap();
    r.restart();
    assert_eq!(r.game.script_memory(&hale, 1), serde_json::json!({}));
    // With no turn of that wake, the save is written at once.
    assert!(
        r.game
            .stage_script_memory_if(&hale, "none", 1, 0, serde_json::json!({ "n": 5 }))
    );
    r.restart();
    assert_eq!(
        r.game.script_memory(&hale, 1),
        serde_json::json!({ "n": 5 })
    );
}
