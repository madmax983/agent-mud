//! Connection admission.
//!
//! Mirrors `crates/mud-server/src/limits.rs`. The text between the mirror
//! markers must match the Rust source; only the spec clauses differ.
//!
//! One step updates the total count and the count of one address. The
//! proof shows an open is admitted iff both are under their caps, a close
//! never underflows, the step cannot overflow, and the invariant
//! `here <= total <= max_total && here <= max_here` survives every step,
//! so no sequence of opens and closes from a fresh limiter breaks it.

use vstd::prelude::*;

verus! {

/// A connection event, as the counter sees it.
#[derive(Clone, Copy)]
// mirror-begin conn_op
pub enum ConnOp {
    /// A client connects.
    Open,
    /// A client goes away.
    Close,
}
// mirror-end conn_op

/// The invariant one address's count keeps with the total.
pub open spec fn inv(total: u32, here: u32, max_total: u32, max_here: u32) -> bool {
    here <= total && total <= max_total && here <= max_here
}

// mirror-begin conn_step
/// One step over the total count and one address's count: the new total,
/// the new count for the address, and whether the step took effect.
#[must_use]
pub const fn conn_step(
    total: u32,
    here: u32,
    op: ConnOp,
    max_total: u32,
    max_here: u32,
) -> (r: (u32, u32, bool))
    ensures
        // An open is admitted iff both counts are under their caps, and
        // then adds one to each; refused, it changes nothing.
        op is Open ==> (r.2 <==> (total < max_total && here < max_here)),
        op is Open && r.2 ==> r.0 == total + 1 && r.1 == here + 1,
        // A close takes effect iff something is open here, and then takes
        // one from each, never below zero.
        op is Close ==> (r.2 <==> (here > 0 && total > 0)),
        op is Close && r.2 ==> r.0 == total - 1 && r.1 == here - 1,
        !r.2 ==> r.0 == total && r.1 == here,
        // The invariant survives the step.
        inv(total, here, max_total, max_here) ==> inv(r.0, r.1, max_total, max_here),
{
    match op {
        ConnOp::Open => {
            if total < max_total && here < max_here {
                (total + 1, here + 1, true)
            } else {
                (total, here, false)
            }
        }
        ConnOp::Close => {
            if here > 0 && total > 0 {
                (total - 1, here - 1, true)
            } else {
                (total, here, false)
            }
        }
    }
}
// mirror-end conn_step

/// The counts after `ops` from a fresh limiter, for one address.
pub open spec fn run(ops: Seq<ConnOp>, max_total: u32, max_here: u32) -> (u32, u32)
    decreases ops.len(),
{
    if ops.len() == 0 {
        (0, 0)
    } else {
        let (total, here) = run(ops.drop_last(), max_total, max_here);
        let op = ops.last();
        let (t, h) = match op {
            ConnOp::Open => if total < max_total && here < max_here {
                ((total + 1) as u32, (here + 1) as u32)
            } else {
                (total, here)
            },
            ConnOp::Close => if here > 0 && total > 0 {
                ((total - 1) as u32, (here - 1) as u32)
            } else {
                (total, here)
            },
        };
        (t, h)
    }
}

/// Safety: from a fresh limiter, every sequence of opens and closes keeps
/// the address within its share and the total within its cap.
pub proof fn lemma_run_keeps_caps(ops: Seq<ConnOp>, max_total: u32, max_here: u32)
    ensures
        inv(run(ops, max_total, max_here).0, run(ops, max_total, max_here).1, max_total, max_here),
    decreases ops.len(),
{
    if ops.len() > 0 {
        lemma_run_keeps_caps(ops.drop_last(), max_total, max_here);
    }
}

} // verus!
