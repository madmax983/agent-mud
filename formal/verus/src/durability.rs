//! Durability rules of the world store: the schema verdict, the events
//! retention cutoff, the archive's append rule, the pending backlog cap and
//! the `truncated` flag of `mud_events`.
//!
//! Mirrors `crates/mud-world/src/durability.rs`. The text between the
//! mirror markers must match the Rust source; only the spec clauses differ.
//!
//! - `schema_verdict`: a file newer than the code is refused; an older one
//!   upgrades; only an equal one is used as it is.
//! - `retention_cutoff`: `keep_days == 0` trims nothing; an event younger
//!   than `keep_days` is never below the cutoff; the cutoff never passes
//!   `now` and never moves back as `now` grows.
//! - `archive_admits`: a row joins an archive file only above its last
//!   line. Over any sequence of batches, crashes and re-runs the file stays
//!   strictly ascending, so no event is written twice
//!   (`lemma_append_keeps_ascending`); a batch leaves every one of its rows
//!   covered (`lemma_append_covers`); and re-running a batch after a crash
//!   at any point of an earlier run of it gives the same file as one clean
//!   run (`lemma_crash_rerun_is_idempotent`).
//! - `pending_overflow`: dropping that many oldest events leaves at most
//!   `cap`, and nothing is dropped within the cap.
//! - `events_truncated`: true exactly when some event after `since_seq`
//!   (up to the last one) is no longer in the in-memory log.
//!
//! Bound: `retention_cutoff` requires `keep_days <= 36_500` (`[world]
//! events_keep_days` is checked to that range).

use vstd::prelude::*;

use crate::rewards::DAY_MS;

verus! {

// mirror-begin max_keep_days
/// The longest retention, in days (a hundred years).
pub const MAX_KEEP_DAYS: u64 = 36_500;
// mirror-end max_keep_days

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
pub const fn schema_verdict(found: u32, latest: u32) -> (r: SchemaVerdict)
    ensures
        (r is Refuse) <==> found > latest,
        (r is Upgrade) <==> found < latest,
        (r is Current) <==> found == latest,
{
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
pub const fn retention_cutoff(now_ms: u64, keep_days: u64) -> (r: u64)
    requires
        keep_days <= MAX_KEEP_DAYS,
    ensures
        keep_days == 0 ==> r == 0,
        r <= now_ms,
        r == spec_cutoff(now_ms, keep_days),
{
    let keep_ms = keep_days * DAY_MS;
    if keep_days == 0 || keep_ms >= now_ms {
        0
    } else {
        now_ms - keep_ms
    }
}
// mirror-end retention_cutoff

/// The cutoff never moves back while the clock moves forward.
pub proof fn lemma_cutoff_is_monotone(now1: u64, now2: u64, keep_days: u64)
    requires
        now1 <= now2,
        keep_days <= MAX_KEEP_DAYS,
    ensures
        spec_cutoff(now1, keep_days) <= spec_cutoff(now2, keep_days),
{
}

/// An event younger than `keep_days` is never below the cutoff, so it is
/// never trimmed.
pub proof fn lemma_young_events_survive(now_ms: u64, keep_days: u64, at: u64)
    requires
        keep_days <= MAX_KEEP_DAYS,
        at as int + keep_days as int * DAY_MS as int >= now_ms as int,
    ensures
        at >= spec_cutoff(now_ms, keep_days),
{
}

pub open spec fn spec_cutoff(now_ms: u64, keep_days: u64) -> int {
    let keep_ms = keep_days as int * DAY_MS as int;
    if keep_days == 0 || keep_ms >= now_ms {
        0
    } else {
        now_ms - keep_ms
    }
}

// mirror-begin archive_admits
/// Whether a row with `seq` joins an archive file whose last line holds
/// `last` (`None`: the file is empty or new).
#[must_use]
pub const fn archive_admits(last: Option<u64>, seq: u64) -> (r: bool)
    ensures
        r == spec_admits(last, seq),
{
    match last {
        Some(l) => seq > l,
        None => true,
    }
}
// mirror-end archive_admits

pub open spec fn spec_admits(last: Option<u64>, seq: u64) -> bool {
    match last {
        Some(l) => seq > l,
        None => true,
    }
}

/// The last line of a file.
pub open spec fn last_of(f: Seq<u64>) -> Option<u64> {
    if f.len() == 0 { None } else { Some(f.last()) }
}

/// Appending one row.
pub open spec fn step(f: Seq<u64>, seq: u64) -> Seq<u64> {
    if spec_admits(last_of(f), seq) { f.push(seq) } else { f }
}

/// Appending a batch, row by row.
pub open spec fn append(f: Seq<u64>, b: Seq<u64>) -> Seq<u64>
    decreases b.len(),
{
    if b.len() == 0 {
        f
    } else {
        step(append(f, b.drop_last()), b.last())
    }
}

/// A file with no repeated and no out-of-order line.
pub open spec fn ascending(f: Seq<u64>) -> bool {
    forall|i: int, j: int| 0 <= i < j < f.len() ==> f[i] < f[j]
}

/// `seq` is at or below the last line: the file already holds it, or
/// skipping it can lose nothing the run would write.
pub open spec fn covers(f: Seq<u64>, seq: u64) -> bool {
    f.len() > 0 && seq <= f.last()
}

pub proof fn lemma_step_keeps_ascending(f: Seq<u64>, seq: u64)
    requires
        ascending(f),
    ensures
        ascending(step(f, seq)),
{
}

/// The archive never holds a row twice, whatever runs, crashes and
/// re-runs built it.
pub proof fn lemma_append_keeps_ascending(f: Seq<u64>, b: Seq<u64>)
    requires
        ascending(f),
    ensures
        ascending(append(f, b)),
    decreases b.len(),
{
    if b.len() > 0 {
        lemma_append_keeps_ascending(f, b.drop_last());
        lemma_step_keeps_ascending(append(f, b.drop_last()), b.last());
    }
}

/// One row never lowers the last line.
pub proof fn lemma_step_keeps_coverage(f: Seq<u64>, x: u64, seq: u64)
    requires
        covers(f, seq),
    ensures
        covers(step(f, x), seq),
{
}

/// The last line never goes down.
pub proof fn lemma_append_keeps_coverage(f: Seq<u64>, b: Seq<u64>, seq: u64)
    requires
        covers(f, seq),
    ensures
        covers(append(f, b), seq),
    decreases b.len(),
{
    if b.len() > 0 {
        lemma_append_keeps_coverage(f, b.drop_last(), seq);
    }
}

/// After a batch, every row of it is covered.
pub proof fn lemma_append_covers(f: Seq<u64>, b: Seq<u64>)
    ensures
        forall|k: int| 0 <= k < b.len() ==> covers(append(f, b), #[trigger] b[k]),
    decreases b.len(),
{
    if b.len() > 0 {
        let p = b.drop_last();
        lemma_append_covers(f, p);
        assert forall|k: int| 0 <= k < b.len() implies covers(append(f, b), #[trigger] b[k]) by {
            if k < b.len() - 1 {
                assert(b[k] == p[k]);
                assert(covers(append(f, p), p[k]));
                lemma_step_keeps_coverage(append(f, p), b.last(), b[k]);
            }
        }
    }
}

/// A batch whose every row is covered appends nothing.
pub proof fn lemma_covered_batch_appends_nothing(f: Seq<u64>, b: Seq<u64>)
    requires
        forall|k: int| 0 <= k < b.len() ==> covers(f, #[trigger] b[k]),
    ensures
        append(f, b) == f,
    decreases b.len(),
{
    if b.len() > 0 {
        let p = b.drop_last();
        assert forall|k: int| 0 <= k < p.len() implies covers(f, #[trigger] p[k]) by {
            assert(p[k] == b[k]);
        }
        lemma_covered_batch_appends_nothing(f, p);
        assert(covers(f, b[b.len() - 1]));
    }
}

/// Appending `a + b` is appending `a`, then `b`.
pub proof fn lemma_append_concat(f: Seq<u64>, a: Seq<u64>, b: Seq<u64>)
    ensures
        append(f, a + b) == append(append(f, a), b),
    decreases b.len(),
{
    if b.len() == 0 {
        assert(a + b =~= a);
    } else {
        assert((a + b).drop_last() =~= a + b.drop_last());
        assert((a + b).last() == b.last());
        lemma_append_concat(f, a, b.drop_last());
    }
}

/// A crash after the first `k` rows of a batch were written (and fsynced),
/// before the trim committed, then a re-run of the whole batch: the file is
/// the one a single clean run writes. The trim then deletes the batch once.
pub proof fn lemma_crash_rerun_is_idempotent(f: Seq<u64>, b: Seq<u64>, k: int)
    requires
        0 <= k <= b.len(),
    ensures
        append(append(f, b.subrange(0, k)), b) == append(f, b),
{
    let head = b.subrange(0, k);
    let tail = b.subrange(k, b.len() as int);
    let g = append(f, head);
    assert(b =~= head + tail);
    lemma_append_concat(g, head, tail);
    lemma_append_covers(f, head);
    lemma_covered_batch_appends_nothing(g, head);
    lemma_append_concat(f, head, tail);
}

// mirror-begin pending_overflow
/// How many of the oldest pending events to drop so that at most `cap`
/// stay.
#[must_use]
pub const fn pending_overflow(len: usize, cap: usize) -> (r: usize)
    ensures
        len - r <= cap,
        r <= len,
        len <= cap ==> r == 0,
        len > cap ==> len - r == cap,
{
    len.saturating_sub(cap)
}
// mirror-end pending_overflow

// mirror-begin events_truncated
/// Whether `mud_events` after `since` misses events: some event in
/// `since + 1 ..= last` is older than the oldest one the in-memory log
/// still holds (`None`: the log is empty).
#[must_use]
pub const fn events_truncated(since: u64, oldest: Option<u64>, last: u64) -> (r: bool)
    requires
        oldest matches Some(o) ==> o <= last,
    ensures
        r <==> since < last && spec_missing(oldest, since + 1),
{
    match oldest {
        Some(o) => o > 0 && since < o - 1,
        None => since < last,
    }
}
// mirror-end events_truncated

/// The flag is exact: it is set iff some event in `since + 1 ..= last` is
/// missing from the log.
pub proof fn lemma_truncated_is_exact(since: u64, oldest: Option<u64>, last: u64)
    requires
        oldest matches Some(o) ==> o <= last,
    ensures
        (since < last && spec_missing(oldest, since + 1)) <==> exists|s: int|
            since < s <= last && #[trigger] spec_missing(oldest, s),
{
    if exists|s: int| since < s <= last && #[trigger] spec_missing(oldest, s) {
        let s = choose|s: int| since < s <= last && #[trigger] spec_missing(oldest, s);
        assert(spec_missing(oldest, since + 1));
    }
    if since < last && spec_missing(oldest, since + 1) {
        assert(spec_missing(oldest, since + 1));
    }
}

/// Whether the log no longer holds event `s`: the log holds `oldest..=last`.
pub open spec fn spec_missing(oldest: Option<u64>, s: int) -> bool {
    match oldest {
        Some(o) => s < o,
        None => true,
    }
}

} // verus!
