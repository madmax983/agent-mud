//! Login throttles: exponential backoff per key.
//!
//! Mirrors `crates/mud-accounts/src/throttle.rs`. The text between the
//! mirror markers must match the Rust source; only the spec clauses differ.
//!
//! Proved with no `requires` on the functions:
//! - S1: no overflow.
//! - S2: a success clears the key, which then allows every time.
//! - S3: a failure never delays past the cap, even from a stored delay that
//!   is larger (a lowered cap applies at once).
//! - S4: a failure records `now` and locks until at least `now`; with room
//!   below `u64::MAX`, exactly until `now + delay <= now + max`.
//! - S5: failures within the free count never delay.
//! - S6: a failure past the free count delays at least `min(base, max)`.
//! - S7: the failure count restarts at 1 after a quiet spell of
//!   `forget_ms`, and otherwise adds one, saturating.
//! - S8: a key allows exactly when `now >= locked_until_ms`.
//!
//! `lemma_rate_bound`: along a run of failures that each passed the check,
//! with no quiet spell, the count is exact and every failure past the free
//! count comes at least `min(base, max)` after the one before it. So a
//! window of `L` ms admits at most `free + 1 + L / min(base, max)` checked
//! failures for one key.

use vstd::prelude::*;

verus! {

#[derive(Clone, Copy)]
// mirror-begin throttle_op
/// What happened at a key.
pub enum ThrottleOp {
    /// A wrong password, or a unit of work.
    Fail,
    /// A right password.
    Succeed,
}
// mirror-end throttle_op

#[derive(Clone, Copy)]
// mirror-begin throttle_policy
/// The backoff of one kind of key.
pub struct ThrottlePolicy {
    /// Failures that never delay.
    pub free: u32,
    /// The first delay, in ms.
    pub base_ms: u64,
    /// The longest delay, in ms.
    pub max_ms: u64,
    /// A quiet spell this long forgets every failure, in ms.
    pub forget_ms: u64,
}
// mirror-end throttle_policy

#[derive(Clone, Copy)]
// mirror-begin throttle_state
/// The backoff state of one key.
pub struct Throttle {
    /// Failures since the last quiet spell or success.
    pub failures: u32,
    /// The delay the last failure set, in ms.
    pub delay_ms: u64,
    /// No check passes before this time.
    pub locked_until_ms: u64,
    /// When the last failure happened.
    pub last_failure_ms: u64,
}
// mirror-end throttle_state

/// A fresh key.
pub open spec fn zero() -> Throttle {
    Throttle { failures: 0, delay_ms: 0, locked_until_ms: 0, last_failure_ms: 0 }
}

/// `min(a, b)`.
pub open spec fn min_u64(a: u64, b: u64) -> u64 {
    if a < b { a } else { b }
}

/// Whether a failure at `now` comes after a quiet spell.
pub open spec fn is_stale(t: Throttle, now: u64, p: ThrottlePolicy) -> bool {
    now >= t.last_failure_ms && now - t.last_failure_ms >= p.forget_ms
}

/// `x + 1`, saturating at `u32::MAX`.
pub open spec fn sat_inc(x: u32) -> u32 {
    if x == u32::MAX { u32::MAX } else { (x + 1) as u32 }
}

/// The delay a failure sets, over plain values.
pub open spec fn spec_delay(t: Throttle, now: u64, p: ThrottlePolicy, failures: u32) -> u64 {
    let first = min_u64(p.base_ms, p.max_ms);
    if failures <= p.free {
        0
    } else if is_stale(t, now, p) || t.delay_ms == 0 {
        first
    } else if t.delay_ms > p.max_ms / 2 {
        p.max_ms
    } else if t.delay_ms * 2 < first {
        first
    } else {
        (t.delay_ms * 2) as u64
    }
}

/// The step over plain values.
pub open spec fn spec_throttle_step(t: Throttle, op: ThrottleOp, now: u64, p: ThrottlePolicy) -> Throttle {
    match op {
        ThrottleOp::Succeed => Throttle {
            failures: 0,
            delay_ms: 0,
            locked_until_ms: 0,
            last_failure_ms: t.last_failure_ms,
        },
        ThrottleOp::Fail => {
            let failures: u32 = if is_stale(t, now, p) { 1 } else { sat_inc(t.failures) };
            let delay = spec_delay(t, now, p, failures);
            let locked: u64 = if now as int + delay as int > u64::MAX as int {
                u64::MAX
            } else {
                (now + delay) as u64
            };
            Throttle { failures, delay_ms: delay, locked_until_ms: locked, last_failure_ms: now }
        },
    }
}

/// Whether a key allows a check at `now`, over plain values.
pub open spec fn spec_allows(t: Throttle, now: u64) -> bool {
    now >= t.locked_until_ms
}

// mirror-begin throttle_allows
/// Whether a key allows a check at `now_ms`.
#[must_use]
pub const fn throttle_allows(t: &Throttle, now_ms: u64) -> (r: bool)
    ensures
        // S8.
        r == spec_allows(*t, now_ms),
        r <==> now_ms >= t.locked_until_ms,
{
    now_ms >= t.locked_until_ms
}
// mirror-end throttle_allows

// mirror-begin throttle_step
/// One step of a key's backoff.
#[must_use]
pub const fn throttle_step(
    t: Throttle,
    op: ThrottleOp,
    now_ms: u64,
    p: &ThrottlePolicy,
) -> (r: Throttle)
    ensures
        r == spec_throttle_step(t, op, now_ms, *p),
        // S2: a success clears the key.
        op is Succeed ==> r.failures == 0 && r.delay_ms == 0 && r.locked_until_ms == 0,
        // S3: never past the cap.
        op is Fail ==> r.delay_ms <= p.max_ms,
        // S4: the failure is recorded, and the lock starts now.
        op is Fail ==> r.last_failure_ms == now_ms && r.locked_until_ms >= now_ms,
        (op is Fail && now_ms as int + p.max_ms as int <= u64::MAX as int) ==> r.locked_until_ms
            == now_ms + r.delay_ms && r.locked_until_ms <= now_ms + p.max_ms,
        // S5: free failures never delay.
        (op is Fail && r.failures <= p.free) ==> r.delay_ms == 0,
        // S6: past free, at least the first delay.
        (op is Fail && r.failures > p.free) ==> r.delay_ms >= min_u64(p.base_ms, p.max_ms),
        // S7: the count.
        op is Fail ==> r.failures == (if is_stale(t, now_ms, *p) { 1 } else { sat_inc(t.failures) }),
{
    match op {
        ThrottleOp::Succeed => Throttle {
            failures: 0,
            delay_ms: 0,
            locked_until_ms: 0,
            last_failure_ms: t.last_failure_ms,
        },
        ThrottleOp::Fail => {
            let stale = now_ms >= t.last_failure_ms && now_ms - t.last_failure_ms >= p.forget_ms;
            let failures = if stale { 1 } else { t.failures.saturating_add(1) };
            let first = if p.base_ms < p.max_ms { p.base_ms } else { p.max_ms };
            let delay_ms = if failures <= p.free {
                0
            } else if stale || t.delay_ms == 0 {
                first
            } else if t.delay_ms > p.max_ms / 2 {
                p.max_ms
            } else if t.delay_ms * 2 < first {
                first
            } else {
                t.delay_ms * 2
            };
            let locked_until_ms = now_ms.saturating_add(delay_ms);
            Throttle {
                failures,
                delay_ms,
                locked_until_ms,
                last_failure_ms: now_ms,
            }
        }
    }
}
// mirror-end throttle_step

/// A lock never outlasts the cap: right after a failure, the key allows
/// again at `now + max`.
pub proof fn lemma_lock_ends_by_cap(t: Throttle, now: u64, p: ThrottlePolicy)
    requires
        now as int + p.max_ms as int <= u64::MAX as int,
    ensures
        spec_allows(spec_throttle_step(t, ThrottleOp::Fail, now, p), (now + p.max_ms) as u64),
{
    let r = spec_throttle_step(t, ThrottleOp::Fail, now, p);
    lemma_delay_capped(t, now, p);
    assert(r.delay_ms <= p.max_ms);
}

/// The delay of a failure is at most the cap.
pub proof fn lemma_delay_capped(t: Throttle, now: u64, p: ThrottlePolicy)
    ensures
        spec_throttle_step(t, ThrottleOp::Fail, now, p).delay_ms <= p.max_ms,
        spec_throttle_step(t, ThrottleOp::Fail, now, p).failures > p.free ==> spec_throttle_step(
            t,
            ThrottleOp::Fail,
            now,
            p,
        ).delay_ms >= min_u64(p.base_ms, p.max_ms),
{
}

/// The state after failures `1..=j` of `ts` from a fresh key (`ts[0]` is
/// unused).
pub open spec fn run(ts: Seq<u64>, j: nat, p: ThrottlePolicy) -> Throttle
    decreases j,
{
    if j == 0 {
        zero()
    } else {
        spec_throttle_step(run(ts, (j - 1) as nat, p), ThrottleOp::Fail, ts[j as int], p)
    }
}

/// Times that never go back stay at or below the last one.
pub proof fn lemma_times_rise(ts: Seq<u64>, i: nat, n: nat)
    requires
        1 <= i <= n < ts.len(),
        forall|j: int| 2 <= j <= n ==> ts[j - 1] <= #[trigger] ts[j],
    ensures
        ts[i as int] <= ts[n as int],
    decreases n - i,
{
    if i < n {
        lemma_times_rise(ts, i, (n - 1) as nat);
    }
}

/// Each step of a run records its own time.
pub proof fn lemma_run_records(ts: Seq<u64>, j: nat, p: ThrottlePolicy)
    requires
        1 <= j < ts.len(),
    ensures
        run(ts, j, p).last_failure_ms == ts[j as int],
{
}

/// The count along a run of failures with no quiet spell is exact.
pub proof fn lemma_run_counts(ts: Seq<u64>, j: nat, n: nat, p: ThrottlePolicy)
    requires
        1 <= j <= n < ts.len(),
        n < u32::MAX,
        forall|k: int| 2 <= k <= n ==> ts[k - 1] <= #[trigger] ts[k],
        forall|k: int| 2 <= k <= n ==> #[trigger] ts[k] - ts[k - 1] < p.forget_ms,
    ensures
        run(ts, j, p).failures == j,
    decreases j,
{
    if j > 1 {
        lemma_run_counts(ts, (j - 1) as nat, n, p);
        lemma_run_records(ts, (j - 1) as nat, p);
        let prev = run(ts, (j - 1) as nat, p);
        assert(ts[j as int] - ts[j - 1] < p.forget_ms);
        assert(!is_stale(prev, ts[j as int], p));
        assert(run(ts, j, p) == spec_throttle_step(prev, ThrottleOp::Fail, ts[j as int], p));
        assert(sat_inc(prev.failures) == j);
    } else {
        reveal_with_fuel(run, 2);
        assert(run(ts, 1, p) == spec_throttle_step(zero(), ThrottleOp::Fail, ts[1], p));
        assert(sat_inc(zero().failures) == 1);
    }
}

/// The rate bound. Along `n` failures of one key, each admitted by the
/// check, in rising times with no quiet spell of `forget_ms`, and with room
/// below `u64::MAX` after the last: the count after failure `j` is `j`, and
/// every failure `j` whose predecessor was already past the free count
/// comes at least `min(base, max)` after it.
pub proof fn lemma_rate_bound(ts: Seq<u64>, n: nat, p: ThrottlePolicy)
    requires
        n < ts.len(),
        n < u32::MAX,
        forall|j: int| 2 <= j <= n ==> ts[j - 1] <= #[trigger] ts[j],
        forall|j: int| 2 <= j <= n ==> #[trigger] ts[j] - ts[j - 1] < p.forget_ms,
        forall|j: int| 1 <= j <= n ==> spec_allows(#[trigger] run(ts, (j - 1) as nat, p), ts[j]),
        n >= 1 ==> ts[n as int] as int + p.max_ms as int <= u64::MAX as int,
    ensures
        forall|j: nat| 1 <= j <= n ==> #[trigger] run(ts, j, p).failures == j,
        forall|j: int|
            2 <= j <= n && j - 1 > p.free ==> #[trigger] ts[j] - ts[j - 1] >= min_u64(
                p.base_ms,
                p.max_ms,
            ),
{
    assert forall|j: nat| 1 <= j <= n implies #[trigger] run(ts, j, p).failures == j by {
        lemma_run_counts(ts, j, n, p);
    }
    assert forall|j: int| 2 <= j <= n && j - 1 > p.free implies #[trigger] ts[j] - ts[j - 1]
        >= min_u64(p.base_ms, p.max_ms) by {
        let i: nat = (j - 1) as nat;
        lemma_run_counts(ts, i, n, p);
        lemma_times_rise(ts, i, n);
        let prev = run(ts, i, p);
        let before = run(ts, (i - 1) as nat, p);
        lemma_delay_capped(before, ts[j - 1], p);
        assert(prev.failures > p.free);
        assert(prev.delay_ms >= min_u64(p.base_ms, p.max_ms));
        assert(ts[j - 1] as int + p.max_ms as int <= u64::MAX as int);
        assert(prev.locked_until_ms == ts[j - 1] + prev.delay_ms);
        assert(spec_allows(run(ts, (j - 1) as nat, p), ts[j]));
    }
}

} // verus!
