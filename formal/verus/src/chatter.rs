//! The anti-chatter kernel of one room.
//!
//! Mirrors `crates/mud-supervisor/src/chatter.rs`. The text between the
//! mirror markers must match the Rust source; only the spec clauses differ.
//!
//! No bound for the step itself. The safety theorem needs `max < 255`: at
//! `max == 255` the saturating count sticks at 255 and every further
//! agent-only event is allowed (see [`lemma_run_allows_at_most_max`]).

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
            (next, next <= max)
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
        // the new count is at most `max`.
        input is AgentAlone ==> r.0 as int == (if turns == 255 { 255 } else { turns + 1 })
            && (r.1 <==> r.0 <= max),
{
    match input {
        ChatterInput::Human | ChatterInput::MacroTick => (0, true),
        ChatterInput::AgentWithHuman | ChatterInput::World => (turns, true),
        ChatterInput::AgentAlone => {
            let next = turns.saturating_add(1);
            (next, next <= max)
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
    requires
        max < 255,
    ensures
        run(inputs, max).1 == (if run(inputs, max).0 <= max { run(inputs, max).0 as int } else { max as int }),
        0 <= run(inputs, max).1 <= max,
    decreases inputs.len(),
{
    if inputs.len() > 0 {
        lemma_run_allows_at_most_max(inputs.drop_last(), max);
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
