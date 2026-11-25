//! Supervisor persistence kernels (ADR 0011, phase 2): what a wake
//! consumes, what a restart restores, and how a deterministic plan keeps
//! its place.
//!
//! The kernels between the mirror markers are proved in
//! `formal/verus/src/persist.rs`:
//!
//! - [`macro_open`]: the macro tick number a wake carries and the queued
//!   ticks it covers. Each tick number is carried by at most one committed
//!   consume, so a script's `nth = 1` runs for one tick only.
//! - [`next_booking_id`]: strictly above every id issued before a restart.
//! - [`booking_disposition`]: wait, fire or drop a booking restored at boot.
//! - [`restore_macro`]: the next macro tick after a restart; a past one
//!   fires once, however long the outage.
//! - [`plan_step`] and [`plan_resume`]: the durable cursor of a plan moves
//!   past a line only with that line's effect, so after any crash a resume
//!   never repeats an effect and never skips a line.
//! - [`wake_id_key`]: the parts of a wake id, unique across restarts and
//!   world resets.

use mud_world::EntityId;
use mud_world::side::MacroRow;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
// mirror-begin macro_open_struct
/// What a wake that opens now consumes of its entity's macro counters.
pub struct MacroOpen {
    /// The tick number the wake carries; 0 when it carries none.
    pub macro_no: u32,
    /// The queued ticks it covers: `cleared` once it is consumed.
    pub cover: u32,
}
// mirror-end macro_open_struct

// mirror-begin macro_open
/// Opens a wake on `row`.
///
/// A wake that carries a macro tick, with a tick
/// owed (`queued > cleared`), carries number `ticks + 1` and covers every
/// queued tick. Any other wake carries no number and covers only what is
/// cleared already, so consuming it changes nothing. At `u32::MAX` ticks
/// no number is left: the owed tick is covered without one.
#[must_use]
pub const fn macro_open(row: MacroRow, carries_tick: bool) -> MacroOpen {
    if carries_tick && row.queued > row.cleared {
        MacroOpen {
            macro_no: if row.ticks < u32::MAX {
                row.ticks + 1
            } else {
                0
            },
            cover: row.queued,
        }
    } else {
        MacroOpen {
            macro_no: 0,
            cover: row.cleared,
        }
    }
}
// mirror-end macro_open

// mirror-begin next_booking_id
/// The next booking id after a restart: one more than the larger of the
/// highest id the meta row recorded and the highest id still stored.
#[must_use]
pub const fn next_booking_id(meta: u64, max_id: u64) -> u64 {
    let m = if meta > max_id { meta } else { max_id };
    if m < u64::MAX { m + 1 } else { u64::MAX }
}
// mirror-end next_booking_id

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// mirror-begin booking_disposition_enum
/// What a booking restored at boot does.
pub enum BookingDisposition {
    /// Not due yet: it waits.
    Wait,
    /// Due, and not too late: it fires when its entity is ready.
    Fire,
    /// Overdue by more than the allowed lateness: it is dropped.
    Drop,
}
// mirror-end booking_disposition_enum

// mirror-begin booking_disposition
/// The disposition of a booking due at `due_ms`, restored at `now_ms`,
/// that may fire at most `max_late_ms` late.
#[must_use]
pub const fn booking_disposition(due_ms: u64, now_ms: u64, max_late_ms: u64) -> BookingDisposition {
    if now_ms < due_ms {
        BookingDisposition::Wait
    } else if now_ms - due_ms <= max_late_ms {
        BookingDisposition::Fire
    } else {
        BookingDisposition::Drop
    }
}
// mirror-end booking_disposition

// mirror-begin restore_macro
/// The next macro tick after a restart, from the stored time `next_ms`.
///
/// A time from `now_ms` to `now_ms + hi_ms` stands. A later one (the clock
/// went back) is clamped to `now_ms + hi_ms`. A past one fires once, at
/// `now_ms + pick_ms`, a pick within the catch-up spread, however many
/// ticks the outage missed.
#[must_use]
pub const fn restore_macro(next_ms: u64, now_ms: u64, hi_ms: u64, pick_ms: u64) -> u64 {
    if next_ms < now_ms {
        if pick_ms <= u64::MAX - now_ms {
            now_ms + pick_ms
        } else {
            u64::MAX
        }
    } else if next_ms - now_ms <= hi_ms {
        next_ms
    } else {
        now_ms + hi_ms
    }
}
// mirror-end restore_macro

// mirror-begin plan_step
/// The durable cursor of a plan after line `line` ran from `cursor`.
///
/// It moves past the line when the line took effect: its cursor commits
/// in the same transaction. It stays when the line changed nothing, which
/// commits nothing.
#[must_use]
pub const fn plan_step(cursor: u32, line: u32, effectful: bool) -> u32 {
    if effectful && line >= cursor && line < u32::MAX {
        line + 1
    } else {
        cursor
    }
}
// mirror-end plan_step

// mirror-begin plan_resume
/// The first line a resumed plan runs: its durable cursor, at most `len`.
/// A plan whose cursor reached `len` is done.
#[must_use]
pub const fn plan_resume(cursor: u32, len: u32) -> u32 {
    if cursor < len { cursor } else { len }
}
// mirror-end plan_resume

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
// mirror-begin wake_key
/// What makes a wake id unique: the `world.db` epoch, the boot of the
/// process and a counter within the boot.
pub struct WakeKey {
    /// The epoch, minted once per `world.db`.
    pub epoch: u64,
    /// The boot, one more at every open of the store.
    pub boot: u64,
    /// The wake's number within the boot.
    pub n: u64,
}
// mirror-end wake_key

// mirror-begin wake_id_key
/// The key of wake `n` of boot `boot` of a world of epoch `epoch`.
#[must_use]
pub const fn wake_id_key(epoch: u64, boot: u64, n: u64) -> WakeKey {
    WakeKey { epoch, boot, n }
}
// mirror-end wake_id_key

impl WakeKey {
    /// The wake id of this key for `entity`:
    /// `wake-{epoch:016x}-{boot}-{entity}-{n}`. The epoch has a fixed
    /// width and boot and `n` are digits only, so [`WakeKey::parse`] reads
    /// any id back, whatever the entity's name holds.
    #[must_use]
    pub fn id(self, entity: &EntityId) -> String {
        format!("wake-{:016x}-{}-{entity}-{}", self.epoch, self.boot, self.n)
    }

    /// The key and entity of a wake id made by [`WakeKey::id`].
    #[must_use]
    pub fn parse(id: &str) -> Option<(Self, EntityId)> {
        let rest = id.strip_prefix("wake-")?;
        let epoch = u64::from_str_radix(rest.get(..16)?, 16).ok()?;
        let rest = rest.get(16..)?.strip_prefix('-')?;
        let (boot, rest) = rest.split_once('-')?;
        let (entity, n) = rest.rsplit_once('-')?;
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if !digits(boot) || !digits(n) || entity.is_empty() {
            return None;
        }
        Some((
            wake_id_key(epoch, boot.parse().ok()?, n.parse().ok()?),
            EntityId::new(entity),
        ))
    }
}

#[cfg(test)]
mod tests;
