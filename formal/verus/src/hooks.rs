//! The world-hook kernels: the breaker of a faulting hook, and effect
//! admission.
//!
//! Mirrors `crates/mud-world/src/hooks.rs`. The text between the mirror
//! markers must match the Rust source; only the spec clauses differ.
//!
//! The breaker: a hook runs at most `max` consecutive times that fault,
//! for every `max` (255 included), and stays tripped until a reload.
//!
//! Admission: the saturating tally admits exactly the effect lists of at
//! most `max` effects with at most one heal/hurt, where a refuse stands
//! alone in a command hook. `max <= 16` (the pack limit), so saturation at
//! 255 can never let an over-long list through.

use vstd::prelude::*;

verus! {

#[derive(Clone, Copy)]
// mirror-begin hook_outcome
/// What happened to a hook, as its breaker sees it.
pub enum HookOutcome {
    /// The hook ran and its effects were admitted.
    Ran,
    /// An error, a limit, a panic or an inadmissible effect list.
    Faulted,
    /// The scripts were loaded or reloaded.
    Reloaded,
}
// mirror-end hook_outcome

// mirror-begin hook_allowed
/// Whether a hook with `faults` consecutive faults may run.
#[must_use]
pub const fn hook_allowed(faults: u8, max: u8) -> (r: bool)
    ensures
        r <==> faults < max,
{
    faults < max
}
// mirror-end hook_allowed

/// The breaker step over plain values.
pub open spec fn spec_hook_step(faults: u8, outcome: HookOutcome, max: u8) -> (u8, bool) {
    match outcome {
        HookOutcome::Reloaded => (0, 0 < max),
        HookOutcome::Ran => if faults < max { (0, true) } else { (faults, false) },
        HookOutcome::Faulted => {
            let next: u8 = if faults == 255 { 255 } else { (faults + 1) as u8 };
            (next, next < max)
        },
    }
}

// mirror-begin hook_step
/// One breaker step: the new consecutive-fault count and whether the hook may run next.
#[must_use]
pub const fn hook_step(faults: u8, outcome: HookOutcome, max: u8) -> (r: (u8, bool))
    ensures
        r == spec_hook_step(faults, outcome, max),
        // The flag is exactly `hook_allowed` of the new count.
        r.1 <==> r.0 < max,
        // A reload reopens the breaker.
        outcome is Reloaded ==> r.0 == 0,
        // A good run under the limit clears the count.
        (outcome is Ran && faults < max) ==> r == (0u8, true),
        // A tripped hook stays tripped until a reload.
        (faults >= max && !(outcome is Reloaded)) ==> !r.1 && r.0 >= faults,
        // A fault counts one more, saturating.
        outcome is Faulted ==> r.0 as int == (if faults == 255 { 255 } else { faults + 1 }),
{
    match outcome {
        HookOutcome::Reloaded => (0, 0 < max),
        HookOutcome::Ran => {
            if faults < max {
                (0, true)
            } else {
                (faults, false)
            }
        }
        HookOutcome::Faulted => {
            let next = faults.saturating_add(1);
            (next, next < max)
        }
    }
}
// mirror-end hook_step

/// The count after `n` consecutive faults from a fresh breaker.
pub open spec fn faults_after(n: nat) -> u8
    decreases n,
{
    if n == 0 {
        0
    } else {
        spec_hook_step(faults_after((n - 1) as nat), HookOutcome::Faulted, 255).0
    }
}

/// `n` faults leave the count at `min(n, 255)`.
pub proof fn lemma_faults_after(n: nat)
    ensures
        faults_after(n) as int == (if n <= 255 { n as int } else { 255 }),
    decreases n,
{
    if n > 0 {
        lemma_faults_after((n - 1) as nat);
    }
}

/// The safety property of the breaker: after `n` consecutive faults the
/// hook may run iff `n < max`, for every `max`, 255 included. So a hook
/// runs at most `max` consecutive faulting times.
pub proof fn lemma_at_most_max_consecutive_faults(n: nat, max: u8)
    ensures
        (faults_after(n) < max) <==> (n < max),
{
    lemma_faults_after(n);
}

#[derive(Clone, Copy)]
// mirror-begin effect_class
/// The class of a script effect, as admission sees it.
pub enum EffectClass {
    /// msg or narrate.
    Text,
    /// refuse.
    Refuse,
    /// heal or hurt.
    Body,
    /// lock, unlock, flags and marks.
    State,
}
// mirror-end effect_class

#[derive(Clone, Copy)]
// mirror-begin effect_tally
/// What a hook's effect list holds so far.
pub struct Tally {
    /// Effects seen (saturating).
    pub effects: u8,
    /// `refuse()` effects seen (saturating).
    pub refuses: u8,
    /// `heal()` and `hurt()` effects seen (saturating).
    pub bodies: u8,
}
// mirror-end effect_tally

/// `x + 1`, saturating at 255.
pub open spec fn sat_inc(x: u8) -> u8 {
    if x == 255 { 255 } else { (x + 1) as u8 }
}

/// The tally step over plain values.
pub open spec fn spec_tally_step(t: Tally, class: EffectClass) -> Tally {
    match class {
        EffectClass::Refuse => Tally { effects: sat_inc(t.effects), refuses: sat_inc(t.refuses), bodies: t.bodies },
        EffectClass::Body => Tally { effects: sat_inc(t.effects), refuses: t.refuses, bodies: sat_inc(t.bodies) },
        _ => Tally { effects: sat_inc(t.effects), refuses: t.refuses, bodies: t.bodies },
    }
}

// mirror-begin tally_step
/// Adds one effect to the tally.
#[must_use]
pub const fn tally_step(t: Tally, class: EffectClass) -> (r: Tally)
    ensures
        r == spec_tally_step(t, class),
{
    let effects = t.effects.saturating_add(1);
    match class {
        EffectClass::Refuse => Tally {
            effects,
            refuses: t.refuses.saturating_add(1),
            bodies: t.bodies,
        },
        EffectClass::Body => Tally {
            effects,
            refuses: t.refuses,
            bodies: t.bodies.saturating_add(1),
        },
        EffectClass::Text | EffectClass::State => Tally {
            effects,
            refuses: t.refuses,
            bodies: t.bodies,
        },
    }
}
// mirror-end tally_step

/// The admission rule over plain values.
pub open spec fn spec_tally_admissible(t: Tally, max: u8, command_hook: bool) -> bool {
    t.effects <= max && t.bodies <= 1 && (t.refuses == 0 || (command_hook && t.effects == 1))
}

// mirror-begin tally_admissible
/// Whether a finished tally may be applied.
#[must_use]
pub const fn tally_admissible(t: Tally, max: u8, command_hook: bool) -> (r: bool)
    ensures
        r == spec_tally_admissible(t, max, command_hook),
{
    t.effects <= max && t.bodies <= 1 && (t.refuses == 0 || (command_hook && t.effects == 1))
}
// mirror-end tally_admissible

/// How many effects of class `c` a list holds.
pub open spec fn count(s: Seq<EffectClass>, c: EffectClass) -> nat
    decreases s.len(),
{
    if s.len() == 0 {
        0
    } else {
        count(s.drop_last(), c) + (if s.last() == c { 1nat } else { 0nat })
    }
}

/// The empty tally.
pub open spec fn tally_zero() -> Tally {
    Tally { effects: 0, refuses: 0, bodies: 0 }
}

/// The tally of a list: the fold of the step from the empty tally.
pub open spec fn tally_of(s: Seq<EffectClass>) -> Tally
    decreases s.len(),
{
    if s.len() == 0 {
        tally_zero()
    } else {
        spec_tally_step(tally_of(s.drop_last()), s.last())
    }
}

/// `min(n, 255)`.
pub open spec fn cap(n: nat) -> int {
    if n <= 255 { n as int } else { 255 }
}

/// The plain meaning of admission: at most `max` effects, at most one
/// heal/hurt, and a refuse only alone in a command hook.
pub open spec fn admissible_spec(s: Seq<EffectClass>, max: u8, cmd: bool) -> bool {
    s.len() <= max && count(s, EffectClass::Body) <= 1 && (count(s, EffectClass::Refuse) == 0 || (
    cmd && s.len() == 1))
}

/// The tally counts exactly, saturating at 255.
pub proof fn lemma_tally_counts(s: Seq<EffectClass>)
    ensures
        tally_of(s).effects as int == cap(s.len()),
        tally_of(s).refuses as int == cap(count(s, EffectClass::Refuse)),
        tally_of(s).bodies as int == cap(count(s, EffectClass::Body)),
    decreases s.len(),
{
    if s.len() > 0 {
        lemma_tally_counts(s.drop_last());
    }
}

/// Admission is exact: the saturating tally admits a list iff the list
/// meets the plain rule. Saturation never admits an over-long list, a
/// second heal/hurt, or a refuse mixed with anything else.
pub proof fn lemma_admission_is_exact(s: Seq<EffectClass>, max: u8, cmd: bool)
    requires
        max <= 16,
    ensures
        spec_tally_admissible(tally_of(s), max, cmd) == admissible_spec(s, max, cmd),
{
    lemma_tally_counts(s);
}

/// The executable fold agrees with [`tally_of`].
pub fn tally_exec(classes: &Vec<EffectClass>) -> (r: Tally)
    ensures
        r == tally_of(classes@),
{
    let mut t = Tally { effects: 0, refuses: 0, bodies: 0 };
    let mut i: usize = 0;
    while i < classes.len()
        invariant
            0 <= i <= classes@.len(),
            t == tally_of(classes@.subrange(0, i as int)),
        decreases classes@.len() - i,
    {
        let c = classes[i];
        proof {
            assert(classes@.subrange(0, i + 1).drop_last() =~= classes@.subrange(0, i as int));
        }
        t = tally_step(t, c);
        i += 1;
    }
    proof {
        assert(classes@.subrange(0, classes@.len() as int) =~= classes@);
    }
    t
}

} // verus!
