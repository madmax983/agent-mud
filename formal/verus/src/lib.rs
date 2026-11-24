//! Verus proofs of the agent-mud kernels.
//!
//! Verus checks this crate root (see `formal/verus/README.md`). Each proved
//! function mirrors a function in the Rust source. The guard test
//! `verus_mirror` in `mud-rules` checks that the mirrored text matches.

use vstd::prelude::*;

pub mod classify;
pub mod injury;
pub mod schedule;
pub mod wounds;

verus! {

fn main() {
}

} // verus!
