//! Verus proofs of the agent-mud kernels.
//!
//! Verus checks this crate root (see `formal/verus/README.md`). Each proved
//! function mirrors a function in the Rust source. The guard test
//! `verus_mirror` in `mud-rules` checks that the mirrored text matches.

use vstd::prelude::*;

pub mod budget;
pub mod chatter;
pub mod classify;
pub mod connections;
pub mod hooks;
pub mod injury;
pub mod login_route;
pub mod schedule;
pub mod throttle;
pub mod typing;
pub mod wounds;

verus! {

fn main() {
}

} // verus!
