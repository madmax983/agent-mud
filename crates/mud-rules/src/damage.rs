//! Damage dice, the strength table, and injury after armor.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::DiceRng;

/// The type of a hit. It sets the wounding multiplier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
// mirror-begin damage_type
pub enum DamageType {
    /// Blunt force: ×1.
    Crushing,
    /// Edges: ×1.5.
    Cutting,
    /// Points: ×2.
    Impaling,
    /// Fire and acid: ×1.
    Burn,
}
// mirror-end damage_type

impl DamageType {
    /// The short tag used in world packs and logs.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Crushing => "cr",
            Self::Cutting => "cut",
            Self::Impaling => "imp",
            Self::Burn => "burn",
        }
    }

    /// Parses a tag such as `cut` or `crushing`.
    #[must_use]
    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag.trim().to_ascii_lowercase().as_str() {
            "cr" | "crush" | "crushing" => Some(Self::Crushing),
            "cut" | "cutting" => Some(Self::Cutting),
            "imp" | "impaling" => Some(Self::Impaling),
            "burn" | "fire" => Some(Self::Burn),
            _ => None,
        }
    }
}

/// The bound on `raw` and `dr` that the Verus proof of [`injury`] assumes:
/// both in `-INJURY_LIMIT..=INJURY_LIMIT`, so nothing overflows. Callers
/// clamp to it. Honest dice (faces 1 to 6, at most 255 of them) roll at
/// most 1657, and any DR above the limit stops such a roll either way.
pub const INJURY_LIMIT: i32 = 10_000;

// mirror-begin injury
/// The injury of a hit: penetration after DR, times the wounding
/// multiplier, rounded down. Never negative.
///
/// A hit that penetrates at all deals at least 1 injury.
#[must_use]
pub const fn injury(raw: i32, dr: i32, kind: DamageType) -> i32 {
    let pen = if raw > dr { raw - dr } else { 0 };
    if pen == 0 {
        return 0;
    }
    let scaled = match kind {
        DamageType::Crushing | DamageType::Burn => pen,
        DamageType::Cutting => pen * 3 / 2,
        DamageType::Impaling => pen * 2,
    };
    if scaled < 1 { 1 } else { scaled }
}
// mirror-end injury

/// A dice expression such as `2d+1` or `1d-2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DamageDice {
    /// The number of dice.
    pub dice: u8,
    /// The flat modifier.
    pub adds: i8,
}

impl DamageDice {
    /// A dice expression.
    #[must_use]
    pub const fn new(dice: u8, adds: i8) -> Self {
        Self { dice, adds }
    }

    /// Adds `n` to the modifier, saturating.
    #[must_use]
    pub const fn plus(self, n: i8) -> Self {
        Self {
            dice: self.dice,
            adds: self.adds.saturating_add(n),
        }
    }

    /// Rolls the expression. Basic damage is never below 0; for a cutting,
    /// impaling or burn attack it is never below 1.
    pub fn roll(self, rng: &mut dyn DiceRng, kind: DamageType) -> i32 {
        let mut total = i32::from(self.adds);
        for _ in 0..self.dice {
            total += i32::from(rng.d6());
        }
        let floor = i32::from(!matches!(kind, DamageType::Crushing));
        total.max(floor)
    }
}

impl fmt::Display for DamageDice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.adds {
            0 => write!(f, "{}d", self.dice),
            a if a > 0 => write!(f, "{}d+{a}", self.dice),
            a => write!(f, "{}d{a}", self.dice),
        }
    }
}

/// A dice expression that did not parse.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("not a dice expression: {0:?} (expected like 1d+2)")]
pub struct ParseDamageError(pub String);

impl FromStr for DamageDice {
    type Err = ParseDamageError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseDamageError(s.to_owned());
        let t = s.trim().to_ascii_lowercase();
        let (count, rest) = t.split_once('d').ok_or_else(err)?;
        let dice: u8 = if count.is_empty() {
            1
        } else {
            count.parse().map_err(|_| err())?
        };
        let adds: i8 = if rest.is_empty() {
            0
        } else {
            rest.trim_start_matches('+').parse().map_err(|_| err())?
        };
        if dice == 0 || dice > 20 {
            return Err(err());
        }
        Ok(Self { dice, adds })
    }
}

/// Thrust and swing damage by ST, from 1 to 20.
const TABLE: [(DamageDice, DamageDice); 20] = [
    (DamageDice::new(1, -6), DamageDice::new(1, -5)),
    (DamageDice::new(1, -6), DamageDice::new(1, -5)),
    (DamageDice::new(1, -5), DamageDice::new(1, -4)),
    (DamageDice::new(1, -5), DamageDice::new(1, -4)),
    (DamageDice::new(1, -4), DamageDice::new(1, -3)),
    (DamageDice::new(1, -4), DamageDice::new(1, -3)),
    (DamageDice::new(1, -3), DamageDice::new(1, -2)),
    (DamageDice::new(1, -3), DamageDice::new(1, -2)),
    (DamageDice::new(1, -2), DamageDice::new(1, -1)),
    (DamageDice::new(1, -2), DamageDice::new(1, 0)),
    (DamageDice::new(1, -1), DamageDice::new(1, 1)),
    (DamageDice::new(1, -1), DamageDice::new(1, 2)),
    (DamageDice::new(1, 0), DamageDice::new(2, -1)),
    (DamageDice::new(1, 0), DamageDice::new(2, 0)),
    (DamageDice::new(1, 1), DamageDice::new(2, 1)),
    (DamageDice::new(1, 1), DamageDice::new(2, 2)),
    (DamageDice::new(1, 2), DamageDice::new(3, -1)),
    (DamageDice::new(1, 2), DamageDice::new(3, 0)),
    (DamageDice::new(2, -1), DamageDice::new(3, 1)),
    (DamageDice::new(2, -1), DamageDice::new(3, 2)),
];

const fn row(st: i32) -> usize {
    let clamped = if st < 1 {
        1
    } else if st > 20 {
        20
    } else {
        st
    };
    clamped.unsigned_abs() as usize - 1
}

/// Thrust damage for `st`. ST outside 1..=20 clamps.
#[must_use]
pub const fn thrust(st: i32) -> DamageDice {
    TABLE[row(st)].0
}

/// Swing damage for `st`. ST outside 1..=20 clamps.
#[must_use]
pub const fn swing(st: i32) -> DamageDice {
    TABLE[row(st)].1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FixedDice;
    use proptest::prelude::*;

    #[test]
    fn multipliers_apply_after_dr() {
        assert_eq!(injury(7, 2, DamageType::Crushing), 5);
        assert_eq!(injury(7, 2, DamageType::Cutting), 7);
        assert_eq!(injury(7, 2, DamageType::Impaling), 10);
        assert_eq!(injury(7, 2, DamageType::Burn), 5);
        assert_eq!(injury(2, 2, DamageType::Impaling), 0);
        assert_eq!(injury(1, 5, DamageType::Cutting), 0);
    }

    #[test]
    fn dice_parse_and_print() {
        assert_eq!("1d+2".parse::<DamageDice>().unwrap(), DamageDice::new(1, 2));
        assert_eq!(
            "2d-1".parse::<DamageDice>().unwrap(),
            DamageDice::new(2, -1)
        );
        assert_eq!("3d".parse::<DamageDice>().unwrap(), DamageDice::new(3, 0));
        assert_eq!("d+1".parse::<DamageDice>().unwrap(), DamageDice::new(1, 1));
        assert!("x".parse::<DamageDice>().is_err());
        assert!("0d".parse::<DamageDice>().is_err());
        assert_eq!(DamageDice::new(2, -1).to_string(), "2d-1");
        assert_eq!(DamageDice::new(1, 2).to_string(), "1d+2");
        assert_eq!(DamageDice::new(3, 0).to_string(), "3d");
    }

    #[test]
    fn rolls_respect_type_floor() {
        let mut d = FixedDice::new([1, 1]);
        assert_eq!(DamageDice::new(1, -4).roll(&mut d, DamageType::Crushing), 0);
        assert_eq!(DamageDice::new(1, -4).roll(&mut d, DamageType::Cutting), 1);
    }

    #[test]
    fn strength_table_matches_reference_rows() {
        assert_eq!(thrust(10), DamageDice::new(1, -2));
        assert_eq!(swing(10), DamageDice::new(1, 0));
        assert_eq!(swing(13), DamageDice::new(2, -1));
        assert_eq!(thrust(0), thrust(1));
        assert_eq!(swing(99), swing(20));
    }

    #[test]
    fn tags_round_trip() {
        for k in [
            DamageType::Crushing,
            DamageType::Cutting,
            DamageType::Impaling,
            DamageType::Burn,
        ] {
            assert_eq!(DamageType::from_tag(k.tag()), Some(k));
        }
        assert_eq!(DamageType::from_tag("nope"), None);
    }

    proptest! {
        #[test]
        fn injury_is_never_negative_and_grows_with_raw(raw in -50i32..200, dr in 0i32..50) {
            for k in [DamageType::Crushing, DamageType::Cutting, DamageType::Impaling, DamageType::Burn] {
                let i = injury(raw, dr, k);
                prop_assert!(i >= 0);
                prop_assert!(injury(raw + 1, dr, k) >= i);
                prop_assert_eq!(i > 0, raw > dr);
            }
        }
    }
}
