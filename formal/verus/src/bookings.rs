//! Booked wakes: the limits of `mud_wake_me` (ADR 0008).
//!
//! Mirrors `crates/mud-supervisor/src/wakes.rs`. The text between the
//! mirror markers must match the Rust source; only the spec clauses differ.
//! The constants sit in a mirrored region too, so the limits proved here
//! are the limits the server ships.
//!
//! - `book_verdict`: total. `Ok` exactly when the delay is within
//!   `MIN_MINUTES..=MAX_MINUTES`, fewer than `MAX_PENDING` bookings wait
//!   and fewer than `MAX_PER_DAY` were made today; otherwise the first
//!   failed check names the refusal, in the order minutes, pending, day.
//! - The day model: the count of one entity's bookings on one game-clock
//!   day, folded over any sequence of asks (each with any delay and any
//!   pending count), where only an `Ok` ask counts and a cancel never
//!   lowers it. The count never passes `MAX_PER_DAY`
//!   (`lemma_a_day_admits_at_most_max_per_day`), and once it reaches it no
//!   further ask of that day is admitted (`lemma_a_full_day_admits_nothing`).
//!
//! No arithmetic in the kernel: comparisons only.

use vstd::prelude::*;

verus! {

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

pub open spec fn spec_book_verdict(minutes: u32, pending: u32, booked_today: u32) -> BookVerdict {
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

// mirror-begin book_verdict
/// Whether an entity with `pending` bookings waiting, that made
/// `booked_today` bookings today, may book one `minutes` ahead.
#[must_use]
pub const fn book_verdict(minutes: u32, pending: u32, booked_today: u32) -> (r: BookVerdict)
    ensures
        r == spec_book_verdict(minutes, pending, booked_today),
        (r is Ok) <==> (MIN_MINUTES <= minutes && minutes <= MAX_MINUTES && pending
            < MAX_PENDING && booked_today < MAX_PER_DAY),
        (r is BadMinutes) <==> (minutes < MIN_MINUTES || minutes > MAX_MINUTES),
        (r is Full) <==> (MIN_MINUTES <= minutes && minutes <= MAX_MINUTES && pending
            >= MAX_PENDING),
        (r is DailyCap) <==> (MIN_MINUTES <= minutes && minutes <= MAX_MINUTES && pending
            < MAX_PENDING && booked_today >= MAX_PER_DAY),
{
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

/// The limits are what ADR 0008 states: one minute to a week, five
/// waiting, 24 a day.
pub proof fn lemma_the_limits()
    ensures
        MIN_MINUTES == 1,
        MAX_MINUTES == 10080,
        MAX_PENDING == 5,
        MAX_PER_DAY == 24,
{
}

/// One step of an entity's day: a booking ask (its delay and the bookings
/// waiting then), or a cancel. A cancel never refunds the day.
pub enum DayStep {
    Ask(u32, u32),
    Cancel,
}

/// The day's count after `steps`, from 0: an ask that `book_verdict`
/// admits adds one; anything else leaves it.
pub open spec fn day_count(steps: Seq<DayStep>) -> u32
    decreases steps.len(),
{
    if steps.len() == 0 {
        0
    } else {
        let c = day_count(steps.drop_last());
        match steps.last() {
            DayStep::Ask(minutes, pending) => {
                if spec_book_verdict(minutes, pending, c) is Ok {
                    (c + 1) as u32
                } else {
                    c
                }
            },
            DayStep::Cancel => c,
        }
    }
}

/// The asks of `steps` that were admitted.
pub open spec fn admitted(steps: Seq<DayStep>) -> nat
    decreases steps.len(),
{
    if steps.len() == 0 {
        0
    } else {
        let c = day_count(steps.drop_last());
        match steps.last() {
            DayStep::Ask(minutes, pending) => {
                if spec_book_verdict(minutes, pending, c) is Ok {
                    admitted(steps.drop_last()) + 1
                } else {
                    admitted(steps.drop_last())
                }
            },
            DayStep::Cancel => admitted(steps.drop_last()),
        }
    }
}

/// Whatever is asked and cancelled in a day, at most `MAX_PER_DAY`
/// bookings are admitted, and the count is exactly the admitted asks.
pub proof fn lemma_a_day_admits_at_most_max_per_day(steps: Seq<DayStep>)
    ensures
        day_count(steps) <= MAX_PER_DAY,
        day_count(steps) == admitted(steps),
    decreases steps.len(),
{
    if steps.len() > 0 {
        lemma_a_day_admits_at_most_max_per_day(steps.drop_last());
    }
}

/// A day that reached `MAX_PER_DAY` admits no further ask, whatever its
/// delay and however many bookings were cancelled meanwhile.
pub proof fn lemma_a_full_day_admits_nothing(steps: Seq<DayStep>, minutes: u32, pending: u32)
    requires
        day_count(steps) == MAX_PER_DAY,
    ensures
        !(spec_book_verdict(minutes, pending, day_count(steps)) is Ok),
        day_count(steps.push(DayStep::Ask(minutes, pending))) == MAX_PER_DAY,
        day_count(steps.push(DayStep::Cancel)) == MAX_PER_DAY,
{
    assert(steps.push(DayStep::Ask(minutes, pending)).drop_last() =~= steps);
    assert(steps.push(DayStep::Cancel).drop_last() =~= steps);
}

} // verus!
