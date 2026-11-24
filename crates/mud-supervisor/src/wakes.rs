//! Wakes that an agent books for itself (`mud_wake_me`).
//!
//! littleguy's own `wake_me` runs read-only, as a heartbeat does, so a wake
//! it books can look but never act in the world. A wake booked here is a
//! real wake: the supervisor routes it like any other (budgets, one wake
//! per entity at a time), and the mind may act. The note the agent left is
//! handed back to it, as its own words, when the wake comes.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use mud_world::EntityId;
use serde::{Deserialize, Serialize};

/// The most wakes one entity may have booked at once.
pub const MAX_PENDING: usize = 5;
/// The shortest delay, in minutes.
pub const MIN_MINUTES: u32 = 1;
/// The longest delay, in minutes (seven days).
pub const MAX_MINUTES: u32 = 7 * 24 * 60;
/// The longest note, in characters.
pub const MAX_NOTE_CHARS: usize = 280;

/// One booked wake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Booking {
    /// Its id, unique within the MUD's run.
    pub id: u64,
    /// When it is due (ms since the epoch, game clock).
    pub due_ms: u64,
    /// What the agent wants to look at then, in its own words.
    pub note: String,
}

/// Why a wake was not booked.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BookError {
    /// The delay is out of range.
    #[error("minutes must be from {MIN_MINUTES} to {MAX_MINUTES}")]
    Minutes,
    /// Too many wakes wait already.
    #[error("you already have {MAX_PENDING} wakes booked; cancel one first")]
    Full,
}

#[derive(Debug, Default)]
struct Inner {
    next_id: u64,
    by_entity: BTreeMap<EntityId, Vec<Booking>>,
}

/// Every entity's booked wakes.
#[derive(Debug, Default)]
pub struct WakeBook {
    inner: Mutex<Inner>,
}

/// The note kept: control characters gone, at most [`MAX_NOTE_CHARS`].
fn clean(note: &str) -> String {
    note.chars()
        .filter(|c| !c.is_control())
        .take(MAX_NOTE_CHARS)
        .collect::<String>()
        .trim()
        .to_owned()
}

impl WakeBook {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Books a wake for `entity`, `minutes` after `now_ms`.
    ///
    /// # Errors
    /// The delay is out of range, or [`MAX_PENDING`] wakes wait already.
    pub fn book(
        &self,
        entity: &EntityId,
        now_ms: u64,
        minutes: u32,
        note: &str,
    ) -> Result<Booking, BookError> {
        if !(MIN_MINUTES..=MAX_MINUTES).contains(&minutes) {
            return Err(BookError::Minutes);
        }
        let mut inner = self.lock();
        if inner.by_entity.get(entity).map_or(0, Vec::len) >= MAX_PENDING {
            return Err(BookError::Full);
        }
        inner.next_id += 1;
        let booking = Booking {
            id: inner.next_id,
            due_ms: now_ms.saturating_add(u64::from(minutes) * 60_000),
            note: clean(note),
        };
        let list = inner.by_entity.entry(entity.clone()).or_default();
        list.push(booking.clone());
        list.sort_by_key(|b| (b.due_ms, b.id));
        drop(inner);
        Ok(booking)
    }

    /// Cancels `entity`'s booking `id`. Whether there was one.
    pub fn cancel(&self, entity: &EntityId, id: u64) -> bool {
        let mut inner = self.lock();
        let Some(list) = inner.by_entity.get_mut(entity) else {
            return false;
        };
        let before = list.len();
        list.retain(|b| b.id != id);
        let removed = list.len() < before;
        if list.is_empty() {
            inner.by_entity.remove(entity);
        }
        removed
    }

    /// `entity`'s bookings, soonest first.
    #[must_use]
    pub fn pending(&self, entity: &EntityId) -> Vec<Booking> {
        self.lock()
            .by_entity
            .get(entity)
            .cloned()
            .unwrap_or_default()
    }

    /// Every booking, by entity, for the observer.
    #[must_use]
    pub fn all(&self) -> BTreeMap<EntityId, Vec<Booking>> {
        self.lock().by_entity.clone()
    }

    /// Takes the bookings due by `now_ms` of each entity that `ready`
    /// accepts (online, and not hard-stopped). A booking of an entity that
    /// is not ready stays, and comes due again at its next chance.
    pub fn take_due(
        &self,
        now_ms: u64,
        ready: impl Fn(&EntityId) -> bool,
    ) -> Vec<(EntityId, Vec<Booking>)> {
        let mut inner = self.lock();
        let mut out = Vec::new();
        for (entity, list) in &mut inner.by_entity {
            if list.first().is_none_or(|b| b.due_ms > now_ms) || !ready(entity) {
                continue;
            }
            let due: Vec<Booking> = list
                .iter()
                .filter(|b| b.due_ms <= now_ms)
                .cloned()
                .collect();
            list.retain(|b| b.due_ms > now_ms);
            out.push((entity.clone(), due));
        }
        inner.by_entity.retain(|_, list| !list.is_empty());
        drop(inner);
        out
    }

    /// Drops every booking of `entity` (it left the world for good).
    pub fn forget(&self, entity: &EntityId) {
        self.lock().by_entity.remove(entity);
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn torin() -> EntityId {
        EntityId::from("torin")
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
        assert_eq!(due, vec![(torin(), vec![b])]);
        assert!(book.take_due(u64::MAX, |_| true).is_empty(), "once");
        assert!(book.pending(&torin()).is_empty(), "taken");
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
        assert!(
            book.book(&EntityId::from("marta"), 0, 9, "x").is_ok(),
            "per entity"
        );
        let first = book.pending(&torin())[0].id;
        assert!(book.cancel(&torin(), first));
        assert!(!book.cancel(&torin(), first), "once");
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
        let due = book.take_due(10 * 60_000, |_| true);
        assert_eq!(due, vec![(torin(), vec![soon, late])]);
        assert_eq!(book.pending(&torin()), vec![later]);
        book.forget(&torin());
        assert!(book.all().is_empty());
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
                for (_, due) in book.take_due(now, |_| true) {
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
}
