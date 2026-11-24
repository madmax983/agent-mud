//! Dice sources. All randomness in the MUD flows through [`DiceRng`].

use std::collections::VecDeque;

/// A source of six-sided dice.
///
/// The engine never calls a global random number generator. A test fixes
/// the dice with [`FixedDice`]; the server uses [`SeededDice`], whose state
/// it can store and replay.
pub trait DiceRng: Send {
    /// One die, from 1 to 6.
    fn d6(&mut self) -> u8;

    /// Three dice, in roll order.
    fn roll_3d6(&mut self) -> [u8; 3] {
        [self.d6(), self.d6(), self.d6()]
    }

    /// A number from 0 to `n - 1`. `n` of 0 gives 0.
    ///
    /// The default builds it from dice, so a fixed source stays fixed.
    fn below(&mut self, n: u32) -> u32 {
        if n <= 1 {
            return 0;
        }
        let mut acc: u64 = 0;
        for _ in 0..12 {
            acc = acc * 6 + u64::from(self.d6() - 1);
        }
        u32::try_from(acc % u64::from(n)).unwrap_or(0)
    }
}

/// A deterministic generator (`SplitMix64`). Its whole state is one `u64`,
/// so the world store can save it and a replay can restore it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeededDice {
    state: u64,
}

impl SeededDice {
    /// A generator that starts at `seed`.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// The state to store. [`SeededDice::new`] with it resumes the stream.
    #[must_use]
    pub const fn state(&self) -> u64 {
        self.state
    }

    const fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

impl DiceRng for SeededDice {
    fn d6(&mut self) -> u8 {
        // Rejection sampling keeps each face equally likely.
        loop {
            let v = self.next_u64() >> 61; // 0..=7
            if v < 6 {
                return u8::try_from(v + 1).unwrap_or(1);
            }
        }
    }

    fn below(&mut self, n: u32) -> u32 {
        if n <= 1 {
            return 0;
        }
        u32::try_from(self.next_u64() % u64::from(n)).unwrap_or(0)
    }
}

/// Dice that come from a list, for tests. An empty list rolls 1s, so an
/// exhausted test fails loudly on the outcome instead of panicking.
#[derive(Debug, Clone, Default)]
pub struct FixedDice {
    faces: VecDeque<u8>,
}

impl FixedDice {
    /// Dice that roll `faces` in order. Each face is clamped to 1..=6.
    #[must_use]
    pub fn new(faces: impl IntoIterator<Item = u8>) -> Self {
        Self {
            faces: faces.into_iter().map(|f| f.clamp(1, 6)).collect(),
        }
    }

    /// The faces not yet rolled.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.faces.len()
    }
}

impl DiceRng for FixedDice {
    fn d6(&mut self) -> u8 {
        self.faces.pop_front().unwrap_or(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_dice_replay_from_saved_state() {
        let mut a = SeededDice::new(42);
        let _ = a.roll_3d6();
        let saved = a.state();
        let next: Vec<_> = (0..20).map(|_| a.d6()).collect();
        let mut b = SeededDice::new(saved);
        let replay: Vec<_> = (0..20).map(|_| b.d6()).collect();
        assert_eq!(next, replay);
    }

    #[test]
    fn seeded_faces_are_in_range_and_all_appear() {
        let mut d = SeededDice::new(7);
        let mut seen = [0u32; 6];
        for _ in 0..6000 {
            let f = d.d6();
            assert!((1..=6).contains(&f));
            seen[usize::from(f - 1)] += 1;
        }
        assert!(seen.iter().all(|&c| c > 800), "{seen:?}");
    }

    #[test]
    fn fixed_dice_roll_in_order_then_ones() {
        let mut d = FixedDice::new([6, 5, 9]);
        assert_eq!(d.roll_3d6(), [6, 5, 6]);
        assert_eq!(d.d6(), 1);
    }

    #[test]
    fn below_stays_in_range() {
        let mut d = SeededDice::new(1);
        for n in [0, 1, 2, 7, 100] {
            for _ in 0..50 {
                assert!(d.below(n) < n.max(1));
            }
        }
        let mut f = FixedDice::new([6; 24]);
        assert!(f.below(10) < 10);
    }
}
