//! Attributes, pools, wound states and active defenses.

use serde::{Deserialize, Serialize};

/// The four basic attributes. 10 is average.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attributes {
    /// Strength: HP and damage.
    pub st: i32,
    /// Dexterity: most physical skills.
    pub dx: i32,
    /// Intelligence: most mental skills.
    pub iq: i32,
    /// Health: FP and staying conscious.
    pub ht: i32,
}

impl Default for Attributes {
    fn default() -> Self {
        Self {
            st: 10,
            dx: 10,
            iq: 10,
            ht: 10,
        }
    }
}

impl Attributes {
    /// The value of an attribute by its short name (`st`, `dx`, `iq`, `ht`,
    /// or `per`/`will`, which follow IQ).
    #[must_use]
    pub fn get(&self, name: &str) -> Option<i32> {
        match name.to_ascii_lowercase().as_str() {
            "st" => Some(self.st),
            "dx" => Some(self.dx),
            "iq" | "per" | "will" => Some(self.iq),
            "ht" => Some(self.ht),
            _ => None,
        }
    }
}

/// Hit points and fatigue points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pools {
    /// Current HP. Goes negative when badly hurt.
    pub hp: i32,
    /// Maximum HP, from ST.
    pub hp_max: i32,
    /// Current FP.
    pub fp: i32,
    /// Maximum FP, from HT.
    pub fp_max: i32,
}

impl Pools {
    /// Full pools for `attrs`.
    #[must_use]
    pub const fn full(attrs: &Attributes) -> Self {
        let hp = if attrs.st < 1 { 1 } else { attrs.st };
        let fp = if attrs.ht < 1 { 1 } else { attrs.ht };
        Self {
            hp,
            hp_max: hp,
            fp,
            fp_max: fp,
        }
    }

    /// The current wound state.
    ///
    /// HP above [`HP_LIMIT`] and a maximum above [`HP_MAX_LIMIT`] are clamped
    /// first, as the Verus proof of [`wound_state`] assumes. Only inputs on
    /// which the unclamped kernel would overflow are affected.
    #[must_use]
    pub const fn wound_state(&self) -> WoundState {
        let hp = if self.hp > HP_LIMIT {
            HP_LIMIT
        } else {
            self.hp
        };
        let hp_max = if self.hp_max > HP_MAX_LIMIT {
            HP_MAX_LIMIT
        } else {
            self.hp_max
        };
        wound_state(hp, hp_max)
    }
}

/// How hurt an entity is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

/// The largest HP that [`wound_state`] is proved for: `3 * hp` fits an
/// `i32`. Any HP at or above it is Healthy for every allowed maximum.
pub const HP_LIMIT: i32 = i32::MAX / 3;

/// The largest maximum HP that [`wound_state`] is proved for: `-5 * max`
/// fits an `i32`.
pub const HP_MAX_LIMIT: i32 = i32::MAX / 5;

// mirror-begin wound_state
/// The wound state for `hp` out of `hp_max` (`hp_max` is at least 1).
#[must_use]
pub const fn wound_state(hp: i32, hp_max: i32) -> WoundState {
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

/// Basic Dodge: `(DX + HT) / 4 + 3`, halved (rounding up) when staggered,
/// and 0 when reeling or worse.
#[must_use]
pub const fn basic_dodge(attrs: &Attributes, state: WoundState) -> i32 {
    let base = (attrs.dx + attrs.ht) / 4 + 3;
    match state {
        WoundState::Healthy => base,
        WoundState::Staggered => (base + 1) / 2,
        WoundState::Reeling | WoundState::Dying | WoundState::Dead => 0,
    }
}

/// Parry or Block from a weapon or shield skill: `skill / 2 + 3`.
#[must_use]
pub const fn defense_from_skill(skill: i32) -> i32 {
    skill / 2 + 3
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn thresholds() {
        assert_eq!(wound_state(10, 10), WoundState::Healthy);
        assert_eq!(wound_state(4, 12), WoundState::Healthy);
        assert_eq!(wound_state(3, 12), WoundState::Staggered);
        assert_eq!(wound_state(0, 12), WoundState::Reeling);
        assert_eq!(wound_state(-12, 12), WoundState::Dying);
        assert_eq!(wound_state(-60, 12), WoundState::Dead);
        assert_eq!(wound_state(1, 0), WoundState::Healthy);
        let huge = Pools {
            hp: i32::MAX,
            hp_max: i32::MAX,
            fp: 1,
            fp_max: 1,
        };
        assert_eq!(huge.wound_state(), WoundState::Healthy);
    }

    #[test]
    fn dodge_halves_when_staggered() {
        let a = Attributes {
            dx: 12,
            ht: 11,
            ..Attributes::default()
        };
        assert_eq!(basic_dodge(&a, WoundState::Healthy), 8);
        assert_eq!(basic_dodge(&a, WoundState::Staggered), 4);
        assert_eq!(basic_dodge(&a, WoundState::Reeling), 0);
        assert_eq!(defense_from_skill(13), 9);
    }

    #[test]
    fn attribute_lookup_and_pools() {
        let a = Attributes {
            st: 13,
            ht: 12,
            ..Attributes::default()
        };
        assert_eq!(a.get("ST"), Some(13));
        assert_eq!(a.get("will"), Some(10));
        assert_eq!(a.get("xx"), None);
        let p = Pools::full(&a);
        assert_eq!((p.hp, p.hp_max, p.fp, p.fp_max), (13, 13, 12, 12));
        assert_eq!(p.wound_state(), WoundState::Healthy);
    }

    proptest! {
        #[test]
        fn losing_hp_never_improves_the_state(hp in -200i32..50, max in 1i32..40) {
            prop_assert!(wound_state(hp - 1, max) >= wound_state(hp, max));
        }

        #[test]
        fn the_pool_clamp_changes_nothing_in_range(hp in -1_000_000i32..1_000_000, max in -5i32..100_000) {
            let p = Pools { hp, hp_max: max, fp: 10, fp_max: 10 };
            prop_assert_eq!(p.wound_state(), wound_state(hp, max));
        }
    }
}
