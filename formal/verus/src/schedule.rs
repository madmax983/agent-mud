//! The schedule kernel: a minute of the day shifted by an offset.
//!
//! Mirrors `crates/mud-world/src/schedule.rs`. The text between the mirror
//! markers must match the Rust source; only the spec clauses differ.
//!
//! No bound: every `u32` minute and `i32` offset is allowed, because the
//! sum runs in `i64`.

use vstd::prelude::*;

verus! {

// mirror-begin day_minutes
/// Minutes in a day.
pub const DAY_MINUTES: u32 = 24 * 60;
// mirror-end day_minutes

/// The shifted minute, over unbounded integers. Verus's `%` on `int` is
/// Euclidean: for a positive divisor it is always in `0..1440`.
pub open spec fn spec_shift(minute: int, offset: int) -> int {
    (minute + offset) % 1440
}

// mirror-begin shift_minute
/// A minute of the day moved by `offset`, wrapping around midnight.
///
/// The result is always in `0..DAY_MINUTES`, and shifting back by
/// `-offset` restores any minute of the day (proved in `formal/verus`).
#[must_use]
pub const fn shift_minute(minute: u32, offset: i32) -> (r: u32)
    ensures
        r == spec_shift(minute as int, offset as int),
        r < 1440,
{
    let day = DAY_MINUTES as i64;
    let sum = minute as i64 + offset as i64;
    // `%` keeps the sign of `sum`, so a negative remainder is lifted by one
    // day. This is `sum.rem_euclid(day)`, written so Verus can check it.
    let rem = sum % day;
    let m = if rem < 0 { rem + day } else { rem };
    // The wrapped minute is in 0..1440, so the cast is exact.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "0..1440"
    )]
    let out = m as u32;
    out
}
// mirror-end shift_minute

/// Shifting by `k` and then by `-k` returns any minute of the day, for any
/// integer `k`.
pub proof fn lemma_shift_round_trip(m: int, k: int)
    requires
        0 <= m < 1440,
    ensures
        spec_shift(spec_shift(m, k), -k) == m,
{
    assert(spec_shift(spec_shift(m, k), -k) == m) by (nonlinear_arith)
        requires
            0 <= m < 1440,
    ;
}

/// The round trip through the executable kernel: `shift_minute(m, k)`
/// then `shift_minute(_, -k)` gives back `m`. (`-k` needs
/// `k > i32::MIN` to be an `i32`.)
pub fn shift_round_trip(m: u32, k: i32)
    requires
        m < 1440,
        k > i32::MIN,
{
    let a = shift_minute(m, k);
    let b = shift_minute(a, -k);
    proof {
        lemma_shift_round_trip(m as int, k as int);
    }
    assert(b == m);
}

} // verus!
