//! The anti-chatter state machine of one room.
//!
//! Agents must not talk to each other all night in an empty room. Each
//! room counts the agent events emitted while no human is present. Any
//! human event and any macro-tick reset the count. Past
//! [`MAX_AGENT_ONLY_TURNS`], the router drops every cross-agent wake in
//! the room until the next reset.

/// The default bound on agent-only turns.
pub const MAX_AGENT_ONLY_TURNS: u8 = 2;

/// What happened in the room, as the chatter counter sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

// mirror-begin chatter_step
/// One step: the new count and whether the event may wake other agents.
#[must_use]
pub const fn chatter_step(turns: u8, input: ChatterInput, max: u8) -> (u8, bool) {
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

/// The counter of one room.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RoomChatter {
    /// Agent events since the last reset with no human present.
    pub agent_only_turns: u8,
}

impl RoomChatter {
    /// Applies one input; returns whether cross-agent wakes may go out.
    pub const fn step(&mut self, input: ChatterInput, max: u8) -> bool {
        let (turns, allow) = chatter_step(self.agent_only_turns, input, max);
        self.agent_only_turns = turns;
        allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn two_agent_turns_then_silence_until_a_human() {
        let mut r = RoomChatter::default();
        assert!(r.step(ChatterInput::AgentAlone, 2));
        assert!(r.step(ChatterInput::AgentAlone, 2));
        assert!(
            !r.step(ChatterInput::AgentAlone, 2),
            "the third agent-only turn is dropped"
        );
        assert!(!r.step(ChatterInput::AgentAlone, 2));
        assert!(r.step(ChatterInput::World, 2), "mobs never count");
        assert!(r.step(ChatterInput::Human, 2));
        assert_eq!(r.agent_only_turns, 0);
        assert!(r.step(ChatterInput::AgentAlone, 2));
    }

    #[test]
    fn a_saturated_count_still_blocks_at_the_top_bound() {
        // Regression: with `next <= max`, max = 255 let every event through.
        let mut r = RoomChatter {
            agent_only_turns: 254,
        };
        assert!(r.step(ChatterInput::AgentAlone, 255));
        assert!(!r.step(ChatterInput::AgentAlone, 255));
        assert!(!r.step(ChatterInput::AgentAlone, 255));
    }

    #[test]
    fn a_macro_tick_resets() {
        let mut r = RoomChatter {
            agent_only_turns: 9,
        };
        assert!(r.step(ChatterInput::MacroTick, 2));
        assert_eq!(r.agent_only_turns, 0);
    }

    proptest! {
        #[test]
        fn never_more_than_max_alone_turns_between_resets(inputs in prop::collection::vec(0u8..5, 0..200)) {
            let mut r = RoomChatter::default();
            let mut allowed_alone = 0u32;
            for i in inputs {
                let input = match i {
                    0 => ChatterInput::Human,
                    1 => ChatterInput::MacroTick,
                    2 => ChatterInput::AgentWithHuman,
                    3 => ChatterInput::AgentAlone,
                    _ => ChatterInput::World,
                };
                let allow = r.step(input, MAX_AGENT_ONLY_TURNS);
                match input {
                    ChatterInput::Human | ChatterInput::MacroTick => allowed_alone = 0,
                    ChatterInput::AgentAlone if allow => allowed_alone += 1,
                    _ => {}
                }
                prop_assert!(allowed_alone <= u32::from(MAX_AGENT_ONLY_TURNS));
            }
        }
    }
}
