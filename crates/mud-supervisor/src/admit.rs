//! The wake hold: a wake waits while its entity cannot act.
//!
//! An unconscious or dead entity can do nothing a wake could ask of it,
//! so the router holds its waiting wake instead of delivering it. What
//! the wake carries (events, booked wakes that came due, macro ticks)
//! stays with it, and more merges in. When the entity comes to, the
//! supervisor forces one Immediate "came to" wake, which merges with the
//! hold and delivers everything at once.
//!
//! The kernel between the mirror markers is proved in
//! `formal/verus/src/admit.rs`: over every history of queued items and
//! router polls, no wake is delivered while the entity cannot act, and
//! every held item is delivered exactly once, in order, by the first wake
//! after it can act again.
//!
//! [`wake_expired`] bounds a running wake: only the time its entity spent
//! outside the gate's holds counts toward the timeout, so a hidden PC that
//! types a long notice is not cut off for typing.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// mirror-begin admit_enum
/// What the router does with an entity's waiting wake.
pub enum Admit {
    /// Deliver it now.
    Take,
    /// Keep it: the entity cannot act (it is unconscious or dead). It is
    /// not dropped; it merges into the wake that comes when the entity can
    /// act again.
    Hold,
    /// Keep it: it is not due yet, or the entity has a wake in flight or a
    /// plan to resume first.
    Wait,
}
// mirror-end admit_enum

// mirror-begin wake_admit
/// Whether the router delivers an entity's waiting wake, due at
/// `due_ms`, at `now_ms`. `can_act`: the entity is neither unconscious nor
/// dead. `free`: it has no wake in flight and no plan to resume first.
#[must_use]
pub const fn wake_admit(can_act: bool, free: bool, due_ms: u64, now_ms: u64) -> Admit {
    if !can_act {
        Admit::Hold
    } else if free && due_ms <= now_ms {
        Admit::Take
    } else {
        Admit::Wait
    }
}
// mirror-end wake_admit

// mirror-begin wake_expired
/// Whether a wake is past its bound of `timeout_ms`.
///
/// It has run `elapsed_ms`, `held_ms` of them with a line of its entity
/// held at the gate (a hidden PC typing). Only the time outside the holds
/// counts: a slow typist is not a mind that hangs.
#[must_use]
pub const fn wake_expired(elapsed_ms: u64, held_ms: u64, timeout_ms: u64) -> bool {
    elapsed_ms.saturating_sub(held_ms) >= timeout_ms
}
// mirror-end wake_expired

/// The reason of the wake an entity gets when it comes to.
pub const CAME_TO: &str = "came to";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_taken_while_the_entity_cannot_act() {
        for free in [false, true] {
            for (due, now) in [(0, 0), (0, 5), (5, 0)] {
                assert_eq!(wake_admit(false, free, due, now), Admit::Hold);
            }
        }
        assert_eq!(wake_admit(true, true, 5, 5), Admit::Take);
        assert_eq!(wake_admit(true, true, 6, 5), Admit::Wait);
        assert_eq!(wake_admit(true, false, 0, 5), Admit::Wait);
    }

    #[test]
    fn only_the_time_outside_holds_counts() {
        assert!(wake_expired(120_000, 0, 120_000));
        assert!(!wake_expired(119_999, 0, 120_000));
        assert!(
            !wake_expired(164_999, 45_000, 120_000),
            "a 45 s hold extends the bound"
        );
        assert!(wake_expired(165_000, 45_000, 120_000));
        assert!(
            !wake_expired(10, 50, 1),
            "held longer than run: nothing counts"
        );
        assert!(wake_expired(10, 50, 0));
    }
}
