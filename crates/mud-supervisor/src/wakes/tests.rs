use std::sync::Arc;

use mud_world::{Game, ManualClock, WorldPack};
use proptest::prelude::*;

use super::*;

fn torin() -> EntityId {
    EntityId::from("torin")
}

/// Takes the due bookings and consumes them, as a wake that acted does.
fn fire(book: &WakeBook, now: u64) -> Vec<(EntityId, Vec<Booking>)> {
    let due = book.take_due(now, |_| true);
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
    assert!(book.take_due(b.due_ms - 1, |_| true).is_empty(), "not yet");
    let due = book.take_due(b.due_ms, |_| true);
    assert_eq!(due, vec![(torin(), vec![b.clone()])]);
    assert!(book.in_flight(b.id), "taken, not gone");
    assert_eq!(book.pending(&torin()), vec![b.clone()], "still booked");
    assert!(book.take_due(u64::MAX, |_| true).is_empty(), "once");
    book.consumed(&torin(), &[b.id]);
    assert!(book.pending(&torin()).is_empty(), "consumed");
    assert!(!book.in_flight(b.id));
}

#[test]
fn a_requeued_booking_comes_due_again() {
    let book = WakeBook::default();
    let b = book.book(&torin(), 0, 1, "x").unwrap();
    let due = book.take_due(b.due_ms, |_| true);
    assert_eq!(due.len(), 1);
    book.requeue(&[b.id]);
    assert_eq!(book.take_due(b.due_ms, |_| true)[0].1, vec![b]);
}

#[test]
fn an_entity_that_is_not_ready_keeps_its_booking() {
    let book = WakeBook::default();
    let b = book.book(&torin(), 0, 1, "x").unwrap();
    assert!(book.take_due(b.due_ms + 5, |_| false).is_empty(), "held");
    assert_eq!(book.take_due(b.due_ms + 9, |_| true)[0].1, vec![b]);
}

#[test]
fn limits_hold() {
    let book = WakeBook::default();
    assert_eq!(book.book(&torin(), 0, 0, "x"), Err(BookError::Minutes));
    assert_eq!(
        book.book(&torin(), 0, MAX_MINUTES + 1, "x"),
        Err(BookError::Minutes)
    );
    for m in 1..=5 {
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
    let dropped = book.restore(
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
    let dropped = book.restore(
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
                prop_assert!(list.len() <= MAX_PENDING);
            }
        }
    }
}
