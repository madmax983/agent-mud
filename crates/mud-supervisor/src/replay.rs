//! Boot derivations (ADR 0011, phase 4): what a restart derives from the
//! reloaded event tail instead of storing it.
//!
//! - The router's waiting wakes. An addressed event (a tell, an attack on
//!   it, speech naming it) that no committed wake of its recipient
//!   consumed, and that is at most `replay_max_age_ms` old, is replayed as
//!   an Immediate wake ([`replay_due`]). Nothing else is replayed: no room
//!   batch, no ambient wake.
//! - The anti-chatter counters, folded from the tail through
//!   [`crate::chatter::chatter_step`] (see `Router::replay`).
//! - The MCP cursor of each agent's token: it starts at the same floor
//!   ([`replay_floor`]), so `mud_events` after a restart hands out nothing
//!   a committed wake consumed and skips nothing after it.
//!
//! The kernels between the mirror markers are proved in
//! `formal/verus/src/replay.rs`: a tell is answered at most once over
//! every history of answers and crashes, and a crash never loses a tell
//! that no committed wake consumed while it is within the window.

use mud_world::side::SideState;
use mud_world::{EntityId, Seq};

// mirror-begin replay_floor
/// The seq after which the boot replays an entity's addressed events, and
/// where its MCP cursor starts.
///
/// It is the stored `seen_seq` (the last event a committed wake of the
/// entity consumed), never above the world's last seq. An entity with no
/// stored row (a file from before versions, an entity new to the pack)
/// starts at the last seq: nothing says whether the events before it were
/// answered.
#[must_use]
pub const fn replay_floor(has_row: bool, seen_seq: u64, last_seq: u64) -> u64 {
    if has_row && seen_seq < last_seq {
        seen_seq
    } else {
        last_seq
    }
}
// mirror-end replay_floor

// mirror-begin replay_due
/// Whether the boot replays an addressed event: it came after the
/// entity's floor, and it is at most `max_age_ms` old at `now_ms`
/// (`at_ms + max_age_ms >= now_ms`, saturating).
#[must_use]
pub const fn replay_due(seq: u64, at_ms: u64, floor: u64, now_ms: u64, max_age_ms: u64) -> bool {
    let edge = if max_age_ms <= u64::MAX - at_ms {
        at_ms + max_age_ms
    } else {
        u64::MAX
    };
    seq > floor && edge >= now_ms
}
// mirror-end replay_due

/// The floor of `entity` at boot, from the side rows the world restored
/// and the world's last seq ([`replay_floor`]). The server issues each
/// agent's MCP token at it, and the supervisor replays above it.
#[must_use]
pub fn boot_floor(side: &SideState, entity: &EntityId, last: Seq) -> Seq {
    let row = side.entities.get(entity);
    Seq(replay_floor(
        row.is_some(),
        row.map_or(0, |r| r.seen_seq),
        last.0,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mud_world::side::EntityRow;
    use proptest::prelude::*;

    #[test]
    fn the_floor_is_the_stored_seen_seq_or_the_last_seq() {
        assert_eq!(replay_floor(true, 4, 9), 4);
        assert_eq!(replay_floor(true, 9, 9), 9);
        assert_eq!(replay_floor(true, 12, 9), 9, "never past the world");
        assert_eq!(replay_floor(false, 4, 9), 9, "no row: nothing to replay");
        let mut side = SideState::default();
        let ada = EntityId::from("ada");
        assert_eq!(boot_floor(&side, &ada, Seq(7)), Seq(7));
        side.entities.insert(
            ada.clone(),
            EntityRow {
                seen_seq: 3,
                ..EntityRow::default()
            },
        );
        assert_eq!(boot_floor(&side, &ada, Seq(7)), Seq(3));
    }

    #[test]
    fn an_event_is_due_after_the_floor_and_within_the_window() {
        assert!(replay_due(5, 1_000, 4, 1_000, 0));
        assert!(replay_due(5, 1_000, 4, 3_000, 2_000));
        assert!(!replay_due(5, 1_000, 4, 3_001, 2_000), "too old");
        assert!(!replay_due(5, 1_000, 5, 1_000, 2_000), "consumed");
        assert!(
            replay_due(5, u64::MAX - 1, 4, u64::MAX, u64::MAX),
            "saturates"
        );
    }

    proptest! {
        #[test]
        fn due_matches_the_spec(seq: u64, at: u64, floor: u64, now: u64, age: u64) {
            let want = seq > floor && u128::from(at) + u128::from(age) >= u128::from(now);
            prop_assert_eq!(replay_due(seq, at, floor, now, age), want);
        }
    }
}
