//! Supervisor persistence: what a wake consumes, what a restart restores,
//! and why a deterministic plan takes each effect exactly once.
//!
//! Mirrors `crates/mud-supervisor/src/persist.rs`. The text between the
//! mirror markers must match the Rust source; only the spec clauses differ.
//!
//! - `macro_open`: a wake that carries a macro tick, with one owed,
//!   carries number `ticks + 1` and covers every queued tick; any other
//!   wake changes nothing. Consuming an open grows `ticks` by at most one
//!   and keeps `cleared <= queued`. Each tick number is carried by at most
//!   one committed consume (`lemma_committed_tick_numbers_are_distinct`).
//! - `next_booking_id`: strictly above every id the meta row or a stored
//!   row holds, so a restart never reissues an id
//!   (`lemma_restart_never_reissues_a_booking_id`).
//! - `booking_disposition`: total; `Wait` exactly before the due time,
//!   `Fire` from it to `max_late` after it, `Drop` past that.
//! - `restore_macro`: never before `now`; a time inside the window stands;
//!   a later one is clamped; a past one fires once at `now + pick`; a
//!   second restore at the same time changes nothing
//!   (`lemma_restore_is_stable`).
//! - `plan_step`, `plan_resume`: over every history of runs, crashes and
//!   resumes, the durable cursor never moves back, each line's effect
//!   commits at most once, a line that took effect never runs again, and a
//!   plan run to its end ran every line (`lemma_plan_exactly_once`).
//! - `wake_id_key`: injective; with boots that grow and a counter that
//!   grows within a boot, no key repeats
//!   (`lemma_wake_keys_never_repeat`).
//!
//! - `budget_join`, `roll_day`, `budget_spend`, `budget_used`: a day's
//!   spend, split into the supervisor's own tally and littleguy's ledger
//!   tally. The join is a join-semilattice
//!   (`lemma_budget_join_is_a_semilattice`) and agrees field by field with
//!   the store's dated join (`lemma_budget_join_is_the_dated_join`); it is
//!   the least upper bound of the dated order
//!   (`lemma_budget_join_is_least_upper_bound`). `roll_day` never lowers the
//!   day and zeroes only on an increase; a spend never goes down in the
//!   order. So the stored budget, the join of any of the in-memory budgets
//!   written in any order, never passes the budget in memory
//!   (`lemma_stored_never_passes_memory`), and on one day the breaker's
//!   count grows with it (`lemma_used_grows_with_the_order`).
//! - `restore_lg`: with the stored tally and the ledger's both at most the
//!   truth, the result is at most the truth and at least each
//!   (`lemma_restore_lg_stays_below_the_truth`).
//! - `restore_budget`, `prime_budget`: the boot keeps the same day and the
//!   next, zeroes an earlier day, resets a later one, and priming only
//!   raises the ledger tally of today.
//!
//! No bound: comparisons, and additions guarded against overflow.

use vstd::prelude::*;

use crate::side::{MacroOp, MacroRow, Tally, below, max64, sound, spec_apply, spec_dated};

verus! {

#[derive(Clone, Copy)]
// mirror-begin macro_open_struct
/// What a wake that opens now consumes of its entity's macro counters.
pub struct MacroOpen {
    /// The tick number the wake carries; 0 when it carries none.
    pub macro_no: u32,
    /// The queued ticks it covers: `cleared` once it is consumed.
    pub cover: u32,
}
// mirror-end macro_open_struct

pub open spec fn spec_open(row: MacroRow, carries_tick: bool) -> MacroOpen {
    if carries_tick && row.queued > row.cleared {
        MacroOpen {
            macro_no: if row.ticks < u32::MAX { (row.ticks + 1) as u32 } else { 0 },
            cover: row.queued,
        }
    } else {
        MacroOpen { macro_no: 0, cover: row.cleared }
    }
}

// mirror-begin macro_open
/// Opens a wake on `row`.
///
/// A wake that carries a macro tick, with a tick
/// owed (`queued > cleared`), carries number `ticks + 1` and covers every
/// queued tick. Any other wake carries no number and covers only what is
/// cleared already, so consuming it changes nothing. At `u32::MAX` ticks
/// no number is left: the owed tick is covered without one.
#[must_use]
pub const fn macro_open(row: MacroRow, carries_tick: bool) -> (r: MacroOpen)
    ensures
        r == spec_open(row, carries_tick),
        r.macro_no > 0 ==> r.macro_no == row.ticks + 1 && carries_tick && row.queued > row.cleared,
        sound(row) ==> row.cleared <= r.cover <= row.queued,
        sound(row) ==> sound(spec_apply(row, MacroOp::Consume(r.macro_no, r.cover))),
        sound(row) ==> spec_apply(row, MacroOp::Consume(r.macro_no, r.cover)).ticks <= row.ticks + 1,
        sound(row) ==> spec_apply(row, MacroOp::Consume(r.macro_no, r.cover)).queued == row.queued,
        sound(row) && !carries_tick ==> spec_apply(row, MacroOp::Consume(r.macro_no, r.cover)) == row,
{
    if carries_tick && row.queued > row.cleared {
        MacroOpen {
            macro_no: if row.ticks < u32::MAX {
                row.ticks + 1
            } else {
                0
            },
            cover: row.queued,
        }
    } else {
        MacroOpen {
            macro_no: 0,
            cover: row.cleared,
        }
    }
}
// mirror-end macro_open

/// The tick number an open of `row` carries.
pub open spec fn opened_no(row: MacroRow, carries: bool) -> u32 {
    spec_open(row, carries).macro_no
}

/// The row after the consume of an open of `row`.
pub open spec fn consumed(row: MacroRow, carries: bool) -> MacroRow {
    spec_apply(row, MacroOp::Consume(spec_open(row, carries).macro_no, spec_open(row, carries).cover))
}

/// Committed consumes, in commit order: the open of each read a row that
/// already held the consume before it (one turn per entity, and a restart
/// reads the durable rows, which hold every committed consume).
pub open spec fn commit_order(rows: Seq<MacroRow>, carries: Seq<bool>) -> bool {
    rows.len() == carries.len()
    && forall|i: int| 0 <= i < rows.len() - 1 ==> below(#[trigger] consumed(rows[i], carries[i]), rows[i + 1])
}

proof fn lemma_ticks_pass(rows: Seq<MacroRow>, carries: Seq<bool>, i: int, j: int)
    requires
        commit_order(rows, carries),
        0 <= i < j < rows.len(),
    ensures
        rows[j].ticks >= opened_no(rows[i], carries[i]),
        rows[j].ticks >= rows[i].ticks,
    decreases j - i,
{
    assert(below(consumed(rows[j - 1], carries[j - 1]), rows[j]));
    if j - 1 > i {
        lemma_ticks_pass(rows, carries, i, j - 1);
    } else {
        assert(below(consumed(rows[i], carries[i]), rows[i + 1]));
    }
}

/// Each macro tick number is carried by at most one committed consume:
/// along committed consumes, the nonzero numbers strictly grow.
pub proof fn lemma_committed_tick_numbers_are_distinct(rows: Seq<MacroRow>, carries: Seq<bool>)
    requires
        commit_order(rows, carries),
    ensures
        forall|i: int, j: int|
            #![trigger opened_no(rows[i], carries[i]), opened_no(rows[j], carries[j])]
            0 <= i < j < rows.len() && opened_no(rows[i], carries[i]) > 0 && opened_no(
                rows[j],
                carries[j],
            ) > 0 ==> opened_no(rows[i], carries[i]) < opened_no(rows[j], carries[j]),
{
    assert forall|i: int, j: int|
        #![trigger opened_no(rows[i], carries[i]), opened_no(rows[j], carries[j])]
        0 <= i < j < rows.len() && opened_no(rows[i], carries[i]) > 0 && opened_no(
            rows[j],
            carries[j],
        ) > 0 implies opened_no(rows[i], carries[i]) < opened_no(rows[j], carries[j]) by {
        lemma_ticks_pass(rows, carries, i, j);
    }
}

// mirror-begin next_booking_id
/// The next booking id after a restart: one more than the larger of the
/// highest id the meta row recorded and the highest id still stored.
#[must_use]
pub const fn next_booking_id(meta: u64, max_id: u64) -> (r: u64)
    ensures
        r as int == spec_next_id(meta, max_id),
        r >= meta && r >= max_id,
        meta < u64::MAX && max_id < u64::MAX ==> r > meta && r > max_id,
{
    let m = if meta > max_id { meta } else { max_id };
    if m < u64::MAX {
        m + 1
    } else {
        u64::MAX
    }
}
// mirror-end next_booking_id

/// Every id issued before a restart was recorded in the meta row, in the
/// transaction that stored its booking, before its "ok". The first id after
/// the restart is above it, and the ids after that grow from there.
pub proof fn lemma_restart_never_reissues_a_booking_id(issued: u64, meta: u64, max_id: u64)
    requires
        issued <= meta,
        meta < u64::MAX,
        max_id < u64::MAX,
    ensures
        spec_next_id(meta, max_id) > issued,
{
}

pub open spec fn spec_next_id(meta: u64, max_id: u64) -> int {
    let m = if meta > max_id { meta } else { max_id };
    if m < u64::MAX {
        m + 1
    } else {
        u64::MAX as int
    }
}

// mirror-begin booking_disposition_enum
/// What a booking restored at boot does.
pub enum BookingDisposition {
    /// Not due yet: it waits.
    Wait,
    /// Due, and not too late: it fires when its entity is ready.
    Fire,
    /// Overdue by more than the allowed lateness: it is dropped.
    Drop,
}
// mirror-end booking_disposition_enum

// mirror-begin booking_disposition
/// The disposition of a booking due at `due_ms`, restored at `now_ms`,
/// that may fire at most `max_late_ms` late.
#[must_use]
pub const fn booking_disposition(due_ms: u64, now_ms: u64, max_late_ms: u64) -> (r: BookingDisposition)
    ensures
        (r is Wait) <==> now_ms < due_ms,
        (r is Fire) <==> due_ms <= now_ms && now_ms as int <= due_ms as int + max_late_ms as int,
        (r is Drop) <==> now_ms as int > due_ms as int + max_late_ms as int,
{
    if now_ms < due_ms {
        BookingDisposition::Wait
    } else if now_ms - due_ms <= max_late_ms {
        BookingDisposition::Fire
    } else {
        BookingDisposition::Drop
    }
}
// mirror-end booking_disposition

pub open spec fn spec_restore(next_ms: u64, now_ms: u64, hi_ms: u64, pick_ms: u64) -> int {
    if next_ms < now_ms {
        if now_ms as int + pick_ms as int <= u64::MAX as int {
            now_ms as int + pick_ms as int
        } else {
            u64::MAX as int
        }
    } else if next_ms as int - now_ms as int <= hi_ms as int {
        next_ms as int
    } else {
        now_ms as int + hi_ms as int
    }
}

// mirror-begin restore_macro
/// The next macro tick after a restart, from the stored time `next_ms`.
///
/// A time from `now_ms` to `now_ms + hi_ms` stands. A later one (the clock
/// went back) is clamped to `now_ms + hi_ms`. A past one fires once, at
/// `now_ms + pick_ms`, a pick within the catch-up spread, however many
/// ticks the outage missed.
#[must_use]
pub const fn restore_macro(next_ms: u64, now_ms: u64, hi_ms: u64, pick_ms: u64) -> (r: u64)
    ensures
        r as int == spec_restore(next_ms, now_ms, hi_ms, pick_ms),
        r >= now_ms,
        now_ms <= next_ms && next_ms as int <= now_ms as int + hi_ms as int ==> r == next_ms,
        next_ms as int > now_ms as int + hi_ms as int ==> r as int == now_ms as int + hi_ms as int,
        next_ms < now_ms && now_ms as int + pick_ms as int <= u64::MAX as int ==> r as int
            == now_ms as int + pick_ms as int,
        r as int <= now_ms as int + hi_ms as int || r as int <= now_ms as int + pick_ms as int,
{
    if next_ms < now_ms {
        if pick_ms <= u64::MAX - now_ms {
            now_ms + pick_ms
        } else {
            u64::MAX
        }
    } else if next_ms - now_ms <= hi_ms {
        next_ms
    } else {
        now_ms + hi_ms
    }
}
// mirror-end restore_macro

/// A second restart at the same time changes nothing: the catch-up time
/// it chose stands (when the spread is within the window).
pub proof fn lemma_restore_is_stable(next_ms: u64, now_ms: u64, hi_ms: u64, pick_ms: u64, again: u64)
    requires
        pick_ms <= hi_ms,
    ensures
        spec_restore(spec_restore(next_ms, now_ms, hi_ms, pick_ms) as u64, now_ms, hi_ms, again)
            == spec_restore(next_ms, now_ms, hi_ms, pick_ms),
{
}

// mirror-begin plan_step
/// The durable cursor of a plan after line `line` ran from `cursor`.
///
/// It moves past the line when the line took effect: its cursor commits
/// in the same transaction. It stays when the line changed nothing, which
/// commits nothing.
#[must_use]
pub const fn plan_step(cursor: u32, line: u32, effectful: bool) -> (r: u32)
    ensures
        r >= cursor,
        line < u32::MAX ==> r as int == spec_step(cursor as nat, line as nat, effectful),
{
    if effectful && line >= cursor && line < u32::MAX {
        line + 1
    } else {
        cursor
    }
}
// mirror-end plan_step

// mirror-begin plan_resume
/// The first line a resumed plan runs: its durable cursor, at most `len`.
/// A plan whose cursor reached `len` is done.
#[must_use]
pub const fn plan_resume(cursor: u32, len: u32) -> (r: u32)
    ensures
        r as nat == spec_resume(cursor as nat, len as nat),
        r <= len,
        r <= cursor,
{
    if cursor < len {
        cursor
    } else {
        len
    }
}
// mirror-end plan_resume

pub open spec fn spec_step(cursor: nat, line: nat, effectful: bool) -> nat {
    if effectful && line >= cursor {
        line + 1
    } else {
        cursor
    }
}

pub open spec fn spec_resume(cursor: nat, len: nat) -> nat {
    if cursor < len {
        cursor
    } else {
        len
    }
}

/// One step of a plan's history: line `i` runs (and takes effect or not),
/// or the process crashes and the plan resumes from its durable cursor.
pub enum PlanStep {
    Run(nat, bool),
    Crash,
}

/// The durable cursor, and the line the runner in memory runs next.
pub struct PlanState {
    pub durable: nat,
    pub next: nat,
}

pub open spec fn plan_apply(s: PlanState, st: PlanStep, len: nat) -> PlanState {
    match st {
        PlanStep::Run(i, e) => PlanState { durable: spec_step(s.durable, i, e), next: i + 1 },
        PlanStep::Crash => PlanState { durable: s.durable, next: spec_resume(s.durable, len) },
    }
}

pub open spec fn plan_state(h: Seq<PlanStep>, len: nat) -> PlanState
    decreases h.len(),
{
    if h.len() == 0 {
        PlanState { durable: 0, next: 0 }
    } else {
        plan_apply(plan_state(h.drop_last(), len), h.last(), len)
    }
}

/// A step the runner can take: the next line, while lines remain; a crash
/// at any time.
pub open spec fn step_ok(s: PlanState, st: PlanStep, len: nat) -> bool {
    match st {
        PlanStep::Run(i, _) => i == s.next && i < len,
        PlanStep::Crash => true,
    }
}

pub open spec fn plan_valid(h: Seq<PlanStep>, len: nat) -> bool
    decreases h.len(),
{
    h.len() == 0 || (plan_valid(h.drop_last(), len) && step_ok(
        plan_state(h.drop_last(), len),
        h.last(),
        len,
    ))
}

/// How often line `j` took effect.
pub open spec fn effects(h: Seq<PlanStep>, j: nat) -> nat
    decreases h.len(),
{
    if h.len() == 0 {
        0
    } else {
        effects(h.drop_last(), j) + match h.last() {
            PlanStep::Run(i, e) => if i == j && e { 1nat } else { 0nat },
            PlanStep::Crash => 0nat,
        }
    }
}

/// How often line `j` ran.
pub open spec fn runs(h: Seq<PlanStep>, j: nat) -> nat
    decreases h.len(),
{
    if h.len() == 0 {
        0
    } else {
        runs(h.drop_last(), j) + match h.last() {
            PlanStep::Run(i, _) => if i == j { 1nat } else { 0nat },
            PlanStep::Crash => 0nat,
        }
    }
}

pub open spec fn plan_inv(h: Seq<PlanStep>, len: nat) -> bool {
    let s = plan_state(h, len);
    s.durable <= s.next && s.next <= len && forall|j: nat|
        #![trigger effects(h, j)]
        #![trigger runs(h, j)]
        effects(h, j) <= 1 && (j >= s.durable ==> effects(h, j) == 0) && (j < s.next ==> runs(
            h,
            j,
        ) >= 1)
}

/// Exactly once, over every history of runs, crashes and resumes:
/// - the durable cursor never passes the runner and never passes `len`;
/// - each line's effect commits at most once, and none at or past the
///   durable cursor has committed, so a resume never repeats an effect;
/// - every line before the runner ran, so a plan run to `len` ran every
///   line: an effectful line took effect exactly once.
pub proof fn lemma_plan_exactly_once(h: Seq<PlanStep>, len: nat)
    requires
        plan_valid(h, len),
    ensures
        plan_inv(h, len),
    decreases h.len(),
{
    if h.len() > 0 {
        let p = h.drop_last();
        lemma_plan_exactly_once(p, len);
        let s = plan_state(p, len);
        let t = plan_state(h, len);
        assert forall|j: nat|
            #![trigger effects(h, j)]
            #![trigger runs(h, j)]
            effects(h, j) <= 1 && (j >= t.durable ==> effects(h, j) == 0) && (j < t.next ==> runs(
                h,
                j,
            ) >= 1) by {
            assert(effects(p, j) <= 1);
            assert(runs(p, j) >= 0);
        }
    }
}

/// The durable cursor never moves back.
pub proof fn lemma_cursor_is_monotone(h: Seq<PlanStep>, st: PlanStep, len: nat)
    ensures
        plan_state(h.push(st), len).durable >= plan_state(h, len).durable,
{
    assert(h.push(st).drop_last() =~= h);
}

/// A line that took effect never runs again: the runner may run line `j`
/// only while its effect count is 0. Only lines that changed nothing run
/// more than once.
pub proof fn lemma_no_run_after_effect(h: Seq<PlanStep>, j: nat, e: bool, len: nat)
    requires
        plan_valid(h.push(PlanStep::Run(j, e)), len),
    ensures
        effects(h, j) == 0,
{
    assert(h.push(PlanStep::Run(j, e)).drop_last() =~= h);
    lemma_plan_exactly_once(h, len);
}

#[derive(Clone, Copy)]
// mirror-begin wake_key
/// What makes a wake id unique: the `world.db` epoch, the boot of the
/// process and a counter within the boot.
pub struct WakeKey {
    /// The epoch, minted once per `world.db`.
    pub epoch: u64,
    /// The boot, one more at every open of the store.
    pub boot: u64,
    /// The wake's number within the boot.
    pub n: u64,
}
// mirror-end wake_key

// mirror-begin wake_id_key
/// The key of wake `n` of boot `boot` of a world of epoch `epoch`.
#[must_use]
pub const fn wake_id_key(epoch: u64, boot: u64, n: u64) -> (r: WakeKey)
    ensures
        r.epoch == epoch && r.boot == boot && r.n == n,
{
    WakeKey { epoch, boot, n }
}
// mirror-end wake_id_key

/// The key is injective.
pub proof fn lemma_wake_id_key_is_injective(a: WakeKey, b: WakeKey)
    requires
        a.epoch == b.epoch && a.boot == b.boot && a.n == b.n,
    ensures
        a == b,
{
}

/// Keys issued in order: a later boot, or the same boot and a later number.
pub open spec fn issued_in_order(boots: Seq<u64>, ns: Seq<u64>) -> bool {
    boots.len() == ns.len() && forall|i: int|
        0 <= i < boots.len() - 1 ==> (#[trigger] boots[i + 1] > boots[i] || (boots[i + 1]
            == boots[i] && ns[i + 1] > ns[i]))
}

pub open spec fn key_before(b1: u64, n1: u64, b2: u64, n2: u64) -> bool {
    b1 < b2 || (b1 == b2 && n1 < n2)
}

proof fn lemma_keys_increase(boots: Seq<u64>, ns: Seq<u64>, i: int, j: int)
    requires
        issued_in_order(boots, ns),
        0 <= i < j < boots.len(),
    ensures
        key_before(boots[i], ns[i], boots[j], ns[j]),
    decreases j - i,
{
    let k = j - 1;
    assert(boots[k + 1] > boots[k] || (boots[k + 1] == boots[k] && ns[k + 1] > ns[k]));
    if j - 1 > i {
        lemma_keys_increase(boots, ns, i, j - 1);
    }
}

/// Within one epoch, boots grow at every open of the store (durably,
/// before any wake) and the counter grows within a boot, so no two wakes
/// ever share a key: no wake id repeats across restarts.
pub proof fn lemma_wake_keys_never_repeat(epoch: u64, boots: Seq<u64>, ns: Seq<u64>)
    requires
        issued_in_order(boots, ns),
    ensures
        forall|i: int, j: int|
            #![auto]
            0 <= i < j < boots.len() ==> (WakeKey { epoch, boot: boots[i], n: ns[i] }) != (WakeKey {
                epoch,
                boot: boots[j],
                n: ns[j],
            }),
{
    assert forall|i: int, j: int| #![auto] 0 <= i < j < boots.len() implies (WakeKey {
        epoch,
        boot: boots[i],
        n: ns[i],
    }) != (WakeKey { epoch, boot: boots[j], n: ns[j] }) by {
        lemma_keys_increase(boots, ns, i, j);
    }
}

// ---- budgets (phase 3) ----

#[derive(Clone, Copy)]
// mirror-begin day_spend
/// One entity's spend on one UTC day, by where it was metered.
pub struct DaySpend {
    /// The UTC day (days since the epoch).
    pub day: u64,
    /// Spend the supervisor metered itself.
    pub local: Tally,
    /// Spend read from littleguy's ledger.
    pub lg: Tally,
}
// mirror-end day_spend

/// `a` is at most `b` in every field.
pub open spec fn tle(a: Tally, b: Tally) -> bool {
    a.tokens <= b.tokens && a.cost_usd_micros <= b.cost_usd_micros && a.calls <= b.calls
}

/// The dated order: an earlier day, or the same day and every field at
/// most.
pub open spec fn le(a: DaySpend, b: DaySpend) -> bool {
    a.day < b.day || (a.day == b.day && tle(a.local, b.local) && tle(a.lg, b.lg))
}

pub open spec fn zero() -> Tally {
    Tally { tokens: 0, cost_usd_micros: 0, calls: 0 }
}

pub open spec fn spec_tmax(a: Tally, b: Tally) -> Tally {
    Tally {
        tokens: max64(a.tokens, b.tokens),
        cost_usd_micros: max64(a.cost_usd_micros, b.cost_usd_micros),
        calls: max64(a.calls, b.calls),
    }
}

pub open spec fn sat(a: u64, b: u64) -> u64 {
    if a + b <= u64::MAX { (a + b) as u64 } else { u64::MAX }
}

pub open spec fn spec_join(a: DaySpend, b: DaySpend) -> DaySpend {
    if a.day > b.day {
        a
    } else if b.day > a.day {
        b
    } else {
        DaySpend { day: a.day, local: spec_tmax(a.local, b.local), lg: spec_tmax(a.lg, b.lg) }
    }
}

pub open spec fn spec_used(s: DaySpend) -> u64 {
    sat(s.local.tokens, s.lg.tokens)
}

// mirror-begin tally_kernels
/// No spend.
#[must_use]
pub const fn tally_zero() -> (r: Tally)
    ensures
        r == zero(),
{
    Tally {
        tokens: 0,
        cost_usd_micros: 0,
        calls: 0,
    }
}

/// Each field's larger value: the join of two tallies of one day.
#[must_use]
pub const fn tally_max(a: Tally, b: Tally) -> (r: Tally)
    ensures
        r == spec_tmax(a, b),
        tle(a, r),
        tle(b, r),
{
    Tally {
        tokens: if a.tokens > b.tokens {
            a.tokens
        } else {
            b.tokens
        },
        cost_usd_micros: if a.cost_usd_micros > b.cost_usd_micros {
            a.cost_usd_micros
        } else {
            b.cost_usd_micros
        },
        calls: if a.calls > b.calls { a.calls } else { b.calls },
    }
}

/// A tally with the spend `add` added, each field saturating.
#[must_use]
pub const fn tally_add(t: Tally, add: Tally) -> (r: Tally)
    ensures
        r.tokens == sat(t.tokens, add.tokens),
        r.cost_usd_micros == sat(t.cost_usd_micros, add.cost_usd_micros),
        r.calls == sat(t.calls, add.calls),
        tle(t, r),
{
    Tally {
        tokens: if add.tokens <= u64::MAX - t.tokens {
            t.tokens + add.tokens
        } else {
            u64::MAX
        },
        cost_usd_micros: if add.cost_usd_micros <= u64::MAX - t.cost_usd_micros {
            t.cost_usd_micros + add.cost_usd_micros
        } else {
            u64::MAX
        },
        calls: if add.calls <= u64::MAX - t.calls {
            t.calls + add.calls
        } else {
            u64::MAX
        },
    }
}
// mirror-end tally_kernels

// mirror-begin budget_join
/// The join of two dated spends: the later day wins whole; on the same
/// day, each field of each tally takes the larger value. A `Spend` upsert
/// merges this way.
#[must_use]
pub const fn budget_join(a: DaySpend, b: DaySpend) -> (r: DaySpend)
    ensures
        r == spec_join(a, b),
{
    if a.day > b.day {
        a
    } else if b.day > a.day {
        b
    } else {
        DaySpend {
            day: a.day,
            local: tally_max(a.local, b.local),
            lg: tally_max(a.lg, b.lg),
        }
    }
}
// mirror-end budget_join

// mirror-begin roll_day
/// The spend as of UTC day `day`: zeroed on a later day, else unchanged.
/// It never rolls back: after the clock went back, the later day stays.
#[must_use]
pub const fn roll_day(s: DaySpend, day: u64) -> (r: DaySpend)
    ensures
        r.day == max64(s.day, day),
        day > s.day ==> r.local == zero() && r.lg == zero(),
        day <= s.day ==> r == s,
        le(s, r),
{
    if day > s.day {
        DaySpend {
            day,
            local: tally_zero(),
            lg: tally_zero(),
        }
    } else {
        s
    }
}
// mirror-end roll_day

// mirror-begin budget_spend
/// Records the spend `add` on UTC day `day`, read from littleguy's ledger
/// (`ledger`) or metered by the supervisor.
#[must_use]
pub const fn budget_spend(s: DaySpend, day: u64, ledger: bool, add: Tally) -> (r: DaySpend)
    ensures
        r.day == max64(s.day, day),
        le(s, r),
        ledger ==> r.local == roll(s, day).local,
        !ledger ==> r.lg == roll(s, day).lg,
{
    let r = roll_day(s, day);
    if ledger {
        DaySpend {
            day: r.day,
            local: r.local,
            lg: tally_add(r.lg, add),
        }
    } else {
        DaySpend {
            day: r.day,
            local: tally_add(r.local, add),
            lg: r.lg,
        }
    }
}
// mirror-end budget_spend

pub open spec fn roll(s: DaySpend, day: u64) -> DaySpend {
    if day > s.day { DaySpend { day, local: zero(), lg: zero() } } else { s }
}

// mirror-begin budget_used
/// The tokens the breaker counts: both tallies, saturating.
#[must_use]
pub const fn budget_used(s: DaySpend) -> (r: u64)
    ensures
        r == spec_used(s),
{
    if s.lg.tokens <= u64::MAX - s.local.tokens {
        s.local.tokens + s.lg.tokens
    } else {
        u64::MAX
    }
}
// mirror-end budget_used

// mirror-begin restore_lg
/// The ledger tally after a restart: each field's larger value.
///
/// The inputs are the stored tally and what the ledger itself holds of
/// today. Both are at most the truth, so the result is too: no ledger row
/// is billed twice, and a row the store missed (a crash before its
/// `Spend`) or the ledger dropped (littleguy purged it) still counts.
#[must_use]
pub const fn restore_lg(stored: Tally, ledger: Tally) -> (r: Tally)
    ensures
        r == spec_tmax(stored, ledger),
        tle(stored, r),
        tle(ledger, r),
{
    tally_max(stored, ledger)
}
// mirror-end restore_lg

// mirror-begin restore_budget
/// The spend restored at boot on UTC day `today` from the stored row.
///
/// The same day stands. An earlier day is over: zeroed. One day ahead
/// (the clock went back across midnight) stands: its counters still apply
/// on the real next day, and roll the day after. More than one day ahead
/// is reset to `today`.
#[must_use]
pub const fn restore_budget(row: DaySpend, today: u64) -> (r: DaySpend)
    ensures
        r.day >= today,
        row.day == today || row.day == today + 1 ==> r == row,
        row.day < today ==> r.day == today && r.local == zero() && r.lg == zero() && le(row, r),
        row.day > today + 1 ==> r.day == today && r.local == zero() && r.lg == zero(),
{
    if row.day < today || row.day - today > 1 {
        DaySpend {
            day: today,
            local: tally_zero(),
            lg: tally_zero(),
        }
    } else {
        row
    }
}
// mirror-end restore_budget

// mirror-begin prime_budget
/// The spend after the ledger was read at boot: on `today`, the ledger
/// tally rises to [`restore_lg`] of it and the ledger's `today`; a spend of
/// another day is left as it is.
#[must_use]
pub const fn prime_budget(s: DaySpend, today: u64, ledger: Tally) -> (r: DaySpend)
    ensures
        r.day == s.day,
        r.local == s.local,
        le(s, r),
        s.day == today ==> r.lg == spec_tmax(s.lg, ledger),
        s.day != today ==> r == s,
{
    if s.day == today {
        DaySpend {
            day: s.day,
            local: s.local,
            lg: restore_lg(s.lg, ledger),
        }
    } else {
        s
    }
}
// mirror-end prime_budget

/// The join is a join-semilattice: `Spend` upserts converge whatever their
/// order and however often they repeat.
pub proof fn lemma_budget_join_is_a_semilattice(a: DaySpend, b: DaySpend, c: DaySpend)
    ensures
        spec_join(a, b) == spec_join(b, a),
        spec_join(spec_join(a, b), c) == spec_join(a, spec_join(b, c)),
        spec_join(a, a) == a,
{
}

/// The kernel's join is the store's: the later day, and each field joined
/// by `dated_max` (`crate::side`), which the `Spend` upsert runs in SQL.
pub proof fn lemma_budget_join_is_the_dated_join(a: DaySpend, b: DaySpend)
    ensures
        spec_join(a, b).day == max64(a.day, b.day),
        spec_join(a, b).local.tokens == spec_dated(a.day, a.local.tokens, b.day, b.local.tokens),
        spec_join(a, b).local.cost_usd_micros == spec_dated(
            a.day,
            a.local.cost_usd_micros,
            b.day,
            b.local.cost_usd_micros,
        ),
        spec_join(a, b).local.calls == spec_dated(a.day, a.local.calls, b.day, b.local.calls),
        spec_join(a, b).lg.tokens == spec_dated(a.day, a.lg.tokens, b.day, b.lg.tokens),
        spec_join(a, b).lg.cost_usd_micros == spec_dated(
            a.day,
            a.lg.cost_usd_micros,
            b.day,
            b.lg.cost_usd_micros,
        ),
        spec_join(a, b).lg.calls == spec_dated(a.day, a.lg.calls, b.day, b.lg.calls),
{
}

/// The dated order is transitive.
pub proof fn lemma_le_is_transitive(a: DaySpend, b: DaySpend, c: DaySpend)
    requires
        le(a, b),
        le(b, c),
    ensures
        le(a, c),
{
}

/// The join is the least upper bound of the dated order.
pub proof fn lemma_budget_join_is_least_upper_bound(a: DaySpend, b: DaySpend, m: DaySpend)
    ensures
        le(a, spec_join(a, b)),
        le(b, spec_join(a, b)),
        le(a, m) && le(b, m) ==> le(spec_join(a, b), m),
{
}

/// A history of the budget in memory: each state at least the one before
/// (a spend, a roll, a priming).
pub open spec fn chain(hist: Seq<DaySpend>) -> bool {
    forall|i: int| 0 <= i && i + 1 < hist.len() ==> le(#[trigger] hist[i], hist[i + 1])
}

proof fn lemma_chain_below_last(hist: Seq<DaySpend>, i: int)
    requires
        chain(hist),
        0 <= i < hist.len(),
    ensures
        le(hist[i], hist[hist.len() - 1]),
    decreases hist.len() - i,
{
    if i + 1 < hist.len() {
        lemma_chain_below_last(hist, i + 1);
        lemma_le_is_transitive(hist[i], hist[i + 1], hist[hist.len() - 1]);
    }
}

/// The stored row after the `Spend` writes `writes`, each a snapshot
/// `hist[k]` of the budget in memory, applied in the order given.
pub open spec fn stored(start: DaySpend, hist: Seq<DaySpend>, writes: Seq<int>) -> DaySpend
    decreases writes.len(),
{
    if writes.len() == 0 {
        start
    } else {
        spec_join(stored(start, hist, writes.drop_last()), hist[writes.last()])
    }
}

/// The stored budget never passes the budget in memory: from a row at most
/// the first state (the boot restored from it), any subset of the states
/// written, in any order, repeated or lost, joins to at most the latest.
pub proof fn lemma_stored_never_passes_memory(start: DaySpend, hist: Seq<DaySpend>, writes: Seq<int>)
    requires
        hist.len() > 0,
        chain(hist),
        le(start, hist[0]),
        forall|j: int| 0 <= j < writes.len() ==> 0 <= #[trigger] writes[j] < hist.len(),
    ensures
        le(stored(start, hist, writes), hist[hist.len() - 1]),
    decreases writes.len(),
{
    let last = hist[hist.len() - 1];
    if writes.len() == 0 {
        lemma_chain_below_last(hist, 0);
        lemma_le_is_transitive(start, hist[0], last);
    } else {
        let w = writes.last();
        assert(0 <= w < hist.len());
        lemma_stored_never_passes_memory(start, hist, writes.drop_last());
        lemma_chain_below_last(hist, w);
        lemma_budget_join_is_least_upper_bound(stored(start, hist, writes.drop_last()), hist[w], last);
    }
}

/// On one day, more in the order is more for the breaker.
pub proof fn lemma_used_grows_with_the_order(a: DaySpend, b: DaySpend)
    requires
        le(a, b),
        a.day == b.day,
    ensures
        spec_used(a) <= spec_used(b),
{
}

/// With the stored tally and the ledger's both at most the truth, the
/// restored tally is at most the truth and at least each.
pub proof fn lemma_restore_lg_stays_below_the_truth(stored: Tally, ledger: Tally, truth: Tally)
    requires
        tle(stored, truth),
        tle(ledger, truth),
    ensures
        tle(spec_tmax(stored, ledger), truth),
        tle(stored, spec_tmax(stored, ledger)),
        tle(ledger, spec_tmax(stored, ledger)),
{
}

} // verus!
