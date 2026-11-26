//! When a macro tick waits, and how soon a lost one comes again.
//!
//! A macro tick is the slow timer of an always-on agent. A player in the
//! room used to defer it for as long as the player stood there, because
//! a present player's actions wake the agent anyway. An idle player does
//! not act, so an agent beside one never woke (the owner's run of
//! 2026-11-26: a hidden PC and an NPC stood in one room after a restart,
//! and neither woke for hours). [`tick_deferred`] defers a tick only
//! while a player other than the agent acted in the room within the
//! window (`[supervisor] macro_defer_ms`). After that the tick fires as
//! if the agent were alone. It reads only who acted and when, never
//! whether a human or an agent plays the PC.
//!
//! A tick whose wake never reached the mind (the mind was unavailable, or
//! it failed before any model call) is lost. [`tick_retry_ms`] gives the
//! delay of the next try: it starts short and doubles with each loss in a
//! row, up to the shortest normal interval, so a broken mind is tried
//! again soon but not hammered.
//!
//! The kernels between the mirror markers are proved in
//! `formal/verus/src/ticks.rs`.

/// The first retry of a lost macro tick, in game-clock ms: 3 minutes.
pub const TICK_RETRY_FIRST_MS: u64 = 180_000;

// mirror-begin tick_deferred
/// Whether a macro tick waits at `now_ms`.
///
/// It waits when a player other than the entity acted in its room at
/// `last_act_ms`, less than `window_ms` ago. `None`: no such player acted
/// (or none is present). An act stamped after `now_ms` counts as now.
#[must_use]
pub const fn tick_deferred(last_act_ms: Option<u64>, now_ms: u64, window_ms: u64) -> bool {
    match last_act_ms {
        Some(at) => now_ms.saturating_sub(at) < window_ms,
        None => false,
    }
}
// mirror-end tick_deferred

// mirror-begin tick_retry_ms
/// The delay before a lost macro tick is tried again, in ms.
///
/// `prev_ms` is the delay of the previous loss in a row, 0 for the first.
/// The first is `first_ms`; each later one doubles the previous; none
/// passes `cap_ms` (the shortest normal interval of the entity).
#[must_use]
pub const fn tick_retry_ms(prev_ms: u64, first_ms: u64, cap_ms: u64) -> u64 {
    if prev_ms == 0 {
        if first_ms < cap_ms { first_ms } else { cap_ms }
    } else if prev_ms > cap_ms / 2 {
        cap_ms
    } else {
        prev_ms * 2
    }
}
// mirror-end tick_retry_ms

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_recent_act_defers_a_tick() {
        let window = 600_000;
        assert!(!tick_deferred(None, 1_000_000, window), "nobody acted");
        assert!(tick_deferred(Some(1_000_000), 1_000_000, window));
        assert!(tick_deferred(Some(400_001), 1_000_000, window));
        assert!(
            !tick_deferred(Some(400_000), 1_000_000, window),
            "ten minutes of quiet"
        );
        assert!(
            tick_deferred(Some(2_000_000), 1_000_000, window),
            "an act stamped ahead of the clock counts as now"
        );
        assert!(!tick_deferred(Some(5), 5, 0), "a window of 0 never defers");
    }

    #[test]
    fn retries_double_up_to_the_cap() {
        let cap = 7_200_000;
        let mut d = 0;
        let mut seen = Vec::new();
        for _ in 0..8 {
            d = tick_retry_ms(d, TICK_RETRY_FIRST_MS, cap);
            seen.push(d);
        }
        assert_eq!(
            seen,
            [
                180_000, 360_000, 720_000, 1_440_000, 2_880_000, 5_760_000, 7_200_000, 7_200_000
            ]
        );
        assert_eq!(tick_retry_ms(0, TICK_RETRY_FIRST_MS, 7_200), 7_200);
        assert_eq!(tick_retry_ms(u64::MAX, 1, u64::MAX), u64::MAX);
        assert_eq!(tick_retry_ms(5, 1, 0), 0);
    }
}
