//! Each Verus proof in `formal/verus` mirrors Rust source text. This guard
//! fails when an account kernel or its proof drifts. It shares the marker
//! and normalization rules of the `mud-rules` guard.

#[path = "../../mud-rules/tests/mirror/mod.rs"]
mod mirror;

use mirror::{TestResult, every_proof_module_is_listed, proof_mirrors};

#[test]
fn the_login_route_proof_covers_the_shipped_kernel() -> TestResult {
    proof_mirrors("login_route")
}

#[test]
fn the_throttle_proof_covers_the_shipped_kernel() -> TestResult {
    proof_mirrors("throttle")
}

#[test]
fn every_proof_module_is_checked() -> TestResult {
    every_proof_module_is_listed()
}
