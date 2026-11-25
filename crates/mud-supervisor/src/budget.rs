//! Daily token budgets and their circuit breakers.
//!
//! At 80% of the daily budget an entity goes soft: its wake cooldowns
//! double and ambient wakes stop. At 100% it goes hard: until UTC midnight
//! a deterministic fallback answers direct interactions with canned
//! flavor, and no model is called.

use mud_world::side::Tally;
use serde::{Deserialize, Serialize};

use crate::persist::{
    DaySpend, budget_spend, budget_used, prime_budget, restore_budget, roll_day, tally_zero,
};

/// How much of its budget an entity may still spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

// mirror-begin budget_level
/// The level for `used` tokens of a `limit` (`limit` above 0).
#[must_use]
pub const fn budget_level(used: u64, limit: u64) -> BudgetLevel {
    if limit == 0 || used >= limit {
        BudgetLevel::Hard
    } else if (used as u128) * 5 >= (limit as u128) * 4 {
        BudgetLevel::Soft
    } else {
        BudgetLevel::Normal
    }
}
// mirror-end budget_level

/// Milliseconds in a day.
pub const DAY_MS: u64 = 86_400_000;

/// Where a spend was metered, which says which tally of the day it joins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpendSource {
    /// The supervisor metered it: a reply's usage, a harvest agent's
    /// token count, a plugin-agent client.
    Local,
    /// littleguy's own cost ledger reported it.
    Ledger,
}

/// The spend of one entity on one UTC day.
///
/// It is split by source (ADR 0011): what the supervisor metered itself
/// (`local`) and what littleguy's ledger reported (`lg`). The breaker
/// counts both. The day only moves forward, and every change is a
/// [`crate::persist`] kernel, so the row the store keeps is never above
/// this one and a restart restores it exactly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Budget {
    /// The daily token limit. `None` means no limit.
    pub limit: Option<u64>,
    /// Today's spend, by source.
    pub spend: DaySpend,
}

impl Budget {
    /// A budget with a daily limit.
    #[must_use]
    pub const fn new(limit: Option<u64>) -> Self {
        Self {
            limit,
            spend: DaySpend {
                day: 0,
                local: tally_zero(),
                lg: tally_zero(),
            },
        }
    }

    /// Rolls over at UTC midnight. Only forward: a clock that went back
    /// keeps the later day's counters, which roll the day after it.
    pub const fn roll(&mut self, now_ms: u64) {
        self.spend = roll_day(self.spend, now_ms / DAY_MS);
    }

    /// Records a spend from `source`.
    pub const fn spend(&mut self, now_ms: u64, source: SpendSource, add: Tally) {
        self.spend = budget_spend(
            self.spend,
            now_ms / DAY_MS,
            matches!(source, SpendSource::Ledger),
            add,
        );
    }

    /// Tokens spent today, both sources.
    #[must_use]
    pub const fn tokens(&self) -> u64 {
        budget_used(self.spend)
    }

    /// Cost spent today, in millionths of a dollar, both sources.
    #[must_use]
    pub const fn cost_usd_micros(&self) -> u64 {
        self.spend
            .local
            .cost_usd_micros
            .saturating_add(self.spend.lg.cost_usd_micros)
    }

    /// Model calls today, both sources.
    #[must_use]
    pub const fn calls(&self) -> u64 {
        self.spend.local.calls.saturating_add(self.spend.lg.calls)
    }

    /// The current level.
    #[must_use]
    pub fn level(&mut self, now_ms: u64) -> BudgetLevel {
        self.roll(now_ms);
        self.limit
            .map_or(BudgetLevel::Normal, |l| budget_level(self.tokens(), l))
    }

    /// Restores the stored row at boot ([`restore_budget`]). Returns
    /// whether a row more than a day ahead of the clock was reset (the
    /// caller warns).
    pub const fn restore(&mut self, row: DaySpend, now_ms: u64) -> bool {
        let today = now_ms / DAY_MS;
        self.spend = restore_budget(row, today);
        row.day > today && self.spend.day != row.day
    }

    /// Raises the ledger tally to what the ledger holds of today
    /// ([`prime_budget`]), at boot.
    pub const fn prime(&mut self, now_ms: u64, ledger: Tally) {
        self.spend = prime_budget(self.spend, now_ms / DAY_MS, ledger);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn thresholds() {
        assert_eq!(budget_level(0, 1000), BudgetLevel::Normal);
        assert_eq!(budget_level(799, 1000), BudgetLevel::Normal);
        assert_eq!(budget_level(800, 1000), BudgetLevel::Soft);
        assert_eq!(budget_level(999, 1000), BudgetLevel::Soft);
        assert_eq!(budget_level(1000, 1000), BudgetLevel::Hard);
        assert_eq!(budget_level(5, 0), BudgetLevel::Hard);
    }

    const fn tally(tokens: u64, cost_usd_micros: u64, calls: u64) -> Tally {
        Tally {
            tokens,
            cost_usd_micros,
            calls,
        }
    }

    #[test]
    fn the_day_rolls_over_at_midnight() {
        let mut b = Budget::new(Some(100));
        b.spend(DAY_MS + 5, SpendSource::Local, tally(60, 7, 1));
        b.spend(DAY_MS + 5, SpendSource::Ledger, tally(40, 3, 2));
        assert_eq!((b.tokens(), b.cost_usd_micros(), b.calls()), (100, 10, 3));
        assert_eq!(b.level(DAY_MS + 6), BudgetLevel::Hard, "both sources count");
        assert_eq!(b.level(2 * DAY_MS), BudgetLevel::Normal);
        assert_eq!(b.tokens(), 0);
        let mut open = Budget::new(None);
        open.spend(0, SpendSource::Local, tally(u64::MAX, 0, 1));
        assert_eq!(open.level(1), BudgetLevel::Normal);
    }

    #[test]
    fn a_clock_that_goes_back_keeps_the_later_day() {
        let mut b = Budget::new(Some(100));
        b.spend(2 * DAY_MS + 5, SpendSource::Local, tally(100, 0, 1));
        assert_eq!(b.level(DAY_MS + 5), BudgetLevel::Hard, "not reset");
        assert_eq!(b.level(2 * DAY_MS + 9), BudgetLevel::Hard);
        assert_eq!(
            b.level(3 * DAY_MS),
            BudgetLevel::Normal,
            "rolls on the day after"
        );
    }

    #[test]
    fn a_restore_far_ahead_is_reported_and_reset() {
        let row = DaySpend {
            day: 10,
            local: tally(5, 0, 1),
            lg: tally_zero(),
        };
        let mut b = Budget::new(Some(10));
        assert!(!b.restore(row, 10 * DAY_MS), "the same day");
        assert_eq!(b.spend, row);
        assert!(!b.restore(row, 9 * DAY_MS), "one day ahead stands");
        assert!(b.restore(row, 8 * DAY_MS), "two days ahead resets");
        assert_eq!(b.tokens(), 0);
        assert!(!b.restore(row, 11 * DAY_MS), "an earlier day is just over");
        b.prime(11 * DAY_MS, tally(7, 1, 1));
        assert_eq!(b.spend.lg, tally(7, 1, 1));
    }

    proptest! {
        #[test]
        fn more_spend_never_lowers_the_level(used in 0u64..1_000_000, limit in 1u64..1_000_000) {
            prop_assert!(budget_level(used + 1, limit) >= budget_level(used, limit));
            let soft = used * 5 >= limit * 4;
            let level = budget_level(used, limit);
            prop_assert_eq!(level == BudgetLevel::Hard, used >= limit);
            if used < limit {
                prop_assert_eq!(level == BudgetLevel::Soft, soft);
            }
        }
    }
}
