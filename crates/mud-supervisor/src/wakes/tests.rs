use std::sync::Arc;

use mud_world::{Game, ManualClock, WorldPack};
use proptest::prelude::*;

use super::*;

fn torin() -> EntityId {
    EntityId::from("torin")
}

/// Takes the due bookings and consumes them, as a wake that acted does.
fn fire(book: &WakeBook, now: u64) -> Vec<(EntityId, Vec<Booking>)> {
    let due = book.take_due(now, u64::MAX, |_| true).taken;
    for (e, list) in &due {
        let ids: Vec<u64> = list.iter().map(|b| b.id).collect();
        book.consumed(e, &ids);
    }
    due
}

#[test]
fn a_booking_comes_due_once_with_its_note() {
    let book = WakeBook::default();
    let b = book
        .book(&torin(), 1_000, 25, "check the board for ore")
        .unwrap();
    assert_eq!(b.due_ms, 1_000 + 25 * 60_000);
    assert!(
        book.take_due(b.due_ms - 1, u64::MAX, |_| true)
            .taken
            .is_empty(),
        "not yet"
    );
    let due = book.take_due(b.due_ms, u64::MAX, |_| true).taken;
    assert_eq!(due, vec![(torin(), vec![b.clone()])]);
    assert!(book.in_flight(b.id), "taken, not gone");
    assert_eq!(book.pending(&torin()), vec![b.clone()], "still booked");
    assert!(
        book.take_due(u64::MAX, u64::MAX, |_| true).taken.is_empty(),
        "once"
    );
    book.consumed(&torin(), &[b.id]);
    assert!(book.pending(&torin()).is_empty(), "consumed");
    assert!(!book.in_flight(b.id));
}

#[test]
fn a_requeued_booking_comes_due_again() {
    let book = WakeBook::default();
    let b = book.book(&torin(), 0, 1, "x").unwrap();
    let due = book.take_due(b.due_ms, u64::MAX, |_| true).taken;
    assert_eq!(due.len(), 1);
    book.requeue(&[b.id]);
    assert_eq!(
        book.take_due(b.due_ms, u64::MAX, |_| true).taken[0].1,
        vec![b]
    );
}

#[test]
fn an_entity_that_is_not_ready_keeps_its_booking() {
    let book = WakeBook::default();
    let b = book.book(&torin(), 0, 1, "x").unwrap();
    assert!(
        book.take_due(b.due_ms + 5, u64::MAX, |_| false)
            .taken
            .is_empty(),
        "held"
    );
    assert_eq!(
        book.take_due(b.due_ms + 9, u64::MAX, |_| true).taken[0].1,
        vec![b]
    );
}

#[test]
fn limits_hold() {
    let book = WakeBook::default();
    assert_eq!(book.book(&torin(), 0, 0, "x"), Err(BookError::Minutes));
    assert_eq!(
        book.book(&torin(), 0, MAX_MINUTES + 1, "x"),
        Err(BookError::Minutes)
    );
    for m in 1..=MAX_PENDING {
        book.book(&torin(), 0, m, "x").unwrap();
    }
    assert_eq!(book.book(&torin(), 0, 9, "x"), Err(BookError::Full));
    assert_eq!(BookError::Full.code(), "full");
    assert!(
        book.book(&EntityId::from("marta"), 0, 9, "x").is_ok(),
        "per entity"
    );
    let first = book.pending(&torin())[0].id;
    assert_eq!(book.cancel(&torin(), first), Ok(true));
    assert_eq!(book.cancel(&torin(), first), Ok(false), "once");
    assert!(book.book(&torin(), 0, 9, "x").is_ok(), "room again");
}

#[test]
fn a_note_is_kept_clean_and_short() {
    let book = WakeBook::default();
    let long = format!("line\nbreak\u{7}{}", "x".repeat(400));
    let b = book.book(&torin(), 0, 5, &long).unwrap();
    assert!(b.note.starts_with("linebreak"));
    assert_eq!(b.note.chars().count(), MAX_NOTE_CHARS);
}

#[test]
fn several_due_bookings_come_together_soonest_first() {
    let book = WakeBook::default();
    let late = book.book(&torin(), 0, 10, "late").unwrap();
    let soon = book.book(&torin(), 0, 2, "soon").unwrap();
    let later = book.book(&torin(), 0, 60, "later").unwrap();
    let due = fire(&book, 10 * 60_000);
    assert_eq!(due, vec![(torin(), vec![soon, late])]);
    assert_eq!(book.pending(&torin()), vec![later]);
    book.forget(&torin());
    assert!(book.all().is_empty());
}

fn stored_game(dir: &std::path::Path) -> Arc<Game> {
    Game::open(
        Arc::new(WorldPack::crypt_v01().unwrap()),
        Some(&dir.join("world.db")),
        7,
        Arc::new(ManualClock::new(1_000)),
    )
    .unwrap()
}

#[test]
fn a_booking_survives_restart_with_its_id_note_and_due() {
    let dir = tempfile::tempdir().unwrap();
    let game = stored_game(dir.path());
    let book = WakeBook::persisted(Arc::clone(&game));
    let b = book.book(&torin(), 1_000, 30, "is the ore in?").unwrap();
    let gone = book.book(&torin(), 1_000, 40, "never mind").unwrap();
    assert_eq!(book.cancel(&torin(), gone.id), Ok(true));
    drop((book, game));

    let game = stored_game(dir.path());
    let side = game.take_restored().unwrap();
    assert_eq!(side.next_booking_id, gone.id, "the highest id issued");
    let book = WakeBook::persisted(Arc::clone(&game));
    let (dropped, _) = book.restore(
        side.bookings.into_values(),
        side.next_booking_id,
        1_000,
        60_000,
    );
    assert!(dropped.is_empty(), "nothing overdue");
    assert_eq!(book.pending(&torin()), vec![b]);
    let next = book.book(&torin(), 1_000, 5, "again").unwrap();
    assert!(
        next.id > gone.id,
        "ids never repeat: {} after {}",
        next.id,
        gone.id
    );
}

#[test]
fn book_store_failure_answers_not_ok_and_keeps_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let game = stored_game(dir.path());
    let book = WakeBook::persisted(Arc::clone(&game));
    let kept = book.book(&torin(), 1_000, 30, "kept").unwrap();
    game.refuse_store_writes(true).unwrap();
    assert_eq!(
        book.book(&torin(), 1_000, 30, "lost"),
        Err(BookError::Store)
    );
    assert_eq!(BookError::Store.code(), "store");
    assert_eq!(book.pending(&torin()), vec![kept.clone()], "nothing kept");
    assert_eq!(book.cancel(&torin(), kept.id), Err(BookError::Store));
    assert_eq!(book.pending(&torin()), vec![kept], "the cancel did nothing");
    game.refuse_store_writes(false).unwrap();
    drop((book, game));
    let game = stored_game(dir.path());
    let side = game.take_restored().unwrap();
    assert_eq!(side.bookings.len(), 1, "only the acknowledged booking");
}

#[test]
fn a_restored_booking_waits_fires_or_drops() {
    let book = WakeBook::default();
    let row = |id, due_ms| BookingRow {
        id,
        entity: torin(),
        due_ms,
        note: format!("n{id}"),
        booked_ms: 0,
    };
    let now = 10_000_000;
    let far = now + 100 * u64::from(MAX_MINUTES) * 60_000;
    let (dropped, _) = book.restore(
        vec![
            row(1, now + 5),
            row(2, now - 50),
            row(3, now - 500),
            row(4, far),
        ],
        9,
        now,
        100,
    );
    assert_eq!(dropped, vec![row(3, now - 500)]);
    let ids: Vec<(u64, u64)> = book
        .pending(&torin())
        .iter()
        .map(|b| (b.id, b.due_ms))
        .collect();
    assert_eq!(
        ids,
        vec![
            (2, now - 50),
            (1, now + 5),
            (4, now + u64::from(MAX_MINUTES) * 60_000)
        ],
        "a booking from a clock that went back is clamped"
    );
    assert_eq!(
        book.book(&torin(), now, 1, "x").map(|b| b.id),
        Ok(10),
        "past the meta"
    );
}

/// Books and cancels `n` wakes for `who` at `now`, so the pending cap
/// never stops them.
fn book_and_cancel(book: &WakeBook, who: &EntityId, now: u64, n: u32) {
    for i in 0..n {
        let b = book.book(who, now, 5, &format!("n{i}")).unwrap();
        assert_eq!(book.cancel(who, b.id), Ok(true));
    }
}

#[test]
fn at_most_max_per_day_a_day() {
    let book = WakeBook::default();
    let day = crate::budget::DAY_MS;
    let now = 3 * day + 1_000;
    book_and_cancel(&book, &torin(), now, MAX_PER_DAY);
    assert_eq!(book.booked_today(&torin(), now), MAX_PER_DAY);
    let late = now + day - 1_001;
    assert_eq!(
        book.book(&torin(), late, 5, "one more"),
        Err(BookError::DailyCap),
        "the same day, to its last ms"
    );
    assert_eq!(BookError::DailyCap.code(), "daily_cap");
    assert!(
        book.book(&EntityId::from("marta"), now, 5, "x").is_ok(),
        "per entity"
    );
    assert!(
        book.book(&torin(), 4 * day, 5, "tomorrow").is_ok(),
        "a new day opens again"
    );
    assert_eq!(book.booked_today(&torin(), 4 * day), 1);
    // A clock set back to yesterday does not reopen the cap.
    assert_eq!(book.booked_today(&torin(), now), 1);
}

#[test]
fn a_cancel_does_not_refund_the_day() {
    let book = WakeBook::default();
    book_and_cancel(&book, &torin(), 1_000, MAX_PER_DAY - 1);
    let last = book.book(&torin(), 1_000, 5, "last").unwrap();
    assert_eq!(book.cancel(&torin(), last.id), Ok(true));
    assert!(book.pending(&torin()).is_empty(), "nothing waits");
    assert_eq!(
        book.book(&torin(), 1_000, 5, "x"),
        Err(BookError::DailyCap),
        "cancelled bookings still count"
    );
}

#[test]
fn the_day_count_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let game = stored_game(dir.path());
    let book = WakeBook::persisted(Arc::clone(&game));
    book_and_cancel(&book, &torin(), 1_000, MAX_PER_DAY);
    drop((book, game));

    let game = stored_game(dir.path());
    let side = game.take_restored().unwrap();
    let row = side.entities.get(&torin()).unwrap();
    assert_eq!((row.booked_day, row.booked_today), (0, MAX_PER_DAY));
    let book = WakeBook::persisted(Arc::clone(&game));
    book.restore_days(
        side.entities
            .iter()
            .map(|(e, r)| (e.clone(), r.booked_day, r.booked_today)),
    );
    let _ = book.restore(
        side.bookings.into_values(),
        side.next_booking_id,
        1_000,
        60_000,
    );
    assert_eq!(book.pending(&torin()), Vec::new());
    assert_eq!(
        book.book(&torin(), 2_000, 5, "the 25th"),
        Err(BookError::DailyCap)
    );
}

#[test]
fn a_booking_on_a_turn_counts_with_its_row() {
    let dir = tempfile::tempdir().unwrap();
    let game = stored_game(dir.path());
    let book = WakeBook::persisted(Arc::clone(&game));
    game.open_turn(&torin(), "w1", Vec::new());
    book.book_on_turn(&torin(), "w1", 1_000, 5, "x").unwrap();
    assert_eq!(book.booked_today(&torin(), 1_000), 1, "counted in memory");
    game.close_turn(&torin(), "w1", Vec::new()).unwrap();
    drop((book, game));
    let game = stored_game(dir.path());
    let side = game.take_restored().unwrap();
    assert_eq!(side.bookings.len(), 1);
    assert_eq!(side.entities.get(&torin()).unwrap().booked_today, 1);
}

#[test]
fn an_overdue_booking_is_dropped_live_as_at_boot() {
    let dir = tempfile::tempdir().unwrap();
    let game = stored_game(dir.path());
    let book = WakeBook::persisted(Arc::clone(&game));
    let max_late = 60_000;
    let late = book.book(&torin(), 1_000, 1, "late").unwrap();
    let kept = book.book(&torin(), 1_000, 30, "kept").unwrap();
    let now = late.due_ms + max_late + 1;
    // The same rule at boot drops it.
    assert_eq!(
        booking_disposition(late.due_ms, now, max_late),
        BookingDisposition::Drop
    );
    let pass = book.take_due(now, max_late, |_| false);
    assert_eq!(pass.dropped, vec![(torin(), late)]);
    assert!(pass.taken.is_empty() && pass.held.is_empty());
    assert_eq!(book.pending(&torin()), vec![kept.clone()]);
    assert!(
        book.take_due(now, max_late, |_| true).dropped.is_empty(),
        "dropped once"
    );
    drop((book, game));
    let game = stored_game(dir.path());
    let side = game.take_restored().unwrap();
    assert_eq!(
        side.bookings.keys().copied().collect::<Vec<_>>(),
        vec![kept.id],
        "its row is gone from the store too"
    );
    // At exactly max_late it still fires, live as at boot.
    let book = WakeBook::default();
    let b = book.book(&torin(), 0, 1, "x").unwrap();
    let pass = book.take_due(b.due_ms + max_late, max_late, |_| true);
    assert_eq!(pass.dropped, Vec::new());
    assert_eq!(pass.taken, vec![(torin(), vec![b])]);
}

#[test]
fn a_held_booking_is_reported_once_per_hold_in_the_book() {
    let book = WakeBook::default();
    let b = book.book(&torin(), 0, 1, "x").unwrap();
    let mut held = 0;
    for _ in 0..3 {
        held += book.take_due(b.due_ms, u64::MAX, |_| false).held.len();
    }
    assert_eq!(held, 1, "one hold, one report");
    assert_eq!(book.take_due(b.due_ms, u64::MAX, |_| true).taken.len(), 1);
    book.requeue(&[b.id]);
    assert_eq!(
        book.take_due(b.due_ms, u64::MAX, |_| false).held,
        vec![(torin(), b)],
        "held again after it was taken: a second report"
    );
}

proptest! {
    /// `book_verdict` is total and says `Ok` only within every limit.
    #[test]
    fn book_verdict_ok_implies_bounds(
        minutes in 0u32..20_000,
        pending in 0u32..10,
        today in 0u32..40,
    ) {
        let v = book_verdict(minutes, pending, today);
        let within = (MIN_MINUTES..=MAX_MINUTES).contains(&minutes)
            && pending < MAX_PENDING
            && today < MAX_PER_DAY;
        prop_assert_eq!(v == BookVerdict::Ok, within);
        if !(MIN_MINUTES..=MAX_MINUTES).contains(&minutes) {
            prop_assert_eq!(v, BookVerdict::BadMinutes);
        } else if pending >= MAX_PENDING {
            prop_assert_eq!(v, BookVerdict::Full);
        } else if today >= MAX_PER_DAY {
            prop_assert_eq!(v, BookVerdict::DailyCap);
        }
    }

    /// However an entity books and cancels in one day, at most
    /// MAX_PER_DAY bookings are made.
    #[test]
    fn a_day_never_admits_more_than_max_per_day(
        ops in proptest::collection::vec((any::<bool>(), 0u32..20), 0..80)
    ) {
        let book = WakeBook::default();
        let mut made = 0u32;
        for (cancel, minutes) in ops {
            if cancel {
                if let Some(b) = book.pending(&torin()).first() {
                    prop_assert_eq!(book.cancel(&torin(), b.id), Ok(true));
                }
            } else if book.book(&torin(), 1_000, minutes, "n").is_ok() {
                made += 1;
            }
        }
        prop_assert!(made <= MAX_PER_DAY);
        prop_assert_eq!(book.booked_today(&torin(), 1_000), made);
    }
}

proptest! {
    /// No entity ever holds more than MAX_PENDING bookings, every
    /// booking is due at least a minute after it was made, and every
    /// booking is taken at most once.
    #[test]
    fn bookings_stay_bounded_and_fire_once(
        ops in proptest::collection::vec((0u8..3, 0u32..20, 0u64..600_000), 0..120)
    ) {
        let book = WakeBook::default();
        let mut now = 0u64;
        let mut fired = std::collections::BTreeSet::new();
        for (who, minutes, step) in ops {
            now += step;
            let e = EntityId::new(format!("e{who}"));
            if let Ok(b) = book.book(&e, now, minutes, "n") {
                prop_assert!(b.due_ms >= now + 60_000);
            }
            for (_, due) in fire(&book, now) {
                for b in due {
                    prop_assert!(b.due_ms <= now);
                    prop_assert!(fired.insert(b.id), "fired twice");
                }
            }
            for list in book.all().values() {
                prop_assert!(u32::try_from(list.len()).unwrap() <= MAX_PENDING);
            }
        }
    }
}

/// Every `.rs` file under `dir`, recursively.
fn sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn the_book_is_never_a_side_door() {
    // The op names, spelled so this file does not hold them.
    let ops = [
        concat!("Booking", "Insert"),
        concat!("Booking", "Delete"),
        concat!("SideOp::", "Booked"),
    ];
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let book = std::path::Path::new("mud-supervisor/src/wakes.rs");
    let mut files = Vec::new();
    for krate in std::fs::read_dir(&crates).unwrap() {
        let krate = krate.unwrap().path();
        // mud-world defines the ops and stores them; it books nothing.
        if krate.ends_with("mud-world") {
            continue;
        }
        let src = krate.join("src");
        if src.is_dir() {
            sources(&src, &mut files);
        }
    }
    assert!(files.len() > 20, "the scan found the sources: {files:?}");
    let mut found = Vec::new();
    for f in &files {
        let rel = f.strip_prefix(&crates).unwrap();
        let text = std::fs::read_to_string(f).unwrap();
        for op in ops {
            if text.contains(op) && rel != book {
                found.push(format!("{} builds {op}", rel.display()));
            }
        }
    }
    assert!(
        found.is_empty(),
        "only the wake book writes bookings: {found:?}"
    );
    let own = std::fs::read_to_string(crates.join(book)).unwrap();
    assert!(
        ops.iter().all(|op| own.contains(op)),
        "the book writes all three"
    );
}
