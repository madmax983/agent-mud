//! The durability rules of the world store: the schema verdict, the
//! events retention cutoff, the archive's append rule, the cap on events
//! waiting for a failed store, and the `truncated` flag of `mud_events`.
//!
//! Each kernel here is mirrored in `formal/verus/src/durability.rs`, which
//! proves it (see that file and ADR 0006). Edit both in one commit; the
//! `verus_mirror` guard fails otherwise.

use crate::rewards::DAY_MS;

// mirror-begin max_keep_days
/// The longest retention, in days (a hundred years).
pub const MAX_KEEP_DAYS: u64 = 36_500;
// mirror-end max_keep_days

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// mirror-begin schema_verdict_enum
/// What opening a store with a given `user_version` does.
pub enum SchemaVerdict {
    /// The file is at the code's version: use it.
    Current,
    /// The file is older: run the missing migration steps.
    Upgrade,
    /// The file is newer than the code: refuse it.
    Refuse,
}
// mirror-end schema_verdict_enum

// mirror-begin schema_verdict
/// The verdict for a file at version `found`, with code at `latest`.
#[must_use]
pub const fn schema_verdict(found: u32, latest: u32) -> SchemaVerdict {
    if found > latest {
        SchemaVerdict::Refuse
    } else if found < latest {
        SchemaVerdict::Upgrade
    } else {
        SchemaVerdict::Current
    }
}
// mirror-end schema_verdict

// mirror-begin retention_cutoff
/// Events stamped before this game-clock time (`at_ms < cutoff`) may be
/// archived and trimmed. `keep_days == 0` keeps everything: the cutoff is 0.
#[must_use]
pub const fn retention_cutoff(now_ms: u64, keep_days: u64) -> u64 {
    let keep_ms = keep_days * DAY_MS;
    if keep_days == 0 || keep_ms >= now_ms {
        0
    } else {
        now_ms - keep_ms
    }
}
// mirror-end retention_cutoff

// mirror-begin archive_admits
/// Whether a row with `seq` joins an archive file whose last line holds
/// `last` (`None`: the file is empty or new).
#[must_use]
pub const fn archive_admits(last: Option<u64>, seq: u64) -> bool {
    match last {
        Some(l) => seq > l,
        None => true,
    }
}
// mirror-end archive_admits

// mirror-begin pending_overflow
/// How many of the oldest pending events to drop so that at most `cap`
/// stay.
#[must_use]
pub const fn pending_overflow(len: usize, cap: usize) -> usize {
    len.saturating_sub(cap)
}
// mirror-end pending_overflow

// mirror-begin events_truncated
/// Whether `mud_events` after `since` misses events: some event in
/// `since + 1 ..= last` is older than the oldest one the in-memory log
/// still holds (`None`: the log is empty).
#[must_use]
pub const fn events_truncated(since: u64, oldest: Option<u64>, last: u64) -> bool {
    match oldest {
        Some(o) => o > 0 && since < o - 1,
        None => since < last,
    }
}
// mirror-end events_truncated

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn a_newer_file_is_refused_and_an_older_one_upgrades() {
        assert_eq!(schema_verdict(0, 1), SchemaVerdict::Upgrade);
        assert_eq!(schema_verdict(1, 1), SchemaVerdict::Current);
        assert_eq!(schema_verdict(2, 1), SchemaVerdict::Refuse);
    }

    #[test]
    fn the_cutoff_keeps_the_last_days_and_zero_keeps_all() {
        let now = DAY_MS * 30 + 5;
        assert_eq!(retention_cutoff(now, 0), 0);
        assert_eq!(retention_cutoff(now, 7), DAY_MS * 23 + 5);
        assert_eq!(retention_cutoff(now, 31), 0, "younger than the window");
        assert_eq!(retention_cutoff(now, MAX_KEEP_DAYS), 0);
    }

    #[test]
    fn the_archive_takes_only_rows_above_its_last_line() {
        assert!(archive_admits(None, 0));
        assert!(archive_admits(Some(4), 5));
        assert!(!archive_admits(Some(5), 5));
        assert!(!archive_admits(Some(9), 5));
    }

    #[test]
    fn the_backlog_drops_only_past_its_cap() {
        assert_eq!(pending_overflow(3, 5), 0);
        assert_eq!(pending_overflow(5, 5), 0);
        assert_eq!(pending_overflow(8, 5), 3);
    }

    #[test]
    fn truncation_is_reported_only_for_lost_events() {
        // The log holds 101..=200.
        assert!(!events_truncated(100, Some(101), 200));
        assert!(!events_truncated(150, Some(101), 200));
        assert!(events_truncated(99, Some(101), 200));
        assert!(events_truncated(0, Some(101), 200));
        // A log that holds everything from 1.
        assert!(!events_truncated(0, Some(1), 200));
        // An empty log misses whatever came after `since`.
        assert!(!events_truncated(0, None, 0));
        assert!(events_truncated(3, None, 7));
        assert!(!events_truncated(7, None, 7));
    }

    proptest! {
        #[test]
        fn the_cutoff_never_trims_a_young_event(
            now in 0u64..u64::MAX / 2,
            days in 0u64..=MAX_KEEP_DAYS,
            age in 0u64..u64::MAX / 4,
        ) {
            let c = retention_cutoff(now, days);
            prop_assert!(c <= now);
            let at = now.saturating_sub(age);
            if days > 0 && u128::from(age) <= u128::from(days) * u128::from(DAY_MS) {
                prop_assert!(at >= c);
            }
        }

        #[test]
        fn truncated_matches_its_definition(
            since in 0u64..300,
            oldest in proptest::option::of(1u64..250),
            extra in 0u64..50,
        ) {
            let last = oldest.unwrap_or(0) + extra;
            let missing = (since + 1..=last).any(|s| oldest.is_none_or(|o| s < o));
            prop_assert_eq!(events_truncated(since, oldest, last), missing);
        }
    }
}
