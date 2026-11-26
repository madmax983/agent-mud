//! Macro ticks: when one waits for a player, and how soon a lost one
//! comes again.
//!
//! Mirrors `crates/mud-supervisor/src/ticks.rs`. The text between the
//! mirror markers must match the Rust source; only the spec clauses differ.
//!
//! - `tick_deferred`: a tick waits exactly when a player other than the
//!   entity acted in its room less than `window_ms` ago (an act stamped
//!   ahead of the clock counts as now). With no such act it never waits
//!   (`lemma_no_act_never_defers`); a deferred tick always has an act
//!   within the window behind it (`lemma_deferred_only_by_a_recent_act`);
//!   and once a room is quiet for the window it stays undeferred as the
//!   clock runs on with no new act (`lemma_quiet_stays_quiet`), so an idle
//!   player never holds a tick back for longer than the window.
//! - `tick_retry_ms`: the first retry is `min(first, cap)`; each later
//!   one is `min(2 * prev, cap)`. Along a run of losses from 0 no delay
//!   passes the cap (`lemma_retries_stay_under_the_cap`), none is shorter
//!   than the one before (`lemma_retries_never_shrink`), and the `n`-th is
//!   at least `min(first * n, cap)` (`lemma_retries_grow`), so a broken
//!   mind is tried less and less often, never every step.
//!
//! No bound: comparisons, a saturating subtraction, and one doubling
//! under its guard `prev <= cap / 2`.

use vstd::prelude::*;

verus! {

/// The age of an act at `now_ms`: 0 for one stamped after it.
pub open spec fn spec_age(at: u64, now_ms: u64) -> int {
    if at < now_ms {
        now_ms - at
    } else {
        0
    }
}

pub open spec fn spec_deferred(last_act_ms: Option<u64>, now_ms: u64, window_ms: u64) -> bool {
    match last_act_ms {
        Some(at) => spec_age(at, now_ms) < window_ms,
        None => false,
    }
}

// mirror-begin tick_deferred
/// Whether a macro tick waits at `now_ms`.
///
/// It waits when a player other than the entity acted in its room at
/// `last_act_ms`, less than `window_ms` ago. `None`: no such player acted
/// (or none is present). An act stamped after `now_ms` counts as now.
#[must_use]
pub const fn tick_deferred(last_act_ms: Option<u64>, now_ms: u64, window_ms: u64) -> (r: bool)
    ensures
        r == spec_deferred(last_act_ms, now_ms, window_ms),
        last_act_ms is None ==> !r,
        window_ms == 0 ==> !r,
{
    match last_act_ms {
        Some(at) => now_ms.saturating_sub(at) < window_ms,
        None => false,
    }
}
// mirror-end tick_deferred

pub open spec fn spec_retry(prev_ms: u64, first_ms: u64, cap_ms: u64) -> u64 {
    if prev_ms == 0 {
        if first_ms < cap_ms {
            first_ms
        } else {
            cap_ms
        }
    } else if 2 * (prev_ms as int) < cap_ms as int {
        (2 * prev_ms) as u64
    } else {
        cap_ms
    }
}

// mirror-begin tick_retry_ms
/// The delay before a lost macro tick is tried again, in ms.
///
/// `prev_ms` is the delay of the previous loss in a row, 0 for the first.
/// The first is `first_ms`; each later one doubles the previous; none
/// passes `cap_ms` (the shortest normal interval of the entity).
#[must_use]
pub const fn tick_retry_ms(prev_ms: u64, first_ms: u64, cap_ms: u64) -> (r: u64)
    ensures
        r == spec_retry(prev_ms, first_ms, cap_ms),
        r <= cap_ms,
        prev_ms == 0 ==> r == if first_ms < cap_ms { first_ms } else { cap_ms },
        prev_ms != 0 && prev_ms <= cap_ms ==> r >= prev_ms,
{
    if prev_ms == 0 {
        if first_ms < cap_ms { first_ms } else { cap_ms }
    } else if prev_ms > cap_ms / 2 {
        cap_ms
    } else {
        prev_ms * 2
    }
}
// mirror-end tick_retry_ms

/// With no player act behind it, a tick never waits.
pub proof fn lemma_no_act_never_defers(now_ms: u64, window_ms: u64)
    ensures
        !spec_deferred(None, now_ms, window_ms),
{
}

/// A deferred tick has a player act less than the window old behind it.
pub proof fn lemma_deferred_only_by_a_recent_act(
    last_act_ms: Option<u64>,
    now_ms: u64,
    window_ms: u64,
)
    requires
        spec_deferred(last_act_ms, now_ms, window_ms),
    ensures
        exists|at: u64| last_act_ms == Some(at) && (at as int) + (window_ms as int) > now_ms as int,
{
    let at = last_act_ms->Some_0;
    assert(last_act_ms == Some(at) && (at as int) + (window_ms as int) > now_ms as int);
}

/// A room quiet for the window stays quiet as the clock runs on with no
/// new act: the tick waits at most the window after the last act.
pub proof fn lemma_quiet_stays_quiet(at: u64, now_ms: u64, later_ms: u64, window_ms: u64)
    requires
        !spec_deferred(Some(at), now_ms, window_ms),
        now_ms <= later_ms,
    ensures
        !spec_deferred(Some(at), later_ms, window_ms),
{
}

/// The act is at least the window old: the tick fires.
pub proof fn lemma_an_old_act_never_defers(at: u64, now_ms: u64, window_ms: u64)
    requires
        (at as int) + (window_ms as int) <= now_ms as int,
    ensures
        !spec_deferred(Some(at), now_ms, window_ms),
{
}

/// The delay of the `n`-th loss in a row (1 is the first), 0 for none.
pub open spec fn retry_run(n: nat, first_ms: u64, cap_ms: u64) -> u64
    decreases n,
{
    if n == 0 {
        0
    } else {
        spec_retry(retry_run((n - 1) as nat, first_ms, cap_ms), first_ms, cap_ms)
    }
}

/// No delay of a run of losses passes the cap.
pub proof fn lemma_retries_stay_under_the_cap(n: nat, first_ms: u64, cap_ms: u64)
    ensures
        retry_run(n, first_ms, cap_ms) <= cap_ms,
    decreases n,
{
    if n > 0 {
        lemma_retries_stay_under_the_cap((n - 1) as nat, first_ms, cap_ms);
    }
}

/// Each delay of a run of losses is at least the one before.
pub proof fn lemma_retries_never_shrink(n: nat, first_ms: u64, cap_ms: u64)
    ensures
        retry_run(n, first_ms, cap_ms) <= retry_run(n + 1, first_ms, cap_ms),
    decreases n,
{
    lemma_retries_stay_under_the_cap(n, first_ms, cap_ms);
    if n > 0 {
        lemma_retries_never_shrink((n - 1) as nat, first_ms, cap_ms);
    }
}

/// The `n`-th delay of a run of losses is at least `min(first * n, cap)`:
/// the tries thin out, so a broken mind is never tried every step.
pub proof fn lemma_retries_grow(n: nat, first_ms: u64, cap_ms: u64)
    requires
        n >= 1,
    ensures
        retry_run(n, first_ms, cap_ms) as int >= if (first_ms as int) * (n as int) < cap_ms as int {
            (first_ms as int) * (n as int)
        } else {
            cap_ms as int
        },
    decreases n,
{
    let f = first_ms as int;
    let c = cap_ms as int;
    if n == 1 {
        assert(retry_run(0, first_ms, cap_ms) == 0);
        assert(retry_run(1, first_ms, cap_ms) == spec_retry(0, first_ms, cap_ms));
        assert(f * 1 == f);
    } else {
        let m = (n - 1) as nat;
        lemma_retries_grow(m, first_ms, cap_ms);
        lemma_retries_stay_under_the_cap(m, first_ms, cap_ms);
        let prev = retry_run(m, first_ms, cap_ms);
        let p = prev as int;
        assert(retry_run(n, first_ms, cap_ms) == spec_retry(prev, first_ms, cap_ms));
        assert(f * (n as int) == f * (m as int) + f) by (nonlinear_arith)
            requires
                n == m + 1,
        ;
        assert(f * (m as int) >= f) by (nonlinear_arith)
            requires
                m >= 1,
                f >= 0,
        ;
        if p == 0 {
            // From 0 >= min(f m, c): the cap is 0, or so is the first delay.
            assert(f == 0 || c == 0);
        }
    }
}

} // verus!
