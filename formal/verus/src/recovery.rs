//! The recovery kernel: one minute of regeneration and coming to.
//!
//! Mirrors `crates/mud-rules/src/recovery.rs`. The text between the mirror
//! markers must match the Rust source; only the spec clauses differ.
//!
//! - `recovery_step`: HP rises by one up to `hp_max` and never falls; an
//!   entity that stays unconscious is at 0 HP or below; it came to exactly
//!   when it was unconscious and no longer is; it comes to only above 0 HP,
//!   or on a successful roll above -HP, so an entity at or below -HP never
//!   comes to by the roll.
//! - `recovery_rolls`: a roll is asked for exactly when its outcome can
//!   change the step (`lemma_roll_matters_only_when_asked`), so the engine
//!   spends no dice on a step the roll cannot decide.
//! - Without any roll, an unconscious entity above -HP with no new injury
//!   comes to within `1 - hp` steps (`lemma_regen_brings_to`).
//!
//! No bound: `max` is at least 1, so `-max` fits, and HP rises only while
//! it is below `hp_max`.

use vstd::prelude::*;

verus! {

/// The maximum the kernel uses: below 1 counts as 1.
pub open spec fn eff_max(hp_max: int) -> int {
    if hp_max < 1 {
        1
    } else {
        hp_max
    }
}

/// HP after one step's regeneration.
pub open spec fn spec_next(hp: int, hp_max: int) -> int {
    if hp < hp_max {
        hp + 1
    } else {
        hp
    }
}

// mirror-begin recovery_rolls
/// Whether one recovery step of an entity at `hp` out of `hp_max` needs a
/// come-to roll: it is unconscious, still at 0 HP or below after the
/// step's regeneration, and above -HP (`hp_max`, or 1 when below 1).
#[must_use]
pub const fn recovery_rolls(hp: i32, hp_max: i32, unconscious: bool) -> (r: bool)
    ensures
        r == (unconscious && spec_next(hp as int, hp_max as int) <= 0 && spec_next(
            hp as int,
            hp_max as int,
        ) > -eff_max(hp_max as int)),
{
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
) -> (r: (i32, bool, bool))
    ensures
        r.0 == spec_next(hp as int, hp_max as int),
        r.0 >= hp,
        r.0 <= hp + 1,
        hp <= hp_max ==> r.0 <= hp_max,
        r.1 ==> r.0 <= 0,
        r.2 <==> (unconscious && !r.1),
        r.1 ==> unconscious,
        r.2 ==> (r.0 > 0 || (roll_ok && r.0 > -eff_max(hp_max as int))),
        r.0 <= -eff_max(hp_max as int) ==> !r.2,
        unconscious && r.0 > 0 ==> r.2,
        unconscious && roll_ok && r.0 > -eff_max(hp_max as int) ==> r.2,
{
    let max = if hp_max < 1 { 1 } else { hp_max };
    let next = if hp < hp_max { hp + 1 } else { hp };
    let up = unconscious && (next > 0 || (roll_ok && next > -max));
    (next, unconscious && !up, up)
}
// mirror-end recovery_step

/// The step as a spec over unbounded integers.
pub open spec fn spec_step(hp: int, hp_max: int, unconscious: bool, roll_ok: bool) -> (
    int,
    bool,
    bool,
) {
    let next = spec_next(hp, hp_max);
    let up = unconscious && (next > 0 || (roll_ok && next > -eff_max(hp_max)));
    (next, unconscious && !up, up)
}

/// The roll's outcome changes the step only when `recovery_rolls` asks
/// for a roll: otherwise a success and a failure give the same step.
pub proof fn lemma_roll_matters_only_when_asked(hp: i32, hp_max: i32, unconscious: bool)
    requires
        !(unconscious && spec_next(hp as int, hp_max as int) <= 0 && spec_next(
            hp as int,
            hp_max as int,
        ) > -eff_max(hp_max as int)),
    ensures
        spec_step(hp as int, hp_max as int, unconscious, true) == spec_step(
            hp as int,
            hp_max as int,
            unconscious,
            false,
        ),
{
}

/// The state after `n` steps from (`hp`, unconscious) with every roll
/// failing and no new injury.
pub open spec fn regen_run(hp: int, hp_max: int, unconscious: bool, n: nat) -> (int, bool)
    decreases n,
{
    if n == 0 {
        (hp, unconscious)
    } else {
        let s = spec_step(hp, hp_max, unconscious, false);
        regen_run(s.0, hp_max, s.1, (n - 1) as nat)
    }
}

/// Regeneration alone brings an unconscious entity to: from `hp <= 0`
/// below `hp_max >= 1`, with every roll failing, it is conscious after
/// `1 - hp` steps (for the log's Marta at -3 of 10: four minutes).
pub proof fn lemma_regen_brings_to(hp: int, hp_max: int)
    requires
        hp <= 0,
        hp_max >= 1,
    ensures
        !regen_run(hp, hp_max, true, (1 - hp) as nat).1,
    decreases 1 - hp,
{
    if hp == 0 {
        assert(spec_step(0, hp_max, true, false).1 == false);
        assert(regen_run(spec_step(0, hp_max, true, false).0, hp_max, false, 0 as nat) == (
        spec_step(0, hp_max, true, false).0, false));
    } else {
        let s = spec_step(hp, hp_max, true, false);
        assert(s.0 == hp + 1);
        assert(s.1 == true);
        lemma_regen_brings_to(hp + 1, hp_max);
        assert((1 - hp) as nat - 1 == (1 - (hp + 1)) as nat);
    }
}

} // verus!
