//! Each Verus proof in `formal/verus` mirrors Rust source text. This guard
//! fails when either side drifts, so a proof never covers stale code. The
//! marker and normalization rules are in `tests/mirror/mod.rs`.

mod mirror;

use mirror::{
    PAIRS, TestResult, compare, every_proof_module_is_listed, normalize_verus, proof_mirrors,
    proof_path, root, squeeze,
};

#[test]
fn the_classify_proof_covers_the_shipped_kernel() -> TestResult {
    proof_mirrors("classify")
}

#[test]
fn the_injury_proof_covers_the_shipped_kernel() -> TestResult {
    proof_mirrors("injury")
}

#[test]
fn the_wound_proof_covers_the_shipped_kernel() -> TestResult {
    proof_mirrors("wounds")
}

#[test]
fn the_schedule_proof_covers_the_shipped_kernel() -> TestResult {
    proof_mirrors("schedule")
}

#[test]
fn the_hooks_proof_covers_the_shipped_kernel() -> TestResult {
    proof_mirrors("hooks")
}

#[test]
fn the_rewards_proof_covers_the_shipped_kernel() -> TestResult {
    proof_mirrors("rewards")
}

#[test]
fn every_proof_module_is_checked() -> TestResult {
    every_proof_module_is_listed()
}

#[test]
fn drift_on_either_side_fails() -> TestResult {
    let rust = std::fs::read_to_string(root().join(PAIRS[1].1))?;
    let verus = std::fs::read_to_string(root().join(proof_path(PAIRS[1].0)))?;
    compare(&rust, &verus)?;
    let needle = "DamageType::Impaling => pen * 2,";
    assert!(rust.contains(needle) && verus.contains(needle));
    let drifted = needle.replace("pen * 2", "pen * 3");
    assert!(compare(&rust.replace(needle, &drifted), &verus).is_err());
    assert!(compare(&rust, &verus.replace(needle, &drifted)).is_err());
    // A changed comment, a renamed region or a missing region also fails.
    assert!(compare(&rust.replace("Never negative.", "Never zero."), &verus).is_err());
    assert!(
        compare(
            &rust.replace("mirror-end injury", "mirror-end injury2"),
            &verus
        )
        .is_err()
    );
    assert!(compare(&rust, &verus.replace("// mirror-begin injury", "// begin")).is_err());
    // A changed signature fails too, outside the spec clauses.
    assert!(compare(&rust, &verus.replace("-> (r: i32)", "-> (r: i64)")).is_err());
    Ok(())
}

#[test]
fn normalization_drops_only_what_verus_needs() {
    let verus = "pub const fn f(x: u8) -> (r: u8)\n    requires\n        x < 9,\n    ensures\n        r == x,\n{\n    x\n}\n";
    assert_eq!(
        normalize_verus(verus),
        squeeze("pub const fn f(x: u8) -> u8 {\n    x\n}\n")
    );
    let tuple = "fn g() -> (r: (u8, bool))\n    ensures\n        r.1,\n{\n    (0, true)\n}\n";
    assert_eq!(
        normalize_verus(tuple),
        squeeze("fn g() -> (u8, bool) {\n    (0, true)\n}\n")
    );
    // A wrong tuple type still differs.
    assert_ne!(
        normalize_verus(tuple),
        squeeze("fn g() -> (u8, u8) {\n    (0, true)\n}\n")
    );
}
