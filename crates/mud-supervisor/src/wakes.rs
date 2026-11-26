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
//!
//! The limits are a proved kernel (`formal/verus/src/bookings.rs`): the
//! constants and [`book_verdict`] between the mirror markers. A booking is
//! made only within one minute to a week, with fewer than [`MAX_PENDING`]
//! waiting and fewer than [`MAX_PER_DAY`] made on the same game-clock day.
//! The day's count commits with the booking (`SideOp::Booked`, joined in
//! the store by the proved `dated_max`), survives a restart, and a cancel
//! never lowers it. Only a Persistent or Turing entity with a mind may
//! book (`Supervisor::can_book`); every other caller is answered
//! `no_wakes`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use mud_world::side::{BookingRow, MetaKey};
use mud_world::{EntityId, Game, SideOp};
use serde::{Deserialize, Serialize};

use tokio::sync::broadcast;

use crate::held::{Held, assert_none_held};
use crate::persist::{BookingDisposition, booking_disposition, next_booking_id};
use crate::runtime::ObserverEvent;

// mirror-begin book_consts
/// The shortest booking delay, in minutes.
pub const MIN_MINUTES: u32 = 1;
/// The longest booking delay, in minutes (a week).
pub const MAX_MINUTES: u32 = 7 * 24 * 60;
/// Bookings one entity may hold at once.
pub const MAX_PENDING: u32 = 5;
/// Bookings one entity may make in one game-clock day.
pub const MAX_PER_DAY: u32 = 24;
// mirror-end book_consts

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// mirror-begin book_verdict_enum
/// Whether a booking may be made, and if not, why.
pub enum BookVerdict {
    /// It may.
    Ok,
    /// The delay is out of range.
    BadMinutes,
    /// `MAX_PENDING` bookings wait already.
    Full,
    /// `MAX_PER_DAY` bookings were made today already.
    DailyCap,
}
// mirror-end book_verdict_enum

// mirror-begin book_verdict
/// Whether an entity with `pending` bookings waiting, that made
/// `booked_today` bookings today, may book one `minutes` ahead.
#[must_use]
pub const fn book_verdict(minutes: u32, pending: u32, booked_today: u32) -> BookVerdict {
    if minutes < MIN_MINUTES || minutes > MAX_MINUTES {
        BookVerdict::BadMinutes
    } else if pending >= MAX_PENDING {
        BookVerdict::Full
    } else if booked_today >= MAX_PER_DAY {
        BookVerdict::DailyCap
    } else {
        BookVerdict::Ok
    }
}
// mirror-end book_verdict

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

/// What happened to a booking, for the observer feed
/// ([`crate::runtime::ObserverEvent::Booking`]). The note never goes with
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BookingAction {
    /// It was booked.
    Booked,
    /// It came due and a wake carries it: its mind sees the note.
    Fired,
    /// It came due, but its entity is not ready (offline, out cold, at its
    /// hard cap, away for a spell, or held by the anti-chatter rule). It
    /// stays, and fires when the entity is ready. Reported once per hold.
    Held,
    /// It was overdue by more than `booking_max_late_ms` and was dropped.
    Dropped,
    /// Its agent cancelled it.
    Cancelled,
    /// A wake took it but never showed it to a mind (its entity went
    /// offline, or the budget fallback ran): it comes due again.
    Requeued,
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
    /// The entity booked its share of wakes for today.
    #[error("you have booked {MAX_PER_DAY} wakes today; book again tomorrow")]
    DailyCap,
    /// The entity may not book wakes at all. The text names no reason: a
    /// reason would tell the agent what kind of entity it is.
    #[error("booking wakes is not open to you")]
    NoWakes,
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
            Self::DailyCap => "daily_cap",
            Self::NoWakes => "no_wakes",
            Self::Store => "store",
        }
    }
}

/// What one pass of [`WakeBook::take_due`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DuePass {
    /// The bookings taken, in flight now, by entity, soonest first.
    pub taken: Vec<(EntityId, Vec<Booking>)>,
    /// The due bookings of entities that are not ready, newly held in this
    /// pass (each is reported once per hold).
    pub held: Vec<(EntityId, Booking)>,
    /// The bookings dropped as too late.
    pub dropped: Vec<(EntityId, Booking)>,
}

/// The UTC day of `now_ms` on the game clock.
const fn day_of(now_ms: u64) -> u64 {
    now_ms / crate::budget::DAY_MS
}

#[derive(Debug)]
struct Inner {
    /// The id the next booking gets.
    next_id: u64,
    by_entity: BTreeMap<EntityId, Vec<Booking>>,
    /// Bookings taken by a wake that has not consumed them yet.
    in_flight: BTreeSet<u64>,
    /// Each entity's bookings on its latest day: `(day, count)`. Restored
    /// from the store at boot; a cancel never lowers it.
    today: BTreeMap<EntityId, (u64, u32)>,
    /// Held bookings already reported in their current hold.
    held_reported: BTreeSet<u64>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            next_id: 1,
            by_entity: BTreeMap::new(),
            in_flight: BTreeSet::new(),
            today: BTreeMap::new(),
            held_reported: BTreeSet::new(),
        }
    }
}

/// Every entity's booked wakes.
///
/// Lock order: `writer` comes before the world (a book or a cancel writes
/// through [`Game::act`], which takes the world lock), so it is never taken
/// inside a world closure; `inner` is a supervisor lock, taken one at a
/// time and never held across a `Game` call.
///
/// The book is the only place that writes a booking row (`BookingInsert`,
/// `BookingDelete`) or a day count (`Booked`): no other module builds
/// those ops.
#[derive(Debug, Default)]
pub struct WakeBook {
    inner: Mutex<Inner>,
    /// Serializes books and cancels around their durable write.
    writer: Mutex<()>,
    /// Where bookings persist; `None` keeps them in memory only.
    game: Option<Arc<Game>>,
    /// Where each booking's fate is published.
    observer: Option<broadcast::Sender<Arc<ObserverEvent>>>,
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

/// The `writer` lock: it comes before the world, so it is not one of the
/// supervisor locks that [`assert_none_held`] counts.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Where a booking's rows go.
enum Lane<'a> {
    /// Now, durably ([`Game::act`]): the agent is told ok only after it.
    Act,
    /// On the entity's open turn of this wake: with its first effectful
    /// command, or at its close.
    Turn(&'a str),
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

    /// The same book, publishing each booking's fate on `observer`.
    #[must_use]
    pub fn with_observer(mut self, observer: broadcast::Sender<Arc<ObserverEvent>>) -> Self {
        self.observer = Some(observer);
        self
    }

    fn lock(&self) -> Held<'_, Inner> {
        crate::held::lock(&self.inner)
    }

    fn write(&self, ops: &[SideOp]) -> Result<(), BookError> {
        assert_none_held("a booking's write");
        self.game
            .as_ref()
            .map_or(Ok(()), |g| g.act(ops).map_err(|_| BookError::Store))
    }

    fn publish(&self, entity: &EntityId, b: &Booking, action: BookingAction) {
        if let Some(tx) = &self.observer {
            let _ = tx.send(Arc::new(ObserverEvent::Booking {
                entity: entity.clone(),
                id: b.id,
                action,
                due_ms: b.due_ms,
            }));
        }
    }

    /// Books a wake for `entity`, `minutes` after `now_ms`. The booking is
    /// in the store, durably, when this returns `Ok`.
    ///
    /// # Errors
    /// The delay is out of range, [`MAX_PENDING`] wakes wait already,
    /// [`MAX_PER_DAY`] were booked today already, or the store failed
    /// (nothing is kept).
    pub fn book(
        &self,
        entity: &EntityId,
        now_ms: u64,
        minutes: u32,
        note: &str,
    ) -> Result<Booking, BookError> {
        self.book_through(&Lane::Act, entity, now_ms, minutes, note)
    }

    /// Books a wake as [`WakeBook::book`] does, for a deterministic mind's
    /// decision: the booking's row is staged on `entity`'s open turn of
    /// `wake_id`, so it is in the store with the decision's first effect
    /// or at the turn's close. A crash before either loses the decision
    /// and the booking together, and the redelivered wake books again
    /// once. The id is reserved in the store at once, so it never
    /// repeats. With no such turn (a mind run outside a wake) the booking
    /// is written now.
    ///
    /// # Errors
    /// As [`WakeBook::book`].
    pub fn book_on_turn(
        &self,
        entity: &EntityId,
        wake_id: &str,
        now_ms: u64,
        minutes: u32,
        note: &str,
    ) -> Result<Booking, BookError> {
        self.book_through(&Lane::Turn(wake_id), entity, now_ms, minutes, note)
    }

    fn book_through(
        &self,
        lane: &Lane<'_>,
        entity: &EntityId,
        now_ms: u64,
        minutes: u32,
        note: &str,
    ) -> Result<Booking, BookError> {
        let _writing = lock(&self.writer);
        let day = day_of(now_ms);
        let (id, day, count) = {
            let inner = self.lock();
            let pending = inner.by_entity.get(entity).map_or(0, Vec::len);
            // A day stored ahead of the clock (a clock set back) counts as
            // today: the cap never reopens by moving the clock.
            let (day, count) = match inner.today.get(entity) {
                Some((d, c)) if *d >= day => (*d, *c),
                _ => (day, 0),
            };
            match book_verdict(minutes, u32::try_from(pending).unwrap_or(u32::MAX), count) {
                BookVerdict::Ok => {}
                BookVerdict::BadMinutes => return Err(BookError::Minutes),
                BookVerdict::Full => return Err(BookError::Full),
                BookVerdict::DailyCap => return Err(BookError::DailyCap),
            }
            (inner.next_id, day, count.saturating_add(1))
        };
        let booking = Booking {
            id,
            due_ms: now_ms.saturating_add(u64::from(minutes) * 60_000),
            note: clean(note),
        };
        // The day count goes with the booking, in the same write: a
        // booking the store kept is always counted.
        let ops = vec![
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
            SideOp::Booked {
                entity: entity.clone(),
                day,
                count,
            },
        ];
        if let (Lane::Turn(_), Some(game)) = (lane, &self.game) {
            // The id is reserved now, whatever becomes of the turn.
            game.persist(vec![SideOp::MetaMax {
                key: MetaKey::NextBookingId,
                value: id,
            }]);
        }
        // A turn that is gone (the mind runs outside a wake) stages
        // nothing: the rows are written now, as a plain book's are.
        let staged = match (lane, &self.game) {
            (Lane::Turn(wake_id), Some(game)) => game.stage(entity, wake_id, ops.clone()).is_ok(),
            _ => false,
        };
        if !staged {
            self.write(&ops)?;
        }
        let mut inner = self.lock();
        inner.next_id = inner.next_id.max(id.saturating_add(1));
        inner.today.insert(entity.clone(), (day, count));
        let list = inner.by_entity.entry(entity.clone()).or_default();
        list.push(booking.clone());
        list.sort_by_key(|b| (b.due_ms, b.id));
        drop(inner);
        self.publish(entity, &booking, BookingAction::Booked);
        Ok(booking)
    }

    /// The bookings `entity` made on the day of `now_ms` (a cancel does
    /// not lower it).
    #[must_use]
    pub fn booked_today(&self, entity: &EntityId, now_ms: u64) -> u32 {
        let day = day_of(now_ms);
        self.lock()
            .today
            .get(entity)
            .filter(|(d, _)| *d >= day)
            .map_or(0, |(_, c)| *c)
    }

    /// Cancels `entity`'s booking `id`. Whether there was one. The day's
    /// count stays: a cancel does not give a booking back.
    ///
    /// # Errors
    /// The store failed: the booking stays.
    pub fn cancel(&self, entity: &EntityId, id: u64) -> Result<bool, BookError> {
        let _writing = lock(&self.writer);
        let held = self
            .lock()
            .by_entity
            .get(entity)
            .and_then(|list| list.iter().find(|b| b.id == id).cloned());
        let Some(booking) = held else {
            return Ok(false);
        };
        self.write(&[SideOp::BookingDelete { ids: vec![id] }])?;
        self.remove(entity, &[id]);
        self.publish(entity, &booking, BookingAction::Cancelled);
        Ok(true)
    }

    /// Removes `ids` of `entity` from memory and returns the bookings
    /// removed.
    fn remove(&self, entity: &EntityId, ids: &[u64]) -> Vec<Booking> {
        let mut inner = self.lock();
        for id in ids {
            inner.in_flight.remove(id);
            inner.held_reported.remove(id);
        }
        let mut gone = Vec::new();
        if let Some(list) = inner.by_entity.get_mut(entity) {
            list.retain(|b| {
                let keep = !ids.contains(&b.id);
                if !keep {
                    gone.push(b.clone());
                }
                keep
            });
            if list.is_empty() {
                inner.by_entity.remove(entity);
            }
        }
        drop(inner);
        gone
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

    /// One pass over the due bookings at `now_ms`.
    ///
    /// A booking overdue by more than `max_late_ms` is dropped
    /// ([`booking_disposition`] says `Drop`), as the boot drops it
    /// ([`WakeBook::restore`]), so a live server and a restarted one agree:
    /// its row is deleted through [`Game::persist`] (a lost delete is
    /// redone at the next boot). The bookings due of each entity that
    /// `ready` accepts are taken and marked in flight: they stay booked
    /// until a wake consumes them ([`WakeBook::consumed`]) or gives them
    /// back ([`WakeBook::requeue`]). A due booking of an entity that is not
    /// ready stays, and comes due again at its next chance; it is reported
    /// held once per hold, until it is taken.
    pub fn take_due(
        &self,
        now_ms: u64,
        max_late_ms: u64,
        ready: impl Fn(&EntityId) -> bool,
    ) -> DuePass {
        let mut pass = DuePass::default();
        let mut inner = self.lock();
        let Inner {
            by_entity,
            in_flight,
            held_reported,
            ..
        } = &mut *inner;
        for (entity, list) in by_entity.iter_mut() {
            list.retain(|b| {
                let late = !in_flight.contains(&b.id)
                    && booking_disposition(b.due_ms, now_ms, max_late_ms)
                        == BookingDisposition::Drop;
                if late {
                    held_reported.remove(&b.id);
                    pass.dropped.push((entity.clone(), b.clone()));
                }
                !late
            });
            let due: Vec<Booking> = list
                .iter()
                .filter(|b| b.due_ms <= now_ms && !in_flight.contains(&b.id))
                .cloned()
                .collect();
            if due.is_empty() {
                continue;
            }
            if !ready(entity) {
                for b in due {
                    if held_reported.insert(b.id) {
                        pass.held.push((entity.clone(), b));
                    }
                }
                continue;
            }
            for b in &due {
                in_flight.insert(b.id);
                held_reported.remove(&b.id);
            }
            pass.taken.push((entity.clone(), due));
        }
        by_entity.retain(|_, list| !list.is_empty());
        drop(inner);
        if !pass.dropped.is_empty()
            && let Some(game) = &self.game
        {
            game.persist(vec![SideOp::BookingDelete {
                ids: pass.dropped.iter().map(|(_, b)| b.id).collect(),
            }]);
        }
        for (e, b) in &pass.dropped {
            // The note is the agent's own text: it stays out of the log.
            tracing::info!(entity = %e, id = b.id, due_ms = b.due_ms, now_ms, "a booked wake was too late and was dropped");
            self.publish(e, b, BookingAction::Dropped);
        }
        for (e, b) in &pass.held {
            self.publish(e, b, BookingAction::Held);
        }
        pass
    }

    /// Gives taken bookings back: their wake never ran (the entity went
    /// offline) or ran a mind that does not see them (the budget fallback).
    /// They come due again at the next chance.
    pub fn requeue(&self, ids: &[u64]) {
        let mut inner = self.lock();
        let mut back = Vec::new();
        for id in ids {
            if inner.in_flight.remove(id) {
                back.push(*id);
            }
        }
        let found: Vec<(EntityId, Booking)> = inner
            .by_entity
            .iter()
            .flat_map(|(e, list)| {
                list.iter()
                    .filter(|b| back.contains(&b.id))
                    .map(move |b| (e.clone(), b.clone()))
            })
            .collect();
        drop(inner);
        for (e, b) in &found {
            self.publish(e, b, BookingAction::Requeued);
        }
    }

    /// Forgets bookings a wake carried: its turn consumes their rows, and
    /// its mind sees their notes (they fired).
    pub fn consumed(&self, entity: &EntityId, ids: &[u64]) {
        for b in self.remove(entity, ids) {
            self.publish(entity, &b, BookingAction::Fired);
        }
    }

    /// Drops every booking of `entity` in memory (it left the world for
    /// good; its rows go with the entity's).
    pub fn forget(&self, entity: &EntityId) {
        let mut inner = self.lock();
        if let Some(list) = inner.by_entity.remove(entity) {
            for b in list {
                inner.in_flight.remove(&b.id);
                inner.held_reported.remove(&b.id);
            }
        }
    }

    /// Restores each entity's day count at boot, from its stored
    /// `(booked_day, booked_today)`, joined with what memory holds (the
    /// later day wins; on one day the larger count).
    pub fn restore_days(&self, days: impl IntoIterator<Item = (EntityId, u64, u32)>) {
        let mut inner = self.lock();
        for (entity, day, count) in days {
            let slot = inner.today.entry(entity).or_insert((day, count));
            *slot = match slot.0.cmp(&day) {
                std::cmp::Ordering::Less => (day, count),
                std::cmp::Ordering::Equal => (day, slot.1.max(count)),
                std::cmp::Ordering::Greater => *slot,
            };
        }
        drop(inner);
    }

    /// Restores the stored bookings at boot, with nothing in flight, and
    /// the id counter past every id issued (`meta`: the highest id the
    /// store recorded). Each is disposed of by [`booking_disposition`]: a
    /// booking not due yet waits (its due time clamped to [`MAX_MINUTES`]
    /// from now, if the clock went back); a due one fires when its entity
    /// is ready; one overdue by more than `max_late_ms` is dropped and
    /// returned, for the caller to report, with the op that deletes the
    /// dropped rows (`None` when nothing was dropped).
    pub fn restore(
        &self,
        rows: impl IntoIterator<Item = BookingRow>,
        meta: u64,
        now_ms: u64,
        max_late_ms: u64,
    ) -> (Vec<BookingRow>, Option<SideOp>) {
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
        inner.held_reported.clear();
        inner.next_id = inner.next_id.max(next_booking_id(meta, max_id));
        drop(inner);
        let op = (!dropped.is_empty()).then(|| SideOp::BookingDelete {
            ids: dropped.iter().map(|b| b.id).collect(),
        });
        (dropped, op)
    }
}

#[cfg(test)]
mod tests;
