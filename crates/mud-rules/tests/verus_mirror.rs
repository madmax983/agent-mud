//! Each Verus proof in `formal/verus` mirrors Rust source text. This guard
//! fails when either side drifts, so a proof never covers stale code.
//!
//! A file marks each mirrored region with `// mirror-begin <name>` and
//! `// mirror-end <name>`, each on its own line. The regions of a Rust file
//! and of its Verus module must have the same names and, after
//! normalization, the same text.
//!
//! Normalization:
//! - All white space is removed, on both sides. Comments and doc comments
//!   still count.
//! - On the Verus side only, two things Verus needs are dropped:
//!   1. The spec clauses of a function: every line from one that reads just
//!      `requires` or `ensures` up to, not including, the next line that
//!      reads just `{`.
//!   2. The name of the return value: `-> (r: T)` reads as `-> T`.
//!
//! Nothing else may differ.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};

type TestResult = Result<(), Box<dyn Error>>;

const BEGIN: &str = "// mirror-begin ";
const END: &str = "// mirror-end ";

/// The mirrored regions of `text`, by name, with their raw lines.
fn regions(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    let mut open: Option<(String, String)> = None;
    for line in text.lines() {
        let t = line.trim();
        if let Some(name) = t.strip_prefix(BEGIN) {
            if let Some((outer, _)) = &open {
                return Err(format!("mirror-begin {name} inside {outer}"));
            }
            open = Some((name.trim().to_owned(), String::new()));
        } else if let Some(name) = t.strip_prefix(END) {
            let Some((outer, body)) = open.take() else {
                return Err(format!("mirror-end {name} without a begin"));
            };
            if outer != name.trim() {
                return Err(format!("mirror-end {name} closes {outer}"));
            }
            if out.insert(outer.clone(), body).is_some() {
                return Err(format!("two regions named {outer}"));
            }
        } else if let Some((_, body)) = &mut open {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some((name, _)) = open {
        return Err(format!("mirror-begin {name} has no end"));
    }
    if out.is_empty() {
        return Err("no mirror markers".to_owned());
    }
    Ok(out)
}

/// Drops the Verus spec clauses (`requires` / `ensures` up to the `{`).
fn drop_spec_clauses(body: &str) -> String {
    let mut out = String::new();
    let mut in_spec = false;
    for line in body.lines() {
        let t = line.trim();
        if t == "requires" || t == "ensures" {
            in_spec = true;
        } else if t == "{" {
            in_spec = false;
        }
        if !in_spec {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn squeeze(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Rewrites a named return `->(r:T)` (white space removed) as `->T`.
fn unname_return(squeezed: &str) -> String {
    const NAMED: &str = "->(r:";
    let mut out = String::new();
    let mut rest = squeezed;
    while let Some(at) = rest.find(NAMED) {
        out.push_str(&rest[..at]);
        out.push_str("->");
        let after = &rest[at + NAMED.len()..];
        if let Some(close) = after.find(')') {
            out.push_str(&after[..close]);
            rest = &after[close + 1..];
        } else {
            out.push_str(after);
            rest = "";
        }
    }
    out.push_str(rest);
    out
}

fn normalize_rust(body: &str) -> String {
    squeeze(body)
}

fn normalize_verus(body: &str) -> String {
    unname_return(&squeeze(&drop_spec_clauses(body)))
}

/// Fails unless the Rust text and the Verus text mirror the same regions.
fn compare(rust: &str, verus: &str) -> Result<(), String> {
    let r = regions(rust).map_err(|e| format!("rust: {e}"))?;
    let v = regions(verus).map_err(|e| format!("verus: {e}"))?;
    let (rn, vn): (Vec<_>, Vec<_>) = (r.keys().collect(), v.keys().collect());
    if rn != vn {
        return Err(format!("regions differ: rust {rn:?}, verus {vn:?}"));
    }
    for (name, body) in &r {
        let (want, got) = (normalize_rust(body), normalize_verus(&v[name]));
        if want != got {
            return Err(format!(
                "region {name} drifted\n rust:  {want}\n verus: {got}"
            ));
        }
    }
    Ok(())
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn mirrors(source: &str, proof: &str) -> TestResult {
    let rust = std::fs::read_to_string(root().join(source))?;
    let verus = std::fs::read_to_string(root().join(proof))?;
    compare(&rust, &verus).map_err(|e| format!("update {proof} to match {source}: {e}"))?;
    Ok(())
}

/// Each proof module and the Rust file it mirrors.
const PAIRS: [(&str, &str); 4] = [
    (
        "crates/mud-rules/src/roll.rs",
        "formal/verus/src/classify.rs",
    ),
    (
        "crates/mud-rules/src/damage.rs",
        "formal/verus/src/injury.rs",
    ),
    (
        "crates/mud-rules/src/stats.rs",
        "formal/verus/src/wounds.rs",
    ),
    (
        "crates/mud-world/src/schedule.rs",
        "formal/verus/src/schedule.rs",
    ),
];

#[test]
fn the_classify_proof_covers_the_shipped_kernel() -> TestResult {
    mirrors(PAIRS[0].0, PAIRS[0].1)
}

#[test]
fn the_injury_proof_covers_the_shipped_kernel() -> TestResult {
    mirrors(PAIRS[1].0, PAIRS[1].1)
}

#[test]
fn the_wound_proof_covers_the_shipped_kernel() -> TestResult {
    mirrors(PAIRS[2].0, PAIRS[2].1)
}

#[test]
fn the_schedule_proof_covers_the_shipped_kernel() -> TestResult {
    mirrors(PAIRS[3].0, PAIRS[3].1)
}

#[test]
fn every_proof_module_is_checked() -> TestResult {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(root().join("formal/verus/src"))? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if name != "lib.rs" {
            found.push(format!("formal/verus/src/{name}"));
        }
    }
    found.sort();
    let mut want: Vec<String> = PAIRS.iter().map(|(_, p)| (*p).to_owned()).collect();
    want.sort();
    assert_eq!(found, want, "add the new proof module to PAIRS");
    Ok(())
}

#[test]
fn drift_on_either_side_fails() -> TestResult {
    let rust = std::fs::read_to_string(root().join(PAIRS[1].0))?;
    let verus = std::fs::read_to_string(root().join(PAIRS[1].1))?;
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
}
