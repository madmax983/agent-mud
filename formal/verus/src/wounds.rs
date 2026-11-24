//! The wound-state kernel: how hurt an entity is at `hp` out of `hp_max`.
//!
//! Mirrors `crates/mud-rules/src/stats.rs`. The text between the mirror
//! markers must match the Rust source; only the spec clauses differ.
//!
//! Bound: `hp <= i32::MAX / 3` (`HP_LIMIT`) and `hp_max <= i32::MAX / 5`
//! (`HP_MAX_LIMIT`), so `3 * hp` and `-5 * max` fit an `i32`. `hp` is
//! unbounded below. `Pools::wound_state` clamps to these limits.

use vstd::prelude::*;

verus! {

/// How hurt an entity is.
// mirror-begin wound_state_enum
pub enum WoundState {
    /// At least a third of HP left.
    Healthy,
    /// Below a third of HP: Dodge is halved.
    Staggered,
    /// At 0 HP or below: an HT roll on each hit to stay conscious.
    Reeling,
    /// At -HP or below: an HT roll on each hit to stay alive.
    Dying,
    /// At -5×HP or below.
    Dead,
}
// mirror-end wound_state_enum

/// The order of the derived `Ord` on `WoundState`: declaration order, so
/// a higher rank is a worse state.
pub open spec fn rank(s: WoundState) -> int {
    match s {
        WoundState::Healthy => 0,
        WoundState::Staggered => 1,
        WoundState::Reeling => 2,
        WoundState::Dying => 3,
        WoundState::Dead => 4,
    }
}

/// The maximum the kernel uses: below 1 counts as 1.
pub open spec fn eff_max(hp_max: int) -> int {
    if hp_max < 1 {
        1
    } else {
        hp_max
    }
}

/// The thresholds, over unbounded integers, for a maximum `max >= 1`.
pub open spec fn spec_wound(hp: int, max: int) -> WoundState {
    if hp <= -5 * max {
        WoundState::Dead
    } else if hp <= -max {
        WoundState::Dying
    } else if hp <= 0 {
        WoundState::Reeling
    } else if 3 * hp < max {
        WoundState::Staggered
    } else {
        WoundState::Healthy
    }
}

// mirror-begin wound_state
/// The wound state for `hp` out of `hp_max` (`hp_max` is at least 1).
#[must_use]
pub const fn wound_state(hp: i32, hp_max: i32) -> (r: WoundState)
    requires
        hp <= i32::MAX / 3,
        hp_max <= i32::MAX / 5,
    ensures
        r == spec_wound(hp as int, eff_max(hp_max as int)),
        // The thresholds, exactly as coded.
        (r is Dead) <==> hp <= -5 * eff_max(hp_max as int),
        (r is Dying) <==> -5 * eff_max(hp_max as int) < hp <= -eff_max(hp_max as int),
        (r is Reeling) <==> -eff_max(hp_max as int) < hp <= 0,
        (r is Staggered) <==> 0 < hp && 3 * hp < eff_max(hp_max as int),
        (r is Healthy) <==> 0 < hp && 3 * hp >= eff_max(hp_max as int),
{
    let max = if hp_max < 1 { 1 } else { hp_max };
    if hp <= -5 * max {
        WoundState::Dead
    } else if hp <= -max {
        WoundState::Dying
    } else if hp <= 0 {
        WoundState::Reeling
    } else if 3 * hp < max {
        WoundState::Staggered
    } else {
        WoundState::Healthy
    }
}
// mirror-end wound_state

/// Losing HP never improves the state: at a fixed maximum, lower HP gives
/// a state at least as bad.
pub proof fn lemma_wound_monotonic(hp1: int, hp2: int, max: int)
    requires
        max >= 1,
        hp1 <= hp2,
    ensures
        rank(spec_wound(hp1, max)) >= rank(spec_wound(hp2, max)),
{
}

/// The same for the executable kernel, for any `hp_max` (below 1 counts
/// as 1).
pub proof fn lemma_wound_exec_monotonic(hp1: i32, hp2: i32, hp_max: i32)
    requires
        hp1 <= hp2,
    ensures
        rank(spec_wound(hp1 as int, eff_max(hp_max as int))) >= rank(
            spec_wound(hp2 as int, eff_max(hp_max as int)),
        ),
{
    lemma_wound_monotonic(hp1 as int, hp2 as int, eff_max(hp_max as int));
}

} // verus!
