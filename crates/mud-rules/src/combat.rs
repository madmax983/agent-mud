//! One attack, start to finish: attack roll, active defense, damage.

use serde::{Deserialize, Serialize};

use crate::damage::INJURY_LIMIT;
use crate::{DamageDice, DamageType, DiceRng, Outcome, SuccessRoll, injury, success_roll};

/// Which active defense the target uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefenseKind {
    /// Get out of the way.
    Dodge,
    /// Deflect with a weapon.
    Parry,
    /// Interpose a shield.
    Block,
}

/// Everything needed to resolve one attack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttackInput {
    /// The attacker's effective weapon skill, with all modifiers.
    pub attack_skill: i32,
    /// The target's best defense and its effective value, if it can defend.
    pub defense: Option<(DefenseKind, i32)>,
    /// The damage dice.
    pub damage: DamageDice,
    /// The damage type.
    pub kind: DamageType,
    /// The target's damage resistance.
    pub dr: i32,
}

/// What happened in one attack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttackResolution {
    /// The attack roll.
    pub attack: SuccessRoll,
    /// The defense roll, when the target got one.
    pub defense: Option<(DefenseKind, SuccessRoll)>,
    /// Whether the attack landed.
    pub hit: bool,
    /// The rolled damage before DR, when it landed.
    pub raw_damage: i32,
    /// The injury after DR and the wounding multiplier.
    pub injury: i32,
    /// The damage type.
    pub kind: DamageType,
}

/// Resolves one attack.
///
/// A critical success on the attack cannot be defended. Otherwise a target
/// with a defense rolls it, and a success stops the attack.
pub fn resolve_attack(rng: &mut dyn DiceRng, input: &AttackInput) -> AttackResolution {
    let attack = success_roll(rng, input.attack_skill);
    let miss = AttackResolution {
        attack,
        defense: None,
        hit: false,
        raw_damage: 0,
        injury: 0,
        kind: input.kind,
    };
    if !attack.outcome.is_success() {
        return miss;
    }
    let mut defense = None;
    if attack.outcome != Outcome::CriticalSuccess
        && let Some((kind, value)) = input.defense
    {
        let roll = success_roll(rng, value);
        defense = Some((kind, roll));
        if roll.outcome.is_success() {
            return AttackResolution { defense, ..miss };
        }
    }
    let raw = input.damage.roll(rng, input.kind);
    AttackResolution {
        attack,
        defense,
        hit: true,
        raw_damage: raw,
        injury: injury(
            raw.clamp(-INJURY_LIMIT, INJURY_LIMIT),
            input.dr.clamp(-INJURY_LIMIT, INJURY_LIMIT),
            input.kind,
        ),
        kind: input.kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FixedDice;

    fn input(defense: Option<(DefenseKind, i32)>) -> AttackInput {
        AttackInput {
            attack_skill: 12,
            defense,
            damage: DamageDice::new(1, 1),
            kind: DamageType::Cutting,
            dr: 1,
        }
    }

    #[test]
    fn hit_defended_or_not() {
        // Attack 3+3+3=9 hits; dodge 8 rolls 6+6+6 and fails; damage 4+1=5, -1 DR, ×1.5 = 6.
        let mut d = FixedDice::new([3, 3, 3, 6, 6, 6, 4]);
        let r = resolve_attack(&mut d, &input(Some((DefenseKind::Dodge, 8))));
        assert!(r.hit);
        assert_eq!(r.raw_damage, 5);
        assert_eq!(r.injury, 6);
        assert!(r.defense.is_some());
    }

    #[test]
    fn successful_defense_stops_the_hit() {
        let mut d = FixedDice::new([3, 3, 3, 2, 2, 2]);
        let r = resolve_attack(&mut d, &input(Some((DefenseKind::Parry, 9))));
        assert!(!r.hit);
        assert_eq!(r.injury, 0);
    }

    #[test]
    fn a_miss_rolls_nothing_else() {
        let mut d = FixedDice::new([6, 6, 5, 1, 1, 1]);
        let r = resolve_attack(&mut d, &input(Some((DefenseKind::Dodge, 20))));
        assert!(!r.hit);
        assert!(r.defense.is_none());
        assert_eq!(d.remaining(), 3);
    }

    #[test]
    fn a_critical_attack_bypasses_defense() {
        let mut d = FixedDice::new([1, 1, 1, 6]);
        let r = resolve_attack(&mut d, &input(Some((DefenseKind::Dodge, 30))));
        assert!(r.hit);
        assert!(r.defense.is_none());
        assert_eq!(r.raw_damage, 7);
    }

    #[test]
    fn no_defense_means_a_success_lands() {
        let mut d = FixedDice::new([4, 4, 4, 2]);
        let r = resolve_attack(&mut d, &input(None));
        assert!(r.hit);
        assert_eq!(r.injury, 3);
    }
}
