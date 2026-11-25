//! Wakes that an agent books for itself (`mud_wake_me`).
//!
//! littleguy's own `wake_me` runs read-only, as a heartbeat does, so a wake
//! it books can look but never act in the world. A wake booked here is a
//! real wake: the supervisor routes it like any other (budgets, one wake
//! per entity at a time), and the mind may act. The note the agent left is
//! handed back to it, as its own words, when the wake comes.
//!
//! Bookings persist (ADR 0011). A book or a cancel is written to the world
//! store with `synchronous = FULL` before the agent is told ok; a store
//! failure answers not ok and keeps nothing. A due booking is taken *in
//! flight*, not deleted: its row goes in the transaction of the first
//! effectful command of the wake that carries it, or at that wake's close.
//! A restart restores every row, with ids that never repeat.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use mud_world::side::{BookingRow, MetaKey};
use mud_world::{EntityId, Game, SideOp};
use serde::{Deserialize, Serialize};

use crate::persist::{BookingDisposition, booking_disposition, next_booking_id};

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
    /// Its id, unique within the world store (never reused, across
    /// restarts too).
    pub id: u64,
    /// When it is due (ms since the epoch, game clock).
    pub due_ms: u64,
    /// What the agent wants to look at then, in its own words.
    pub note: String,
}

/// Why a wake was not booked, or not cancelled.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BookError {
    /// The delay is out of range.
    #[error("minutes must be from {MIN_MINUTES} to {MAX_MINUTES}")]
    Minutes,
    /// Too many wakes wait already.
    #[error("you already have {MAX_PENDING} wakes booked; cancel one first")]
    Full,
    /// The world store failed: nothing was booked or cancelled.
    #[error("the world store failed; nothing changed, try again later")]
    Store,
}

impl BookError {
    /// The stable code of the error, for tool answers.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Minutes => "minutes",
            Self::Full => "full",
            Self::Store => "store",
        }
    }
}

#[derive(Debug)]
struct Inner {
    /// The id the next booking gets.
    next_id: u64,
    by_entity: BTreeMap<EntityId, Vec<Booking>>,
    /// Bookings taken by a wake that has not consumed them yet.
    in_flight: BTreeSet<u64>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            next_id: 1,
            by_entity: BTreeMap::new(),
            in_flight: BTreeSet::new(),
        }
    }
}

/// Every entity's booked wakes.
///
/// Lock order: `writer` comes before the world (a book or a cancel writes
/// through [`Game::act`], which takes the world lock), so it is never taken
/// inside a world closure; `inner` is a supervisor lock, taken one at a
/// time and never held across a `Game` call.
#[derive(Debug, Default)]
pub struct WakeBook {
    inner: Mutex<Inner>,
    /// Serializes books and cancels around their durable write.
    writer: Mutex<()>,
    /// Where bookings persist; `None` keeps them in memory only.
    game: Option<Arc<Game>>,
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

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl WakeBook {
    /// A book that writes every booking through `game`'s store. With an
    /// in-memory world the writes are no-ops.
    #[must_use]
    pub fn persisted(game: Arc<Game>) -> Self {
        Self {
            game: Some(game),
            ..Self::default()
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        lock(&self.inner)
    }

    fn write(&self, ops: &[SideOp]) -> Result<(), BookError> {
        self.game
            .as_ref()
            .map_or(Ok(()), |g| g.act(ops).map_err(|_| BookError::Store))
    }

    /// Books a wake for `entity`, `minutes` after `now_ms`. The booking is
    /// in the store, durably, when this returns `Ok`.
    ///
    /// # Errors
    /// The delay is out of range, [`MAX_PENDING`] wakes wait already, or
    /// the store failed (nothing is kept).
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
        let _writing = lock(&self.writer);
        let id = {
            let inner = self.lock();
            if inner.by_entity.get(entity).map_or(0, Vec::len) >= MAX_PENDING {
                return Err(BookError::Full);
            }
            inner.next_id
        };
        let booking = Booking {
            id,
            due_ms: now_ms.saturating_add(u64::from(minutes) * 60_000),
            note: clean(note),
        };
        self.write(&[
            SideOp::BookingInsert {
                row: BookingRow {
                    id,
                    entity: entity.clone(),
                    due_ms: booking.due_ms,
                    note: booking.note.clone(),
                    booked_ms: now_ms,
                },
            },
            SideOp::MetaMax {
                key: MetaKey::NextBookingId,
                value: id,
            },
        ])?;
        let mut inner = self.lock();
        inner.next_id = inner.next_id.max(id.saturating_add(1));
        let list = inner.by_entity.entry(entity.clone()).or_default();
        list.push(booking.clone());
        list.sort_by_key(|b| (b.due_ms, b.id));
        drop(inner);
        Ok(booking)
    }

    /// Cancels `entity`'s booking `id`. Whether there was one.
    ///
    /// # Errors
    /// The store failed: the booking stays.
    pub fn cancel(&self, entity: &EntityId, id: u64) -> Result<bool, BookError> {
        let _writing = lock(&self.writer);
        let held = self
            .lock()
            .by_entity
            .get(entity)
            .is_some_and(|list| list.iter().any(|b| b.id == id));
        if !held {
            return Ok(false);
        }
        self.write(&[SideOp::BookingDelete { ids: vec![id] }])?;
        self.remove(entity, &[id]);
        Ok(true)
    }

    fn remove(&self, entity: &EntityId, ids: &[u64]) {
        let mut inner = self.lock();
        for id in ids {
            inner.in_flight.remove(id);
        }
        if let Some(list) = inner.by_entity.get_mut(entity) {
            list.retain(|b| !ids.contains(&b.id));
            if list.is_empty() {
                inner.by_entity.remove(entity);
            }
        }
    }

    /// `entity`'s bookings, soonest first (in flight included).
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

    /// Whether booking `id` is in flight: taken by a wake that has not
    /// consumed it yet.
    #[must_use]
    pub fn in_flight(&self, id: u64) -> bool {
        self.lock().in_flight.contains(&id)
    }

    /// Takes the bookings due by `now_ms` of each entity that `ready`
    /// accepts (online, and not hard-stopped), and marks them in flight:
    /// they stay booked until a wake consumes them ([`WakeBook::consumed`])
    /// or gives them back ([`WakeBook::requeue`]). A booking of an entity
    /// that is not ready stays, and comes due again at its next chance.
    pub fn take_due(
        &self,
        now_ms: u64,
        ready: impl Fn(&EntityId) -> bool,
    ) -> Vec<(EntityId, Vec<Booking>)> {
        let mut inner = self.lock();
        let Inner {
            by_entity,
            in_flight,
            ..
        } = &mut *inner;
        let mut out = Vec::new();
        for (entity, list) in by_entity.iter() {
            let due: Vec<Booking> = list
                .iter()
                .filter(|b| b.due_ms <= now_ms && !in_flight.contains(&b.id))
                .cloned()
                .collect();
            if due.is_empty() || !ready(entity) {
                continue;
            }
            in_flight.extend(due.iter().map(|b| b.id));
            out.push((entity.clone(), due));
        }
        drop(inner);
        out
    }

    /// Gives taken bookings back: their wake never ran (the entity went
    /// offline) or ran a mind that does not see them (the budget fallback).
    /// They come due again at the next chance.
    pub fn requeue(&self, ids: &[u64]) {
        let mut inner = self.lock();
        for id in ids {
            inner.in_flight.remove(id);
        }
    }

    /// Forgets bookings a wake carried: its turn consumes their rows.
    pub fn consumed(&self, entity: &EntityId, ids: &[u64]) {
        self.remove(entity, ids);
    }

    /// Drops every booking of `entity` in memory (it left the world for
    /// good; its rows go with the entity's).
    pub fn forget(&self, entity: &EntityId) {
        let mut inner = self.lock();
        if let Some(list) = inner.by_entity.remove(entity) {
            for b in list {
                inner.in_flight.remove(&b.id);
            }
        }
    }

    /// Restores the stored bookings at boot, with nothing in flight, and
    /// the id counter past every id issued (`meta`: the highest id the
    /// store recorded). Each is disposed of by [`booking_disposition`]: a
    /// booking not due yet waits (its due time clamped to [`MAX_MINUTES`]
    /// from now, if the clock went back); a due one fires when its entity
    /// is ready; one overdue by more than `max_late_ms` is dropped and
    /// returned, for the caller to delete and report.
    pub fn restore(
        &self,
        rows: impl IntoIterator<Item = BookingRow>,
        meta: u64,
        now_ms: u64,
        max_late_ms: u64,
    ) -> Vec<BookingRow> {
        let mut inner = self.lock();
        let mut dropped = Vec::new();
        let mut max_id = 0;
        let latest = now_ms.saturating_add(u64::from(MAX_MINUTES) * 60_000);
        for row in rows {
            max_id = max_id.max(row.id);
            match booking_disposition(row.due_ms, now_ms, max_late_ms) {
                BookingDisposition::Drop => dropped.push(row),
                BookingDisposition::Wait | BookingDisposition::Fire => {
                    let list = inner.by_entity.entry(row.entity.clone()).or_default();
                    if !list.iter().any(|b| b.id == row.id) {
                        list.push(Booking {
                            id: row.id,
                            due_ms: row.due_ms.min(latest),
                            note: row.note,
                        });
                    }
                }
            }
        }
        for list in inner.by_entity.values_mut() {
            list.sort_by_key(|b| (b.due_ms, b.id));
        }
        inner.in_flight.clear();
        inner.next_id = inner.next_id.max(next_booking_id(meta, max_id));
        drop(inner);
        dropped
    }
}

#[cfg(test)]
mod tests;
