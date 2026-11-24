//! The success-roll kernel: how a 3d6 total classifies against an
//! effective skill.
//!
//! Mirrors `crates/mud-rules/src/roll.rs`. The text between the mirror
//! markers must match the Rust source; only the spec clauses differ.
//!
//! Bound: `effective` in `-100..=100`, so `t - effective` never overflows.
//! `success_roll` clamps to that range, and [`lemma_clamp_keeps_outcome`]
//! proves the clamp never changes an outcome. `total` may be any `u8`.

use vstd::prelude::*;

verus! {

/// How a success roll came out.
// mirror-begin outcome
pub enum Outcome {
    /// A critical success. An attack that crits cannot be defended.
    CriticalSuccess,
    /// A plain success.
    Success,
    /// A plain failure.
    Failure,
    /// A critical failure.
    CriticalFailure,
}
// mirror-end outcome

/// `Outcome::is_success`.
pub open spec fn is_success(o: Outcome) -> bool {
    o is CriticalSuccess || o is Success
}

/// The order of outcomes, worst first.
pub open spec fn rank(o: Outcome) -> int {
    match o {
        Outcome::CriticalFailure => 0,
        Outcome::Failure => 1,
        Outcome::Success => 2,
        Outcome::CriticalSuccess => 3,
    }
}

/// The classification over unbounded integers.
pub open spec fn spec_classify(t: int, e: int) -> Outcome {
    if t <= 4 || (t == 5 && e >= 15) || (t == 6 && e >= 16) {
        Outcome::CriticalSuccess
    } else if t >= 18 || (t == 17 && e <= 15) || t - e >= 10 {
        Outcome::CriticalFailure
    } else if t == 17 || t > e {
        Outcome::Failure
    } else {
        Outcome::Success
    }
}

/// `e` clamped to `-100..=100`, as `success_roll` does.
pub open spec fn clamp_effective(e: int) -> int {
    if e < -100 {
        -100
    } else if e > 100 {
        100
    } else {
        e
    }
}

// mirror-begin classify
/// Classifies a 3d6 `total` against the `effective` skill.
///
/// The rules, in order:
/// - 3 or 4 is always a critical success.
/// - 5 is critical at effective 15+, 6 at effective 16+.
/// - 18 is always a critical failure; 17 is one at effective 15 or less.
/// - A failure by 10 or more is a critical failure.
/// - 17 is always at least a failure, whatever the skill.
/// - Otherwise the roll succeeds when `total <= effective`.
#[must_use]
pub const fn classify(total: u8, effective: i32) -> (r: Outcome)
    requires
        -100 <= effective <= 100,
    ensures
        r == spec_classify(total as int, effective as int),
        // 3 and 4 always crit.
        total == 3 || total == 4 ==> r is CriticalSuccess,
        // 18 always fumbles.
        total == 18 ==> r is CriticalFailure,
        // 17 never succeeds.
        total == 17 ==> !is_success(r),
        // Away from the edges, the roll succeeds exactly when it is at most
        // the skill.
        7 <= total <= 16 ==> (is_success(r) <==> total as int <= effective as int),
{
    let t = total as i32;
    if t <= 4 || (t == 5 && effective >= 15) || (t == 6 && effective >= 16) {
        Outcome::CriticalSuccess
    } else if t >= 18 || (t == 17 && effective <= 15) || t - effective >= 10 {
        Outcome::CriticalFailure
    } else if t == 17 || t > effective {
        Outcome::Failure
    } else {
        Outcome::Success
    }
}
// mirror-end classify

/// More skill never hurts: raising the effective skill never lowers the
/// rank of the outcome, for any total.
pub proof fn lemma_classify_monotonic(t: int, e1: int, e2: int)
    requires
        e1 <= e2,
    ensures
        rank(spec_classify(t, e1)) <= rank(spec_classify(t, e2)),
{
}

/// The same, for the executable kernel.
pub proof fn lemma_classify_exec_monotonic(total: u8, e1: i32, e2: i32)
    requires
        -100 <= e1 <= e2 <= 100,
    ensures
        rank(spec_classify(total as int, e1 as int)) <= rank(
            spec_classify(total as int, e2 as int),
        ),
{
    lemma_classify_monotonic(total as int, e1 as int, e2 as int);
}

/// Clamping the skill to `-100..=100` never changes the outcome of any
/// `u8` total, so `success_roll` may clamp before it classifies.
pub proof fn lemma_clamp_keeps_outcome(t: int, e: int)
    requires
        0 <= t <= 255,
    ensures
        spec_classify(t, clamp_effective(e)) == spec_classify(t, e),
{
}

} // verus!
