//! Supervisor side rows: the monotone merges the store applies.
//!
//! Mirrors `crates/mud-world/src/side.rs`. The text between the mirror
//! markers must match the Rust source; only the spec clauses differ.
//!
//! A side op is applied as an SQL upsert. The store may apply an op twice
//! (a retried save), apply two ops in either order (a deferred op behind a
//! later commit), or lose the newest ops in an OS crash. These kernels make
//! that safe:
//! - `macro_queue` and `macro_consume` are inflationary, idempotent and
//!   commute with each other and themselves, and keep `cleared <= queued`
//!   (`lemma_macro_ops_commute`, `lemma_macro_ops_idempotent`).
//! - `dated_max` is the value half of a join on dated counters: the later
//!   day wins, the larger value on the same day. With the day joined by
//!   `max` it is commutative, associative and idempotent
//!   (`lemma_dated_join_is_a_semilattice`), so upserts in any order, any
//!   number of times, converge.
//! - `rev_join` decides whether a Steward row replaces the stored one: the
//!   higher rev wins. On rows `(rev, body)` where an equal rev means an
//!   equal body (one writer per entity), the join is commutative,
//!   associative and idempotent (`lemma_rev_join_is_a_semilattice`), the
//!   stored rev never goes down, and any order of a set of writes ends at
//!   the same row, the one with the highest rev
//!   (`lemma_any_order_ends_at_the_same_row`).
//!
//! No bound: only comparisons.

use vstd::prelude::*;

verus! {

#[derive(Clone, Copy)]
// mirror-begin macro_row
/// The macro-tick counters of one entity, as `sup_entity` keeps them.
pub struct MacroRow {
    /// Macro ticks the scheduler queued (monotone).
    pub queued: u32,
    /// Queued ticks a consumed wake covered (monotone, at most `queued`).
    pub cleared: u32,
    /// Macro wakes consumed: what a script's `nth` sees (monotone).
    pub ticks: u32,
}
// mirror-end macro_row

/// `a` is below `b` in every field.
pub open spec fn below(a: MacroRow, b: MacroRow) -> bool {
    a.queued <= b.queued && a.cleared <= b.cleared && a.ticks <= b.ticks
}

/// The invariant every stored row keeps.
pub open spec fn sound(r: MacroRow) -> bool {
    r.cleared <= r.queued
}

/// One macro op: a queue (`Queue(queued)`) or a consume
/// (`Consume(macro_no, cover)`).
pub enum MacroOp {
    Queue(u32),
    Consume(u32, u32),
}

pub open spec fn max32(a: u32, b: u32) -> u32 {
    if a > b { a } else { b }
}

pub open spec fn spec_apply(r: MacroRow, op: MacroOp) -> MacroRow {
    match op {
        MacroOp::Queue(q) => MacroRow { queued: max32(r.queued, q), cleared: r.cleared, ticks: r.ticks },
        MacroOp::Consume(n, c) => MacroRow {
            queued: max32(r.queued, c),
            cleared: max32(r.cleared, c),
            ticks: max32(r.ticks, n),
        },
    }
}

// mirror-begin macro_merges
/// `MacroQueued`: the scheduler has queued `queued` ticks in all.
#[must_use]
pub const fn macro_queue(row: MacroRow, queued: u32) -> (r: MacroRow)
    ensures
        r == spec_apply(row, MacroOp::Queue(queued)),
        below(row, r),
        sound(row) ==> sound(r),
        r.queued >= queued,
{
    MacroRow {
        queued: if queued > row.queued { queued } else { row.queued },
        cleared: row.cleared,
        ticks: row.ticks,
    }
}

/// `Consume`: a wake that carried tick number `macro_no` (0: none) and
/// covered the queued ticks up to `cover` acted, or closed.
#[must_use]
pub const fn macro_consume(row: MacroRow, macro_no: u32, cover: u32) -> (r: MacroRow)
    ensures
        r == spec_apply(row, MacroOp::Consume(macro_no, cover)),
        below(row, r),
        sound(row) ==> sound(r),
        r.ticks >= macro_no,
        r.cleared >= cover,
{
    MacroRow {
        queued: if cover > row.queued { cover } else { row.queued },
        cleared: if cover > row.cleared { cover } else { row.cleared },
        ticks: if macro_no > row.ticks { macro_no } else { row.ticks },
    }
}
// mirror-end macro_merges

/// Any two macro ops commute: a deferred op that lands after a later one
/// gives the same row.
pub proof fn lemma_macro_ops_commute(r: MacroRow, a: MacroOp, b: MacroOp)
    ensures
        spec_apply(spec_apply(r, a), b) == spec_apply(spec_apply(r, b), a),
{
}

/// Applying an op twice (a retried save) is applying it once.
pub proof fn lemma_macro_ops_idempotent(r: MacroRow, a: MacroOp)
    ensures
        spec_apply(spec_apply(r, a), a) == spec_apply(r, a),
{
}

/// A run of ops from a sound row stays sound and never goes down.
pub open spec fn apply_all(r: MacroRow, ops: Seq<MacroOp>) -> MacroRow
    decreases ops.len(),
{
    if ops.len() == 0 {
        r
    } else {
        spec_apply(apply_all(r, ops.drop_last()), ops.last())
    }
}

pub proof fn lemma_runs_stay_sound_and_grow(r: MacroRow, ops: Seq<MacroOp>)
    requires
        sound(r),
    ensures
        sound(apply_all(r, ops)),
        below(r, apply_all(r, ops)),
    decreases ops.len(),
{
    if ops.len() > 0 {
        lemma_runs_stay_sound_and_grow(r, ops.drop_last());
    }
}

// mirror-begin dated_max
/// The value of the later of two dated counters, or the larger of the two
/// on the same day. The day of the result is the later day.
#[must_use]
pub const fn dated_max(a_day: u64, a: u64, b_day: u64, b: u64) -> (r: u64)
    ensures
        r == spec_dated(a_day, a, b_day, b),
{
    if a_day > b_day {
        a
    } else if b_day > a_day {
        b
    } else if a > b {
        a
    } else {
        b
    }
}
// mirror-end dated_max

// mirror-begin rev_join
/// Whether a Steward row written at rev `incoming` replaces the one stored
/// at rev `stored`: the higher rev wins, and an equal rev is the same
/// write retried, so it changes nothing.
#[must_use]
pub const fn rev_join(stored: u64, incoming: u64) -> (r: bool)
    ensures
        r == (incoming > stored),
{
    incoming > stored
}
// mirror-end rev_join

/// A stored Steward row as `(rev, body)`; the body is abstract.
pub open spec fn row_join(stored: (u64, u64), incoming: (u64, u64)) -> (u64, u64) {
    if incoming.0 > stored.0 {
        incoming
    } else {
        stored
    }
}

/// One writer per entity: two rows with one rev are one write.
pub open spec fn consistent(a: (u64, u64), b: (u64, u64)) -> bool {
    a.0 == b.0 ==> a.1 == b.1
}

/// The row join is a join-semilattice on consistent rows, and it never
/// moves the stored rev back.
pub proof fn lemma_rev_join_is_a_semilattice(a: (u64, u64), b: (u64, u64), c: (u64, u64))
    requires
        consistent(a, b),
        consistent(b, c),
        consistent(a, c),
    ensures
        row_join(a, b) == row_join(b, a),
        row_join(row_join(a, b), c) == row_join(a, row_join(b, c)),
        row_join(a, a) == a,
        row_join(a, b).0 >= a.0,
        row_join(a, b).0 >= b.0,
{
}

/// The row after `writes`, applied in order on `start`.
pub open spec fn fold_rows(start: (u64, u64), writes: Seq<(u64, u64)>) -> (u64, u64)
    decreases writes.len(),
{
    if writes.len() == 0 {
        start
    } else {
        row_join(fold_rows(start, writes.drop_last()), writes.last())
    }
}

/// The folded row is at least every write and the start, and it is one of
/// them.
pub proof fn lemma_fold_is_an_upper_element(start: (u64, u64), writes: Seq<(u64, u64)>)
    ensures
        fold_rows(start, writes).0 >= start.0,
        forall|i: int| 0 <= i < writes.len() ==> fold_rows(start, writes).0 >= #[trigger] writes[i].0,
        fold_rows(start, writes) == start || writes.contains(fold_rows(start, writes)),
    decreases writes.len(),
{
    if writes.len() > 0 {
        let init = writes.drop_last();
        lemma_fold_is_an_upper_element(start, init);
        let f = fold_rows(start, init);
        assert forall|i: int| 0 <= i < writes.len() implies fold_rows(start, writes).0
            >= #[trigger] writes[i].0 by {
            if i < writes.len() - 1 {
                assert(init[i] == writes[i]);
            }
        }
        if fold_rows(start, writes) == writes.last() {
            assert(writes[writes.len() - 1] == writes.last());
        } else if f != start {
            let j = choose|j: int| 0 <= j < init.len() && init[j] == f;
            assert(writes[j] == f);
        }
    }
}

/// Any two orders of one set of consistent writes end at the same row:
/// the write with the highest rev.
pub proof fn lemma_any_order_ends_at_the_same_row(
    start: (u64, u64),
    a: Seq<(u64, u64)>,
    b: Seq<(u64, u64)>,
)
    requires
        forall|x: (u64, u64)| a.contains(x) <==> b.contains(x),
        forall|x: (u64, u64), y: (u64, u64)|
            (x == start || a.contains(x)) && (y == start || a.contains(y)) ==> consistent(x, y),
    ensures
        fold_rows(start, a) == fold_rows(start, b),
{
    lemma_fold_is_an_upper_element(start, a);
    lemma_fold_is_an_upper_element(start, b);
    let fa = fold_rows(start, a);
    let fb = fold_rows(start, b);
    if fa != start {
        let i = choose|i: int| 0 <= i < a.len() && a[i] == fa;
        assert(b.contains(fa));
        let j = choose|j: int| 0 <= j < b.len() && b[j] == fa;
        assert(fb.0 >= b[j].0);
    }
    if fb != start {
        assert(a.contains(fb));
        let i = choose|i: int| 0 <= i < a.len() && a[i] == fb;
        assert(fa.0 >= a[i].0);
    }
    assert(fa.0 == fb.0);
    assert(consistent(fa, fb));
}

#[derive(Clone, Copy)]
// mirror-begin tally
/// One UTC day's spend through one channel.
pub struct Tally {
    /// Tokens.
    pub tokens: u64,
    /// Cost, in millionths of a dollar.
    pub cost_usd_micros: u64,
    /// Model calls.
    pub calls: u64,
}
// mirror-end tally

pub open spec fn spec_dated(a_day: u64, a: u64, b_day: u64, b: u64) -> u64 {
    if a_day > b_day {
        a
    } else if b_day > a_day {
        b
    } else if a > b {
        a
    } else {
        b
    }
}

pub open spec fn max64(a: u64, b: u64) -> u64 {
    if a > b { a } else { b }
}

/// The join of two dated counters `(day, value)`.
pub open spec fn join(a: (u64, u64), b: (u64, u64)) -> (u64, u64) {
    (max64(a.0, b.0), spec_dated(a.0, a.1, b.0, b.1))
}

/// The dated join is a join-semilattice: upserts of `Spend` and `Booked`
/// rows converge whatever their order and however often they repeat.
pub proof fn lemma_dated_join_is_a_semilattice(a: (u64, u64), b: (u64, u64), c: (u64, u64))
    ensures
        join(a, b) == join(b, a),
        join(join(a, b), c) == join(a, join(b, c)),
        join(a, a) == a,
{
}

/// The join never moves a counter back: its day is at least each day, and
/// on the joined day its value is at least each value dated that day.
pub proof fn lemma_dated_join_is_inflationary(a: (u64, u64), b: (u64, u64))
    ensures
        join(a, b).0 >= a.0,
        join(a, b).0 >= b.0,
        join(a, b).0 == a.0 ==> join(a, b).1 >= a.1,
        join(a, b).0 == b.0 ==> join(a, b).1 >= b.1,
{
}

} // verus!
