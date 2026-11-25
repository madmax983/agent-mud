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
//! - [`budget_join`], [`roll_day`], [`budget_spend`] (phase 3): a day's
//!   spend, split into what the supervisor metered and what littleguy's
//!   ledger reported. The join is a join-semilattice, a spend never lowers
//!   it, and the day never rolls back, so `Spend` upserts in any order
//!   converge and the stored budget never passes the one in memory.
//! - [`restore_budget`], [`restore_lg`], [`prime_budget`] (phase 3): the
//!   budget at boot. The ledger tally rises to what the ledger holds of
//!   today, each field to the larger value, so a restart neither bills a
//!   ledger row twice nor forgets one.

use mud_world::EntityId;
use mud_world::side::{MacroRow, Tally};

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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
// mirror-begin day_spend
/// One entity's spend on one UTC day, by where it was metered.
pub struct DaySpend {
    /// The UTC day (days since the epoch).
    pub day: u64,
    /// Spend the supervisor metered itself.
    pub local: Tally,
    /// Spend read from littleguy's ledger.
    pub lg: Tally,
}
// mirror-end day_spend

// mirror-begin tally_kernels
/// No spend.
#[must_use]
pub const fn tally_zero() -> Tally {
    Tally {
        tokens: 0,
        cost_usd_micros: 0,
        calls: 0,
    }
}

/// Each field's larger value: the join of two tallies of one day.
#[must_use]
pub const fn tally_max(a: Tally, b: Tally) -> Tally {
    Tally {
        tokens: if a.tokens > b.tokens {
            a.tokens
        } else {
            b.tokens
        },
        cost_usd_micros: if a.cost_usd_micros > b.cost_usd_micros {
            a.cost_usd_micros
        } else {
            b.cost_usd_micros
        },
        calls: if a.calls > b.calls { a.calls } else { b.calls },
    }
}

/// A tally with the spend `add` added, each field saturating.
#[must_use]
pub const fn tally_add(t: Tally, add: Tally) -> Tally {
    Tally {
        tokens: if add.tokens <= u64::MAX - t.tokens {
            t.tokens + add.tokens
        } else {
            u64::MAX
        },
        cost_usd_micros: if add.cost_usd_micros <= u64::MAX - t.cost_usd_micros {
            t.cost_usd_micros + add.cost_usd_micros
        } else {
            u64::MAX
        },
        calls: if add.calls <= u64::MAX - t.calls {
            t.calls + add.calls
        } else {
            u64::MAX
        },
    }
}
// mirror-end tally_kernels

// mirror-begin budget_join
/// The join of two dated spends: the later day wins whole; on the same
/// day, each field of each tally takes the larger value. A `Spend` upsert
/// merges this way.
#[must_use]
pub const fn budget_join(a: DaySpend, b: DaySpend) -> DaySpend {
    if a.day > b.day {
        a
    } else if b.day > a.day {
        b
    } else {
        DaySpend {
            day: a.day,
            local: tally_max(a.local, b.local),
            lg: tally_max(a.lg, b.lg),
        }
    }
}
// mirror-end budget_join

// mirror-begin roll_day
/// The spend as of UTC day `day`: zeroed on a later day, else unchanged.
/// It never rolls back: after the clock went back, the later day stays.
#[must_use]
pub const fn roll_day(s: DaySpend, day: u64) -> DaySpend {
    if day > s.day {
        DaySpend {
            day,
            local: tally_zero(),
            lg: tally_zero(),
        }
    } else {
        s
    }
}
// mirror-end roll_day

// mirror-begin budget_spend
/// Records the spend `add` on UTC day `day`, read from littleguy's ledger
/// (`ledger`) or metered by the supervisor.
#[must_use]
pub const fn budget_spend(s: DaySpend, day: u64, ledger: bool, add: Tally) -> DaySpend {
    let r = roll_day(s, day);
    if ledger {
        DaySpend {
            day: r.day,
            local: r.local,
            lg: tally_add(r.lg, add),
        }
    } else {
        DaySpend {
            day: r.day,
            local: tally_add(r.local, add),
            lg: r.lg,
        }
    }
}
// mirror-end budget_spend

// mirror-begin budget_used
/// The tokens the breaker counts: both tallies, saturating.
#[must_use]
pub const fn budget_used(s: DaySpend) -> u64 {
    if s.lg.tokens <= u64::MAX - s.local.tokens {
        s.local.tokens + s.lg.tokens
    } else {
        u64::MAX
    }
}
// mirror-end budget_used

// mirror-begin restore_lg
/// The ledger tally after a restart: each field's larger value.
///
/// The inputs are the stored tally and what the ledger itself holds of
/// today. Both are at most the truth, so the result is too: no ledger row
/// is billed twice, and a row the store missed (a crash before its
/// `Spend`) or the ledger dropped (littleguy purged it) still counts.
#[must_use]
pub const fn restore_lg(stored: Tally, ledger: Tally) -> Tally {
    tally_max(stored, ledger)
}
// mirror-end restore_lg

// mirror-begin restore_budget
/// The spend restored at boot on UTC day `today` from the stored row.
///
/// The same day stands. An earlier day is over: zeroed. One day ahead
/// (the clock went back across midnight) stands: its counters still apply
/// on the real next day, and roll the day after. More than one day ahead
/// is reset to `today`.
#[must_use]
pub const fn restore_budget(row: DaySpend, today: u64) -> DaySpend {
    if row.day < today || row.day - today > 1 {
        DaySpend {
            day: today,
            local: tally_zero(),
            lg: tally_zero(),
        }
    } else {
        row
    }
}
// mirror-end restore_budget

// mirror-begin prime_budget
/// The spend after the ledger was read at boot: on `today`, the ledger
/// tally rises to [`restore_lg`] of it and the ledger's `today`; a spend of
/// another day is left as it is.
#[must_use]
pub const fn prime_budget(s: DaySpend, today: u64, ledger: Tally) -> DaySpend {
    if s.day == today {
        DaySpend {
            day: s.day,
            local: s.local,
            lg: restore_lg(s.lg, ledger),
        }
    } else {
        s
    }
}
// mirror-end prime_budget

#[cfg(test)]
mod tests;
