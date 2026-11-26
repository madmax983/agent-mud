//! The wake hold: no wake runs while its entity cannot act, and nothing
//! held is lost.
//!
//! Mirrors `crates/mud-supervisor/src/admit.rs`. The text between the
//! mirror markers must match the Rust source; only the spec clauses differ.
//!
//! - `wake_admit`: `Hold` exactly when the entity cannot act (unconscious
//!   or dead), `Take` exactly when it can act, is free and the wake is due,
//!   `Wait` otherwise. Nothing is admitted while the entity cannot act.
//! - The hold model: an entity's waiting wake as the list of what it
//!   carries (events, bookings, ticks), over every history of queued items
//!   and router polls. No wake is ever delivered at a poll where the entity
//!   cannot act (`lemma_no_wake_while_held`, the budget rule: an
//!   unconscious agent spends nothing); every queued item is either still
//!   waiting or was delivered, in order, exactly once
//!   (`lemma_nothing_held_is_lost`); and the first poll after the entity
//!   can act again, free and due, delivers everything held in one wake
//!   (`lemma_one_wake_carries_the_hold`).
//!
//! - `wake_expired` (ADR 0014): a running wake is past its bound exactly
//!   when the time it ran outside the gate's holds reaches it. Holds only
//!   extend the bound (`lemma_holds_only_extend`), and a wake past it
//!   stays past it (`lemma_expired_stays_expired`).
//!
//! No bound: comparisons and a saturating subtraction.

use vstd::prelude::*;

verus! {

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

pub open spec fn spec_admit(can_act: bool, free: bool, due_ms: u64, now_ms: u64) -> Admit {
    if !can_act {
        Admit::Hold
    } else if free && due_ms <= now_ms {
        Admit::Take
    } else {
        Admit::Wait
    }
}

// mirror-begin wake_admit
/// Whether the router delivers an entity's waiting wake, due at
/// `due_ms`, at `now_ms`. `can_act`: the entity is neither unconscious nor
/// dead. `free`: it has no wake in flight and no plan to resume first.
#[must_use]
pub const fn wake_admit(can_act: bool, free: bool, due_ms: u64, now_ms: u64) -> (r: Admit)
    ensures
        r == spec_admit(can_act, free, due_ms, now_ms),
        (r is Hold) <==> !can_act,
        (r is Take) <==> (can_act && free && due_ms <= now_ms),
        (r is Wait) <==> (can_act && !(free && due_ms <= now_ms)),
        (r is Take) ==> can_act,
{
    if !can_act {
        Admit::Hold
    } else if free && due_ms <= now_ms {
        Admit::Take
    } else {
        Admit::Wait
    }
}
// mirror-end wake_admit

/// The time of a wake that counts toward its bound: what it ran outside
/// the gate's holds.
pub open spec fn spec_active(elapsed_ms: u64, held_ms: u64) -> int {
    if held_ms < elapsed_ms {
        elapsed_ms - held_ms
    } else {
        0
    }
}

// mirror-begin wake_expired
/// Whether a wake is past its bound of `timeout_ms`.
///
/// It has run `elapsed_ms`, `held_ms` of them with a line of its entity
/// held at the gate (a hidden PC typing). Only the time outside the holds
/// counts: a slow typist is not a mind that hangs.
#[must_use]
pub const fn wake_expired(elapsed_ms: u64, held_ms: u64, timeout_ms: u64) -> (r: bool)
    ensures
        r <==> spec_active(elapsed_ms, held_ms) >= timeout_ms,
        // A wake that spent all its time typing never runs out, unless its
        // bound is 0.
        elapsed_ms <= held_ms ==> (r <==> timeout_ms == 0),
        // Without a hold the bound is the plain one.
        held_ms == 0 ==> (r <==> elapsed_ms >= timeout_ms),
        // The holds only ever extend the bound, never shorten it.
        r ==> elapsed_ms >= timeout_ms,
{
    elapsed_ms.saturating_sub(held_ms) >= timeout_ms
}
// mirror-end wake_expired

/// One step of an entity's history: an item joins its waiting wake (an
/// event, a booking that came due, a macro tick), or the router polls.
pub enum HoldStep {
    Queue(u64),
    Poll(bool, bool, u64, u64),
}

/// The waiting wake, what was delivered (flattened, in order), how many
/// wakes ran, and how many of them ran while the entity could not act.
pub struct HoldState {
    pub pending: Seq<u64>,
    pub delivered: Seq<u64>,
    pub wakes: nat,
    pub held_wakes: nat,
}

pub open spec fn start() -> HoldState {
    HoldState { pending: Seq::empty(), delivered: Seq::empty(), wakes: 0, held_wakes: 0 }
}

/// The router: a queued item merges into the waiting wake; a poll that
/// `wake_admit` takes delivers the whole waiting wake, else keeps it.
pub open spec fn hold_step(s: HoldState, step: HoldStep) -> HoldState {
    match step {
        HoldStep::Queue(x) => HoldState { pending: s.pending.push(x), ..s },
        HoldStep::Poll(can_act, free, due, now) => {
            if s.pending.len() > 0 && spec_admit(can_act, free, due, now) is Take {
                HoldState {
                    pending: Seq::empty(),
                    delivered: s.delivered + s.pending,
                    wakes: s.wakes + 1,
                    held_wakes: if can_act {
                        s.held_wakes
                    } else {
                        s.held_wakes + 1
                    },
                }
            } else {
                s
            }
        },
    }
}

pub open spec fn run(steps: Seq<HoldStep>) -> HoldState
    decreases steps.len(),
{
    if steps.len() == 0 {
        start()
    } else {
        hold_step(run(steps.drop_last()), steps.last())
    }
}

/// Every item queued in a history, in order.
pub open spec fn queued(steps: Seq<HoldStep>) -> Seq<u64>
    decreases steps.len(),
{
    if steps.len() == 0 {
        Seq::empty()
    } else {
        match steps.last() {
            HoldStep::Queue(x) => queued(steps.drop_last()).push(x),
            HoldStep::Poll(..) => queued(steps.drop_last()),
        }
    }
}

/// No wake is delivered while the entity cannot act, over every history.
pub proof fn lemma_no_wake_while_held(steps: Seq<HoldStep>)
    ensures
        run(steps).held_wakes == 0,
    decreases steps.len(),
{
    if steps.len() > 0 {
        lemma_no_wake_while_held(steps.drop_last());
    }
}

/// Nothing held is lost or repeated: what was delivered, then what still
/// waits, is exactly what was queued, in order.
pub proof fn lemma_nothing_held_is_lost(steps: Seq<HoldStep>)
    ensures
        run(steps).delivered + run(steps).pending =~= queued(steps),
    decreases steps.len(),
{
    if steps.len() > 0 {
        let prev = steps.drop_last();
        lemma_nothing_held_is_lost(prev);
        let s = run(prev);
        match steps.last() {
            HoldStep::Queue(x) => {
                assert(s.delivered + s.pending.push(x) =~= (s.delivered + s.pending).push(x));
            },
            HoldStep::Poll(can_act, free, due, now) => {
                if s.pending.len() > 0 && spec_admit(can_act, free, due, now) is Take {
                    assert((s.delivered + s.pending) + Seq::<u64>::empty() =~= s.delivered
                        + s.pending);
                }
            },
        }
    }
}

/// Once the entity can act, is free and its wake is due, one poll
/// delivers everything held, as one wake, and leaves nothing waiting.
pub proof fn lemma_one_wake_carries_the_hold(s: HoldState, free: bool, due: u64, now: u64)
    requires
        s.pending.len() > 0,
        free,
        due <= now,
    ensures
        hold_step(s, HoldStep::Poll(true, free, due, now)).pending.len() == 0,
        hold_step(s, HoldStep::Poll(true, free, due, now)).delivered =~= s.delivered + s.pending,
        hold_step(s, HoldStep::Poll(true, free, due, now)).wakes == s.wakes + 1,
{
}

/// While the entity cannot act, a poll changes nothing.
pub proof fn lemma_a_held_poll_keeps_everything(s: HoldState, free: bool, due: u64, now: u64)
    ensures
        hold_step(s, HoldStep::Poll(false, free, due, now)) == s,
{
}

/// Once a wake is past its bound it stays past it as time goes on, while
/// no further hold is counted.
pub proof fn lemma_expired_stays_expired(e1: u64, e2: u64, held_ms: u64, timeout_ms: u64)
    requires
        e1 <= e2,
        spec_active(e1, held_ms) >= timeout_ms,
    ensures
        spec_active(e2, held_ms) >= timeout_ms,
{
}

/// More time held never makes a wake run out sooner: a wake past its bound
/// with more held time is past it with less.
pub proof fn lemma_holds_only_extend(elapsed_ms: u64, h1: u64, h2: u64, timeout_ms: u64)
    requires
        h1 <= h2,
        spec_active(elapsed_ms, h2) >= timeout_ms,
    ensures
        spec_active(elapsed_ms, h1) >= timeout_ms,
{
}

} // verus!
