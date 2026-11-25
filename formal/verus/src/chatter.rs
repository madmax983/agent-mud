//! The anti-chatter kernel of one room.
//!
//! Mirrors `crates/mud-supervisor/src/chatter.rs`. The text between the
//! mirror markers must match the Rust source; only the spec clauses differ.
//!
//! No bound is needed. An agent-only event is allowed iff the count before
//! it is under `max`, so the saturating count cannot let events through at
//! `max == 255` (the proof found that hole in an earlier `next <= max`).

use vstd::prelude::*;

verus! {

/// What happened in the room, as the chatter counter sees it.
#[derive(Clone, Copy)]
// mirror-begin chatter_input
pub enum ChatterInput {
    /// A human did something.
    Human,
    /// A macro-tick fired.
    MacroTick,
    /// An agent did something while a human is present.
    AgentWithHuman,
    /// An agent did something while no human is present.
    AgentAlone,
    /// The server did something (a mob, a respawn): it does not count.
    World,
}
// mirror-end chatter_input

/// The step over plain values.
pub open spec fn spec_step(turns: u8, input: ChatterInput, max: u8) -> (u8, bool) {
    match input {
        ChatterInput::Human | ChatterInput::MacroTick => (0, true),
        ChatterInput::AgentWithHuman | ChatterInput::World => (turns, true),
        ChatterInput::AgentAlone => {
            let next: u8 = if turns == 255 { 255 } else { (turns + 1) as u8 };
            (next, turns < max)
        },
    }
}

// mirror-begin chatter_step
/// One step: the new count and whether the event may wake other agents.
#[must_use]
pub const fn chatter_step(turns: u8, input: ChatterInput, max: u8) -> (r: (u8, bool))
    ensures
        r == spec_step(turns, input, max),
        // Human and macro-tick reset to 0 and allow.
        (input is Human || input is MacroTick) ==> r.0 == 0 && r.1,
        // An agent with a human present, or the world, changes nothing and
        // allows.
        (input is AgentWithHuman || input is World) ==> r.0 == turns && r.1,
        // An agent alone counts one more (saturating) and is allowed iff
        // the count before it is under `max`.
        input is AgentAlone ==> r.0 as int == (if turns == 255 { 255 } else { turns + 1 })
            && (r.1 <==> turns < max),
{
    match input {
        ChatterInput::Human | ChatterInput::MacroTick => (0, true),
        ChatterInput::AgentWithHuman | ChatterInput::World => (turns, true),
        ChatterInput::AgentAlone => {
            let next = turns.saturating_add(1);
            (next, turns < max)
        }
    }
}
// mirror-end chatter_step

/// A room run from a fresh counter: the count after `inputs`, and how many
/// agent-only events were allowed since the last reset (human or tick).
pub open spec fn run(inputs: Seq<ChatterInput>, max: u8) -> (u8, int)
    decreases inputs.len(),
{
    if inputs.len() == 0 {
        (0, 0)
    } else {
        let (turns, alone) = run(inputs.drop_last(), max);
        let input = inputs.last();
        let (next, allow) = spec_step(turns, input, max);
        let alone2 = match input {
            ChatterInput::Human | ChatterInput::MacroTick => 0,
            ChatterInput::AgentAlone => if allow { alone + 1 } else { alone },
            _ => alone,
        };
        (next, alone2)
    }
}

/// The safety property: starting from 0, for any sequence of inputs, the
/// agent-only events allowed since the last reset number exactly
/// `min(turns, max)`, so never more than `max`.
pub proof fn lemma_run_allows_at_most_max(inputs: Seq<ChatterInput>, max: u8)
    ensures
        run(inputs, max).1 == (if run(inputs, max).0 <= max { run(inputs, max).0 as int } else { max as int }),
        0 <= run(inputs, max).1 <= max,
    decreases inputs.len(),
{
    if inputs.len() > 0 {
        lemma_run_allows_at_most_max(inputs.drop_last(), max);
    }
}

/// How much an input counts toward holding a room: a reset (human,
/// macro-tick) least, then the inputs that keep the count (an agent with a
/// human present, the world), then an agent alone.
pub open spec fn rank(input: ChatterInput) -> int {
    match input {
        ChatterInput::Human | ChatterInput::MacroTick => 0,
        ChatterInput::AgentWithHuman | ChatterInput::World => 1,
        ChatterInput::AgentAlone => 2,
    }
}

/// The count after `inputs`, from a counter at `turns`.
pub open spec fn count_from(turns: u8, inputs: Seq<ChatterInput>, max: u8) -> u8
    decreases inputs.len(),
{
    if inputs.len() == 0 {
        turns
    } else {
        spec_step(count_from(turns, inputs.drop_last(), max), inputs.last(), max).0
    }
}

/// Whether input `i` of `inputs` may wake other agents, from a counter at
/// `turns`.
pub open spec fn allowed_at(turns: u8, inputs: Seq<ChatterInput>, max: u8, i: int) -> bool {
    spec_step(count_from(turns, inputs.take(i), max), inputs[i], max).1
}

pub open spec fn min8(a: u8, b: u8) -> u8 {
    if a <= b { a } else { b }
}

/// One step keeps "holds at least as much" (the count clamped at `max`)
/// when the second input counts at least as much as the first, and the
/// second counter allows only what the first allows.
pub proof fn lemma_step_holds_at_least(a: u8, b: u8, x: ChatterInput, y: ChatterInput, max: u8)
    requires
        min8(a, max) <= min8(b, max),
        rank(x) <= rank(y),
    ensures
        min8(spec_step(a, x, max).0, max) <= min8(spec_step(b, y, max).0, max),
        spec_step(b, y, max).1 ==> spec_step(a, x, max).1,
{
}

/// The boot fold can only hold more (ADR 0011, phase 4). The live counter
/// saw the inputs `live`; the boot folds `boot` over the reloaded event
/// tail, where each input counts at least as much: a macro-tick reset,
/// which is not an event, reads as the world (no change), and an agent
/// event whose recipients show no player reads as an agent alone. From a
/// start that holds at least as much (`max`, a history the tail does not
/// reach back to the start of; 0 from the world's first event), the fold
/// ends holding at least as much, and every event it lets wake other agents
/// the live counter let through too.
pub proof fn lemma_fold_holds_at_least(
    live: Seq<ChatterInput>,
    boot: Seq<ChatterInput>,
    live_turns: u8,
    boot_turns: u8,
    max: u8,
)
    requires
        live.len() == boot.len(),
        min8(live_turns, max) <= min8(boot_turns, max),
        forall|i: int| 0 <= i < live.len() ==> rank(#[trigger] live[i]) <= rank(boot[i]),
    ensures
        min8(count_from(live_turns, live, max), max) <= min8(count_from(boot_turns, boot, max), max),
        forall|i: int|
            0 <= i < live.len() ==> (#[trigger] allowed_at(boot_turns, boot, max, i) ==> allowed_at(
                live_turns,
                live,
                max,
                i,
            )),
    decreases live.len(),
{
    if live.len() > 0 {
        let (lp, bp) = (live.drop_last(), boot.drop_last());
        assert forall|i: int| 0 <= i < lp.len() implies rank(#[trigger] lp[i]) <= rank(bp[i]) by {
            assert(lp[i] == live[i] && bp[i] == boot[i]);
        }
        lemma_fold_holds_at_least(lp, bp, live_turns, boot_turns, max);
        let last = live.len() - 1;
        assert(rank(live[last]) <= rank(boot[last]));
        lemma_step_holds_at_least(
            count_from(live_turns, lp, max),
            count_from(boot_turns, bp, max),
            live.last(),
            boot.last(),
            max,
        );
        assert forall|i: int|
            0 <= i < live.len() && #[trigger] allowed_at(boot_turns, boot, max, i) implies allowed_at(
                live_turns,
                live,
                max,
                i,
            ) by {
            if i < last {
                assert(live.take(i) =~= lp.take(i));
                assert(boot.take(i) =~= bp.take(i));
                assert(live[i] == lp[i] && boot[i] == bp[i]);
                assert(allowed_at(boot_turns, bp, max, i) ==> allowed_at(live_turns, lp, max, i));
            } else {
                assert(live.take(i) =~= lp);
                assert(boot.take(i) =~= bp);
            }
        }
    }
}

/// The executable steps agree with [`run`]: stepping a fresh counter
/// through `inputs` with [`chatter_step`] ends at `run(inputs, max).0`.
pub fn run_exec(inputs: &Vec<ChatterInput>, max: u8) -> (r: u8)
    ensures
        r == run(inputs@, max).0,
{
    let mut turns: u8 = 0;
    let mut i: usize = 0;
    while i < inputs.len()
        invariant
            0 <= i <= inputs@.len(),
            turns == run(inputs@.subrange(0, i as int), max).0,
        decreases inputs@.len() - i,
    {
        let input = inputs[i];
        proof {
            assert(inputs@.subrange(0, i + 1).drop_last() =~= inputs@.subrange(0, i as int));
        }
        let (next, _allow) = chatter_step(turns, input, max);
        turns = next;
        i += 1;
    }
    proof {
        assert(inputs@.subrange(0, inputs@.len() as int) =~= inputs@);
    }
    turns
}

} // verus!
