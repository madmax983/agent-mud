//! Recovery: one step of regeneration and coming to.
//!
//! The engine runs one step for each entity in the world every recovery
//! period (a minute, by default). HP rises by one, up to the maximum. An
//! unconscious entity comes to once it is above 0 HP, or, when the pack
//! asks for a come-to roll, on a successful roll while it is still above
//! -HP. The roll itself stays outside the kernel: the caller rolls only when
//! [`recovery_rolls`] says the roll can decide the step, and passes the
//! outcome in.
//!
//! Both kernels are mirrored by `formal/verus/src/recovery.rs`.

// mirror-begin recovery_rolls
/// Whether one recovery step of an entity at `hp` out of `hp_max` needs a
/// come-to roll: it is unconscious, still at 0 HP or below after the
/// step's regeneration, and above -HP (`hp_max`, or 1 when below 1).
#[must_use]
pub const fn recovery_rolls(hp: i32, hp_max: i32, unconscious: bool) -> bool {
    let max = if hp_max < 1 { 1 } else { hp_max };
    let next = if hp < hp_max { hp + 1 } else { hp };
    unconscious && next <= 0 && next > -max
}
// mirror-end recovery_rolls

// mirror-begin recovery_step
/// One recovery step (a minute, by default) of an entity at `hp` out of
/// `hp_max`.
///
/// HP rises by one, up to `hp_max`. An unconscious entity comes to above
/// 0 HP, or when `roll_ok` (its come-to roll succeeded) above -HP. Returns
/// the new HP, whether it is still unconscious, and whether it came to in
/// this step.
#[must_use]
pub const fn recovery_step(
    hp: i32,
    hp_max: i32,
    unconscious: bool,
    roll_ok: bool,
) -> (i32, bool, bool) {
    let max = if hp_max < 1 { 1 } else { hp_max };
    let next = if hp < hp_max { hp + 1 } else { hp };
    let up = unconscious && (next > 0 || (roll_ok && next > -max));
    (next, unconscious && !up, up)
}
// mirror-end recovery_step

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn regen_alone_brings_marta_to_in_four_steps() {
        let mut s = (-3, true, false);
        let mut steps = 0;
        while s.1 {
            s = recovery_step(s.0, 10, s.1, false);
            steps += 1;
        }
        assert_eq!((steps, s), (4, (1, false, true)));
    }

    #[test]
    fn a_roll_brings_an_entity_to_at_negative_hp_but_not_at_minus_hp() {
        assert_eq!(recovery_step(-4, 10, true, true), (-3, false, true));
        assert!(recovery_rolls(-4, 10, true));
        assert_eq!(recovery_step(-11, 10, true, true), (-10, true, false));
        assert!(!recovery_rolls(-11, 10, true), "at -HP no roll is asked");
        assert!(!recovery_rolls(0, 10, true), "regen alone brings it to");
        assert!(!recovery_rolls(-3, 10, false), "awake: nothing to roll");
    }

    #[test]
    fn hp_stops_at_the_maximum() {
        assert_eq!(recovery_step(10, 10, false, false), (10, false, false));
        assert_eq!(recovery_step(12, 10, false, false), (12, false, false));
        assert_eq!(recovery_step(5, i32::MIN, true, true), (5, false, true));
    }

    proptest! {
        #[test]
        fn the_step_keeps_its_promises(hp in any::<i32>(), max in any::<i32>(), down: bool, ok: bool) {
            let (next, still, came) = recovery_step(hp, max, down, ok);
            prop_assert!(next >= hp && next - hp <= 1);
            if hp <= max {
                prop_assert!(next <= max);
            }
            prop_assert!(!still || next <= 0);
            prop_assert_eq!(came, down && !still);
            if !recovery_rolls(hp, max, down) {
                prop_assert_eq!(recovery_step(hp, max, down, !ok), (next, still, came));
            }
        }
    }
}
