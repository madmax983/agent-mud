//! The limits a pack sets in `scripts.toml` `[limits]`.
//!
//! They are pack data because they change outcomes: a script that runs out
//! of operations faults, and a faulting hook applies nothing.

use mud_world::hooks::{HookLimits, MAX_EFFECTS_CEILING};
use serde::{Deserialize, Serialize};

/// The `[limits]` table as written; every key is optional.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsDef {
    /// Rhai operations per call.
    pub max_operations: Option<i64>,
    /// Effects per hook call.
    pub max_effects: Option<i64>,
    /// Lines per mind wake.
    pub max_lines_per_wake: Option<i64>,
    /// Consecutive faults before a breaker trips.
    pub max_consecutive_faults: Option<i64>,
}

/// The checked limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Rhai operations per call, and the work units per call.
    pub max_operations: u64,
    /// Effects per hook call (at most 16, which the admission proof needs).
    pub max_effects: u8,
    /// Lines per mind wake.
    pub max_lines_per_wake: u8,
    /// Consecutive faults before a breaker trips.
    pub max_consecutive_faults: u8,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_operations: 10_000,
            max_effects: 8,
            max_lines_per_wake: 4,
            max_consecutive_faults: 3,
        }
    }
}

impl Limits {
    /// The limits the engine's breakers and admission use.
    #[must_use]
    pub const fn hook_limits(&self) -> HookLimits {
        HookLimits {
            max_effects: self.max_effects,
            max_faults: self.max_consecutive_faults,
        }
    }
}

fn ranged(
    key: &str,
    v: Option<i64>,
    lo: i64,
    hi: i64,
    default: u64,
    problems: &mut Vec<String>,
) -> u64 {
    match v {
        None => default,
        Some(n) if (lo..=hi).contains(&n) => u64::try_from(n).unwrap_or(default),
        Some(n) => {
            problems.push(format!(
                "scripts.toml limits.{key} = {n} is outside {lo}..={hi}"
            ));
            default
        }
    }
}

impl LimitsDef {
    /// Checks the ranges. Each problem is added to `problems`.
    #[must_use]
    pub fn check(&self, problems: &mut Vec<String>) -> Limits {
        let d = Limits::default();
        let small = |n: u64, fallback: u8| u8::try_from(n).unwrap_or(fallback);
        Limits {
            max_operations: ranged(
                "max_operations",
                self.max_operations,
                1_000,
                20_000,
                d.max_operations,
                problems,
            ),
            max_effects: small(
                ranged(
                    "max_effects",
                    self.max_effects,
                    1,
                    i64::from(MAX_EFFECTS_CEILING),
                    u64::from(d.max_effects),
                    problems,
                ),
                d.max_effects,
            ),
            max_lines_per_wake: small(
                ranged(
                    "max_lines_per_wake",
                    self.max_lines_per_wake,
                    1,
                    8,
                    u64::from(d.max_lines_per_wake),
                    problems,
                ),
                d.max_lines_per_wake,
            ),
            max_consecutive_faults: small(
                ranged(
                    "max_consecutive_faults",
                    self.max_consecutive_faults,
                    1,
                    10,
                    u64::from(d.max_consecutive_faults),
                    problems,
                ),
                d.max_consecutive_faults,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_validate_their_ranges() {
        let mut p = Vec::new();
        assert_eq!(LimitsDef::default().check(&mut p), Limits::default());
        assert_eq!(p, Vec::<String>::new());
        let bad = LimitsDef {
            max_operations: Some(999),
            max_effects: Some(17),
            max_lines_per_wake: Some(0),
            max_consecutive_faults: Some(11),
        };
        let l = bad.check(&mut p);
        assert_eq!(l, Limits::default());
        assert_eq!(
            p,
            [
                "scripts.toml limits.max_operations = 999 is outside 1000..=20000",
                "scripts.toml limits.max_effects = 17 is outside 1..=16",
                "scripts.toml limits.max_lines_per_wake = 0 is outside 1..=8",
                "scripts.toml limits.max_consecutive_faults = 11 is outside 1..=10",
            ]
        );
        let good = LimitsDef {
            max_operations: Some(20_000),
            max_effects: Some(16),
            max_lines_per_wake: Some(8),
            max_consecutive_faults: Some(1),
        };
        let mut none = Vec::new();
        let l = good.check(&mut none);
        assert_eq!(none, Vec::<String>::new());
        assert_eq!(l.max_operations, 20_000);
        assert_eq!(l.hook_limits().max_effects, 16);
        assert_eq!(l.hook_limits().max_faults, 1);
    }
}
