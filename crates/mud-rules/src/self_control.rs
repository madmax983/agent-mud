//! Self-control rolls for disadvantages such as greed or cowardice.

use serde::{Deserialize, Serialize};

use crate::DiceRng;

/// One self-control roll. A failure means the trait takes over: the server
/// tells the entity privately that it strongly wants to act on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfControlRoll {
    /// The three dice.
    pub dice: [u8; 3],
    /// Their sum.
    pub total: u8,
    /// The self-control number, such as 12.
    pub target_number: u8,
    /// `target_number - total`.
    pub margin: i32,
    /// Whether the entity kept control.
    pub resisted: bool,
}

/// Rolls 3d6 against a self-control number. Equal or under resists.
pub fn self_control_roll(rng: &mut dyn DiceRng, target_number: u8) -> SelfControlRoll {
    let dice = rng.roll_3d6();
    let total = dice[0] + dice[1] + dice[2];
    SelfControlRoll {
        dice,
        total,
        target_number,
        margin: i32::from(target_number) - i32::from(total),
        resisted: total <= target_number,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FixedDice;

    #[test]
    fn the_design_example_fails_greedy_twelve() {
        let r = self_control_roll(&mut FixedDice::new([5, 5, 4]), 12);
        assert_eq!(r.total, 14);
        assert_eq!(r.margin, -2);
        assert!(!r.resisted);
        assert!(self_control_roll(&mut FixedDice::new([4, 4, 4]), 12).resisted);
    }
}
