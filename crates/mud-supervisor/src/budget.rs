//! Daily token budgets and their circuit breakers.
//!
//! At 80% of the daily budget an entity goes soft: its wake cooldowns
//! double and ambient wakes stop. At 100% it goes hard: until UTC midnight
//! a deterministic fallback answers direct interactions with canned
//! flavor, and no model is called.

use serde::{Deserialize, Serialize};

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

/// The spend of one entity on one UTC day.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    /// The daily token limit. `None` means no limit.
    pub limit: Option<u64>,
    /// The UTC day the counters belong to (days since the epoch).
    pub day: u64,
    /// Tokens spent today.
    pub tokens: u64,
    /// Cost spent today, in millionths of a dollar.
    pub cost_usd_micros: u64,
    /// Model calls today.
    pub calls: u64,
}

impl Budget {
    /// A budget with a daily limit.
    #[must_use]
    pub const fn new(limit: Option<u64>) -> Self {
        Self {
            limit,
            day: 0,
            tokens: 0,
            cost_usd_micros: 0,
            calls: 0,
        }
    }

    /// Rolls over at UTC midnight.
    pub const fn roll(&mut self, now_ms: u64) {
        let day = now_ms / DAY_MS;
        if day != self.day {
            self.day = day;
            self.tokens = 0;
            self.cost_usd_micros = 0;
            self.calls = 0;
        }
    }

    /// Records a spend.
    pub fn spend(&mut self, now_ms: u64, tokens: u64, cost_usd_micros: u64, calls: u64) {
        self.roll(now_ms);
        self.tokens = self.tokens.saturating_add(tokens);
        self.cost_usd_micros = self.cost_usd_micros.saturating_add(cost_usd_micros);
        self.calls = self.calls.saturating_add(calls);
    }

    /// The current level.
    #[must_use]
    pub fn level(&mut self, now_ms: u64) -> BudgetLevel {
        self.roll(now_ms);
        self.limit
            .map_or(BudgetLevel::Normal, |l| budget_level(self.tokens, l))
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

    #[test]
    fn the_day_rolls_over_at_midnight() {
        let mut b = Budget::new(Some(100));
        b.spend(DAY_MS + 5, 100, 7, 1);
        assert_eq!(b.level(DAY_MS + 6), BudgetLevel::Hard);
        assert_eq!(b.level(2 * DAY_MS), BudgetLevel::Normal);
        assert_eq!(b.tokens, 0);
        let mut open = Budget::new(None);
        open.spend(0, u64::MAX, 0, 1);
        assert_eq!(open.level(1), BudgetLevel::Normal);
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
