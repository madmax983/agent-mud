//! The reward-pool kernels: what a pack pool may still give, per period.
//!
//! Mirrors `crates/mud-world/src/rewards.rs`. The text between the mirror
//! markers must match the Rust source; only the spec clauses differ.
//!
//! A pool has a budget per period (a UTC day of the game clock, or ever).
//! `pool_admit` admits an ask only whole and only within what is left;
//! `pool_roll` starts a later period from zero and never moves back;
//! `pool_period` is the period of a game time. Safety: for every sequence
//! of asks and clock readings, what a pool has spent in its period never
//! passes its budget, and every admitted ask is counted exactly.

use vstd::arithmetic::div_mod::lemma_div_is_ordered;
use vstd::prelude::*;

verus! {

// mirror-begin pool_period
/// One day of the game clock, in milliseconds.
pub const DAY_MS: u64 = 86_400_000;

/// The period of a pool at game time `now_ms`: the UTC day for a per-day
/// pool, else 0 (a pool that never refills).
#[must_use]
pub const fn pool_period(per_day: bool, now_ms: u64) -> (r: u64)
    ensures
        per_day ==> r == now_ms / DAY_MS,
        !per_day ==> r == 0,
{
    if per_day {
        now_ms / DAY_MS
    } else {
        0
    }
}
// mirror-end pool_period

/// A later game time is never an earlier period.
pub proof fn lemma_period_is_monotone(per_day: bool, a: u64, b: u64)
    requires
        a <= b,
    ensures
        (if per_day { a / DAY_MS } else { 0u64 }) <= (if per_day { b / DAY_MS } else { 0u64 }),
{
    if per_day {
        lemma_div_is_ordered(a as int, b as int, DAY_MS as int);
    }
}

// mirror-begin pool_roll
/// The period a pool counts in now, and what it has spent there.
///
/// It takes the period of the pool's last mint, what it had spent then, and
/// the period of now. A later period starts from zero; an earlier one (a
/// clock set back) keeps the later count, so setting the clock back never
/// refills a pool.
#[must_use]
pub const fn pool_roll(last: u64, spent: u64, period: u64) -> (r: (u64, u64))
    ensures
        r.0 == (if period > last { period } else { last }),
        period > last ==> r.1 == 0,
        period <= last ==> r.1 == spent,
        r.1 <= spent,
{
    if period > last {
        (period, 0)
    } else {
        (last, spent)
    }
}
// mirror-end pool_roll

// mirror-begin pool_admit
/// Admits one ask against a pool's budget for the period: whether it fits
/// in what is left, and what the pool has spent after it. Never partly,
/// never past the budget, and never an overflow.
#[must_use]
pub const fn pool_admit(spent: u64, budget: u64, ask: u64) -> (r: (bool, u64))
    ensures
        r.0 <==> (spent <= budget && ask <= budget - spent),
        r.0 ==> r.1 == spent + ask,
        r.0 ==> r.1 <= budget,
        !r.0 ==> r.1 == spent,
{
    if spent <= budget && ask <= budget - spent {
        (true, spent + ask)
    } else {
        (false, spent)
    }
}
// mirror-end pool_admit

/// The admission step over plain values.
pub open spec fn spec_admit(spent: u64, budget: u64, ask: u64) -> (bool, u64) {
    if spent <= budget && ask <= budget - spent {
        (true, (spent + ask) as u64)
    } else {
        (false, spent)
    }
}

/// What a pool has spent after a run of asks in one period, from `spent`.
pub open spec fn spent_after(spent: u64, budget: u64, asks: Seq<u64>) -> u64
    decreases asks.len(),
{
    if asks.len() == 0 {
        spent
    } else {
        spec_admit(spent_after(spent, budget, asks.drop_last()), budget, asks.last()).1
    }
}

/// The sum of the asks of a run that were admitted: what the ledger records.
pub open spec fn admitted_total(spent: u64, budget: u64, asks: Seq<u64>) -> int
    decreases asks.len(),
{
    if asks.len() == 0 {
        0
    } else {
        let before = spent_after(spent, budget, asks.drop_last());
        admitted_total(spent, budget, asks.drop_last()) + (if spec_admit(
            before,
            budget,
            asks.last(),
        ).0 {
            asks.last() as int
        } else {
            0int
        })
    }
}

/// Safety of a period: from any `spent` within the budget, every run of
/// asks keeps what the pool has spent within the budget, and it equals the
/// start plus exactly the admitted asks (the ledger counts every mint).
pub proof fn lemma_run_never_passes_budget(spent: u64, budget: u64, asks: Seq<u64>)
    requires
        spent <= budget,
    ensures
        spent <= spent_after(spent, budget, asks) <= budget,
        spent_after(spent, budget, asks) as int == spent as int + admitted_total(
            spent,
            budget,
            asks,
        ),
    decreases asks.len(),
{
    if asks.len() > 0 {
        lemma_run_never_passes_budget(spent, budget, asks.drop_last());
    }
}

/// A period counter after one clock reading and one ask: roll to the
/// period of the reading, then admit the ask.
pub open spec fn spec_step(state: (u64, u64), budget: u64, period: u64, ask: u64) -> (u64, u64) {
    let rolled = if period > state.0 { (period, 0u64) } else { state };
    (rolled.0, spec_admit(rolled.1, budget, ask).1)
}

/// The counter after a run of (period, ask) steps from a fresh pool.
pub open spec fn counter_after(budget: u64, steps: Seq<(u64, u64)>) -> (u64, u64)
    decreases steps.len(),
{
    if steps.len() == 0 {
        (0, 0)
    } else {
        spec_step(counter_after(budget, steps.drop_last()), budget, steps.last().0, steps.last().1)
    }
}

/// Safety across periods: for every sequence of clock readings (forward,
/// back or equal) and asks, a pool never has more than its budget spent in
/// the period it counts in, and that period never moves back.
pub proof fn lemma_every_period_keeps_its_budget(budget: u64, steps: Seq<(u64, u64)>)
    ensures
        counter_after(budget, steps).1 <= budget,
        steps.len() > 0 ==> counter_after(budget, steps).0 >= counter_after(
            budget,
            steps.drop_last(),
        ).0,
    decreases steps.len(),
{
    if steps.len() > 0 {
        lemma_every_period_keeps_its_budget(budget, steps.drop_last());
    }
}

/// The executable kernels agree with the spec, step by step, over a whole
/// period: folding `pool_admit` gives `spent_after`.
pub fn admit_run(spent: u64, budget: u64, asks: &Vec<u64>) -> (r: u64)
    requires
        spent <= budget,
    ensures
        r == spent_after(spent, budget, asks@),
        r <= budget,
{
    let mut s = spent;
    let mut i: usize = 0;
    while i < asks.len()
        invariant
            0 <= i <= asks@.len(),
            spent <= budget,
            s == spent_after(spent, budget, asks@.subrange(0, i as int)),
            s <= budget,
        decreases asks@.len() - i,
    {
        proof {
            assert(asks@.subrange(0, i + 1).drop_last() =~= asks@.subrange(0, i as int));
        }
        let (_, next) = pool_admit(s, budget, asks[i]);
        s = next;
        i += 1;
    }
    proof {
        assert(asks@.subrange(0, asks@.len() as int) =~= asks@);
        lemma_run_never_passes_budget(spent, budget, asks@);
    }
    s
}

} // verus!
