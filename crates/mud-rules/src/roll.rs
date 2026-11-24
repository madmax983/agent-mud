//! The success roll: 3d6 against an effective skill.

use serde::{Deserialize, Serialize};

use crate::DiceRng;

/// How a success roll came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

impl Outcome {
    /// Whether the roll succeeded, critically or not.
    #[must_use]
    pub const fn is_success(self) -> bool {
        matches!(self, Self::CriticalSuccess | Self::Success)
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
pub const fn classify(total: u8, effective: i32) -> Outcome {
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

/// One rolled success check, with everything a replay or a log needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuccessRoll {
    /// The three dice.
    pub dice: [u8; 3],
    /// Their sum.
    pub total: u8,
    /// The effective skill rolled against, after modifiers.
    pub target: i32,
    /// `target - total`. Positive is a margin of success.
    pub margin: i32,
    /// The classified outcome.
    pub outcome: Outcome,
}

/// Rolls 3d6 against `effective`.
pub fn success_roll(rng: &mut dyn DiceRng, effective: i32) -> SuccessRoll {
    let dice = rng.roll_3d6();
    let total = dice[0] + dice[1] + dice[2];
    SuccessRoll {
        dice,
        total,
        target: effective,
        margin: effective - i32::from(total),
        outcome: classify(total, effective),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FixedDice;
    use proptest::prelude::*;

    #[test]
    fn criticals_follow_the_table() {
        assert_eq!(classify(3, -5), Outcome::CriticalSuccess);
        assert_eq!(classify(4, 3), Outcome::CriticalSuccess);
        assert_eq!(classify(5, 14), Outcome::Success);
        assert_eq!(classify(5, 15), Outcome::CriticalSuccess);
        assert_eq!(classify(6, 15), Outcome::Success);
        assert_eq!(classify(6, 16), Outcome::CriticalSuccess);
        assert_eq!(classify(17, 15), Outcome::CriticalFailure);
        assert_eq!(classify(17, 16), Outcome::Failure);
        assert_eq!(classify(17, 30), Outcome::Failure);
        assert_eq!(classify(18, 30), Outcome::CriticalFailure);
        assert_eq!(classify(14, 4), Outcome::CriticalFailure);
        assert_eq!(classify(13, 4), Outcome::Failure);
    }

    #[test]
    fn a_roll_records_dice_and_margin() {
        let mut d = FixedDice::new([5, 5, 4]);
        let r = success_roll(&mut d, 12);
        assert_eq!(r.dice, [5, 5, 4]);
        assert_eq!(r.total, 14);
        assert_eq!(r.margin, -2);
        assert_eq!(r.outcome, Outcome::Failure);
    }

    proptest! {
        #[test]
        fn success_iff_total_at_most_skill_away_from_the_edges(total in 7u8..=16, eff in -10i32..30) {
            let o = classify(total, eff);
            prop_assert_eq!(o.is_success(), i32::from(total) <= eff);
        }

        #[test]
        fn more_skill_never_hurts(total in 3u8..=18, eff in -10i32..30) {
            let rank = |o: Outcome| match o {
                Outcome::CriticalFailure => 0,
                Outcome::Failure => 1,
                Outcome::Success => 2,
                Outcome::CriticalSuccess => 3,
            };
            prop_assert!(rank(classify(total, eff + 1)) >= rank(classify(total, eff)));
        }
    }
}
