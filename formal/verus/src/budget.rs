//! The budget breaker kernel: the level of a daily token budget.
//!
//! Mirrors `crates/mud-supervisor/src/budget.rs`. The text between the
//! mirror markers must match the Rust source; only the spec clauses differ.
//!
//! No bound: the 80% test runs in `u128`, so it never overflows.

use vstd::prelude::*;

verus! {

/// How much of its budget an entity may still spend.
// mirror-begin budget_level_enum
pub enum BudgetLevel {
    /// Under 80%.
    Normal,
    /// 80% or more: double cooldowns, no ambient wakes.
    Soft,
    /// 100% or more: deterministic fallback only.
    Hard,
}
// mirror-end budget_level_enum

/// The derived `Ord` on `BudgetLevel`: declaration order.
pub open spec fn rank(l: BudgetLevel) -> int {
    match l {
        BudgetLevel::Normal => 0,
        BudgetLevel::Soft => 1,
        BudgetLevel::Hard => 2,
    }
}

/// The level over unbounded integers.
pub open spec fn spec_level(used: int, limit: int) -> BudgetLevel {
    if limit == 0 || used >= limit {
        BudgetLevel::Hard
    } else if used * 5 >= limit * 4 {
        BudgetLevel::Soft
    } else {
        BudgetLevel::Normal
    }
}

// mirror-begin budget_level
/// The level for `used` tokens of a `limit` (`limit` above 0).
#[must_use]
pub const fn budget_level(used: u64, limit: u64) -> (r: BudgetLevel)
    ensures
        r == spec_level(used as int, limit as int),
        (r is Hard) <==> (limit == 0 || used >= limit),
        (r is Soft) <==> !(limit == 0 || used >= limit) && used as int * 5 >= limit as int * 4,
        (r is Normal) <==> !(limit == 0 || used >= limit) && used as int * 5 < limit as int * 4,
{
    if limit == 0 || used >= limit {
        BudgetLevel::Hard
    } else if (used as u128) * 5 >= (limit as u128) * 4 {
        BudgetLevel::Soft
    } else {
        BudgetLevel::Normal
    }
}
// mirror-end budget_level

/// Spending more never lowers the level.
pub proof fn lemma_level_monotonic(used1: int, used2: int, limit: int)
    requires
        0 <= used1 <= used2,
        0 <= limit,
    ensures
        rank(spec_level(used1, limit)) <= rank(spec_level(used2, limit)),
{
}

} // verus!
