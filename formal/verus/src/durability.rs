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
//! - `archive_verdict`: a row is written only above its file's last line;
//!   at or below it, the file must already hold the row's exact line
//!   (skipped), or the batch is refused and nothing is trimmed. Over any
//!   sequence of batches, crashes and re-runs the file stays strictly
//!   ascending, so no event is written twice
//!   (`lemma_append_keeps_ascending`); an accepted batch leaves every one
//!   of its rows in the file, the exact line, so the delete removes only
//!   archived rows (`lemma_every_trimmed_row_is_archived`); re-running a
//!   batch after a crash at any point of an earlier run of it gives the
//!   same file as one clean run (`lemma_crash_rerun_is_idempotent`); and a
//!   seq-prefix batch above the file is written whole
//!   (`lemma_prefix_leaves_only_higher_rows`,
//!   `lemma_fresh_batch_appends_all`).
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

// mirror-begin archive_verdict_enum
/// What the archive does with one row of a trim batch.
pub enum ArchiveVerdict {
    /// The row is above the file's last line: write it.
    Append,
    /// The file already holds this exact line (a re-run after a crash
    /// between the append and the delete): skip it.
    Present,
    /// The file has a line at or above this seq, and not this row's line:
    /// another history under the same epoch (a restored backup). Refuse
    /// the batch; nothing is trimmed.
    Conflict,
}
// mirror-end archive_verdict_enum

// mirror-begin archive_verdict
/// The verdict for a row with `seq` against an archive file.
///
/// `last` is the seq of the file's last line (`None`: the file is empty or
/// new). `present` says whether the file holds this row's exact line; it
/// counts only at or below `last`.
#[must_use]
pub const fn archive_verdict(last: Option<u64>, seq: u64, present: bool) -> (r: ArchiveVerdict)
    ensures
        r == spec_verdict(last, seq, present),
{
    match last {
        Some(l) => {
            if seq > l {
                ArchiveVerdict::Append
            } else if present {
                ArchiveVerdict::Present
            } else {
                ArchiveVerdict::Conflict
            }
        }
        None => ArchiveVerdict::Append,
    }
}
// mirror-end archive_verdict

pub open spec fn spec_verdict(last: Option<u64>, seq: u64, present: bool) -> ArchiveVerdict {
    match last {
        Some(l) => {
            if seq > l {
                ArchiveVerdict::Append
            } else if present {
                ArchiveVerdict::Present
            } else {
                ArchiveVerdict::Conflict
            }
        }
        None => ArchiveVerdict::Append,
    }
}

/// A row of `events`, and a line of an archive file: its seq and its
/// text (abstracted to a number).
pub type Row = (u64, u64);

/// The seq of the last line of a file.
pub open spec fn last_of(f: Seq<Row>) -> Option<u64> {
    if f.len() == 0 {
        None
    } else {
        Some(f.last().0)
    }
}

/// The file holds this exact line.
pub open spec fn holds(f: Seq<Row>, row: Row) -> bool {
    exists|i: int| 0 <= i < f.len() && #[trigger] f[i] == row
}

/// One row: `None` when the archive refuses the batch.
pub open spec fn step(f: Seq<Row>, row: Row) -> Option<Seq<Row>> {
    match spec_verdict(last_of(f), row.0, holds(f, row)) {
        ArchiveVerdict::Append => Some(f.push(row)),
        ArchiveVerdict::Present => Some(f),
        ArchiveVerdict::Conflict => None,
    }
}

/// A batch, row by row; `None` when any row is refused (then nothing is
/// trimmed).
pub open spec fn append(f: Seq<Row>, b: Seq<Row>) -> Option<Seq<Row>>
    decreases b.len(),
{
    if b.len() == 0 {
        Some(f)
    } else {
        match append(f, b.drop_last()) {
            Some(g) => step(g, b.last()),
            None => None,
        }
    }
}

/// Appending after a possible refusal.
pub open spec fn then(o: Option<Seq<Row>>, b: Seq<Row>) -> Option<Seq<Row>> {
    match o {
        Some(g) => append(g, b),
        None => None,
    }
}

/// Strictly ascending seqs: no line twice, none out of order. A table
/// (by its primary key) and every archive file are.
pub open spec fn ascending(f: Seq<Row>) -> bool {
    forall|i: int, j: int| #![trigger f[i], f[j]] 0 <= i < j < f.len() ==> f[i].0 < f[j].0
}

pub proof fn lemma_step_keeps_ascending(f: Seq<Row>, row: Row)
    requires
        ascending(f),
        step(f, row) is Some,
    ensures
        ascending(step(f, row).unwrap()),
{
    if spec_verdict(last_of(f), row.0, holds(f, row)) is Append {
        let g = f.push(row);
        assert forall|i: int, j: int| #![trigger g[i], g[j]] 0 <= i < j < g.len() implies g[i].0 < g[j].0 by {
            if j == f.len() {
                if i < f.len() - 1 {
                    assert(f[i].0 < f[f.len() - 1].0);
                }
            }
        }
    }
}

/// The archive never holds a row twice, nor out of order, whatever runs,
/// crashes and re-runs built it.
pub proof fn lemma_append_keeps_ascending(f: Seq<Row>, b: Seq<Row>)
    requires
        ascending(f),
        append(f, b) is Some,
    ensures
        ascending(append(f, b).unwrap()),
    decreases b.len(),
{
    if b.len() > 0 {
        lemma_append_keeps_ascending(f, b.drop_last());
        lemma_step_keeps_ascending(append(f, b.drop_last()).unwrap(), b.last());
    }
}

/// A line once held stays held.
pub proof fn lemma_step_keeps_holds(f: Seq<Row>, row: Row, x: Row)
    requires
        holds(f, x),
        step(f, row) is Some,
    ensures
        holds(step(f, row).unwrap(), x),
{
    let i = choose|i: int| 0 <= i < f.len() && #[trigger] f[i] == x;
    let g = step(f, row).unwrap();
    assert(g[i] == x);
}

pub proof fn lemma_append_keeps_holds(f: Seq<Row>, b: Seq<Row>, x: Row)
    requires
        holds(f, x),
        append(f, b) is Some,
    ensures
        holds(append(f, b).unwrap(), x),
    decreases b.len(),
{
    if b.len() > 0 {
        lemma_append_keeps_holds(f, b.drop_last(), x);
        lemma_step_keeps_holds(append(f, b.drop_last()).unwrap(), b.last(), x);
    }
}

/// No row is lost: after a batch the archive accepted, the file holds
/// every row of it, so the delete that follows removes only archived
/// rows. Not merely "at or below the last line": the exact line.
pub proof fn lemma_every_trimmed_row_is_archived(f: Seq<Row>, b: Seq<Row>)
    requires
        append(f, b) is Some,
    ensures
        forall|k: int| 0 <= k < b.len() ==> holds(append(f, b).unwrap(), #[trigger] b[k]),
    decreases b.len(),
{
    if b.len() > 0 {
        let p = b.drop_last();
        let g = append(f, p).unwrap();
        let h = append(f, b).unwrap();
        lemma_every_trimmed_row_is_archived(f, p);
        // The new row itself.
        if spec_verdict(last_of(g), b.last().0, holds(g, b.last())) is Append {
            assert(h[g.len() as int] == b.last());
        }
        assert forall|k: int| 0 <= k < b.len() implies holds(h, #[trigger] b[k]) by {
            if k < b.len() - 1 {
                assert(b[k] == p[k]);
                assert(holds(g, p[k]));
                lemma_step_keeps_holds(g, b.last(), b[k]);
            }
        }
    }
}

/// A batch whose every row the file already holds appends nothing and
/// is not refused.
pub proof fn lemma_held_batch_appends_nothing(f: Seq<Row>, b: Seq<Row>)
    requires
        ascending(f),
        forall|k: int| 0 <= k < b.len() ==> holds(f, #[trigger] b[k]),
    ensures
        append(f, b) == Some(f),
    decreases b.len(),
{
    if b.len() > 0 {
        let p = b.drop_last();
        assert forall|k: int| 0 <= k < p.len() implies holds(f, #[trigger] p[k]) by {
            assert(p[k] == b[k]);
        }
        lemma_held_batch_appends_nothing(f, p);
        let x = b.last();
        assert(holds(f, b[b.len() - 1]));
        let i = choose|i: int| 0 <= i < f.len() && #[trigger] f[i] == x;
        if i < f.len() - 1 {
            assert(f[i].0 < f[f.len() - 1].0);
        }
        assert(x.0 <= f.last().0);
    }
}

/// Appending `a + b` is appending `a`, then `b`.
pub proof fn lemma_append_concat(f: Seq<Row>, a: Seq<Row>, b: Seq<Row>)
    ensures
        append(f, a + b) == then(append(f, a), b),
    decreases b.len(),
{
    if b.len() == 0 {
        assert(a + b =~= a);
        match append(f, a) {
            Some(g) => {},
            None => {},
        }
    } else {
        assert((a + b).drop_last() =~= a + b.drop_last());
        assert((a + b).last() == b.last());
        lemma_append_concat(f, a, b.drop_last());
    }
}

/// A crash after the first `k` rows of a batch were written (and synced),
/// before the trim committed, then a re-run of the whole batch: the file
/// is the one a single clean run writes, and the re-run is not refused.
/// The trim then deletes the batch once.
pub proof fn lemma_crash_rerun_is_idempotent(f: Seq<Row>, b: Seq<Row>, k: int)
    requires
        0 <= k <= b.len(),
        ascending(f),
        append(f, b) is Some,
    ensures
        append(f, b.subrange(0, k)) is Some,
        append(append(f, b.subrange(0, k)).unwrap(), b) == append(f, b),
{
    let head = b.subrange(0, k);
    let tail = b.subrange(k, b.len() as int);
    assert(b =~= head + tail);
    lemma_append_concat(f, head, tail);
    let g = append(f, head).unwrap();
    lemma_append_keeps_ascending(f, head);
    lemma_every_trimmed_row_is_archived(f, head);
    lemma_held_batch_appends_nothing(g, head);
    lemma_append_concat(g, head, tail);
}

/// A batch of ascending rows all above the file's last line is written
/// whole: never refused, nothing skipped.
pub proof fn lemma_fresh_batch_appends_all(f: Seq<Row>, b: Seq<Row>)
    requires
        ascending(b),
        forall|k: int| 0 <= k < b.len() ==> spec_verdict(last_of(f), #[trigger] b[k].0, false) is Append,
    ensures
        append(f, b) == Some(f + b),
    decreases b.len(),
{
    if b.len() == 0 {
        assert(f + b =~= f);
    } else {
        let p = b.drop_last();
        assert forall|i: int, j: int| #![trigger p[i], p[j]] 0 <= i < j < p.len() implies p[i].0 < p[j].0 by {
            assert(p[i] == b[i] && p[j] == b[j]);
        }
        assert forall|k: int| 0 <= k < p.len() implies spec_verdict(last_of(f), #[trigger] p[k].0, false) is Append by {
            assert(p[k] == b[k]);
        }
        lemma_fresh_batch_appends_all(f, p);
        let x = b.last();
        assert(spec_verdict(last_of(f), b[b.len() - 1].0, false) is Append);
        if p.len() > 0 {
            assert((f + p).last() == p.last());
            assert(p.last() == b[b.len() - 2]);
        } else {
            assert(f + p =~= f);
        }
        assert((f + p).push(x) =~= f + b);
    }
}

/// The trim batch is a seq prefix of the table: the rows from the oldest
/// on, stopping before the first row too young to go. Every row left in
/// the table is then above every row of the batch, so (with
/// `lemma_fresh_batch_appends_all`) the next batch is above each file's
/// last line and is written whole: a row is never skipped as "already
/// archived" when it is not, and a row that missed one cutoff never
/// meets a file that moved past it.
pub proof fn lemma_prefix_leaves_only_higher_rows(t: Seq<Row>, n: int)
    requires
        ascending(t),
        0 <= n <= t.len(),
    ensures
        forall|i: int, j: int| #![trigger t[i], t[j]] 0 <= i < n <= j < t.len() ==> t[i].0 < t[j].0,
        ascending(t.subrange(n, t.len() as int)),
{
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
