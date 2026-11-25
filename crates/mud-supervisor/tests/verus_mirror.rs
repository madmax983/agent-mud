//! Each Verus proof in `formal/verus` mirrors Rust source text. This guard
//! fails when a supervisor kernel or its proof drifts. It shares the marker
//! and normalization rules of the `mud-rules` guard.

#[path = "../../mud-rules/tests/mirror/mod.rs"]
mod mirror;

use mirror::{TestResult, every_proof_module_is_listed, proof_mirrors};

#[test]
fn the_chatter_proof_covers_the_shipped_kernel() -> TestResult {
    proof_mirrors("chatter")
}

#[test]
fn the_budget_proof_covers_the_shipped_kernel() -> TestResult {
    proof_mirrors("budget")
}

#[test]
fn the_typing_proof_covers_the_shipped_kernel() -> TestResult {
    proof_mirrors("typing")
}

#[test]
fn the_persist_proof_covers_the_shipped_kernels() -> TestResult {
    proof_mirrors("persist")
}

#[test]
fn the_replay_proof_covers_the_shipped_kernels() -> TestResult {
    proof_mirrors("replay")
}

#[test]
fn every_proof_module_is_checked() -> TestResult {
    every_proof_module_is_listed()
}
