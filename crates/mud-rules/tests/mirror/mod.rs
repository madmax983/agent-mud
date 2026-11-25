//! The shared logic of the Verus mirror guards (`verus_mirror` in
//! `mud-rules`, `mud-supervisor`, `mud-server` and `mud-accounts`).
//!
//! Each Verus proof in `formal/verus` mirrors Rust source text. A file marks
//! each mirrored region with `// mirror-begin <name>` and
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

pub type TestResult = Result<(), Box<dyn Error>>;

const BEGIN: &str = "// mirror-begin ";
const END: &str = "// mirror-end ";

/// The mirrored regions of `text`, by name, with their raw lines.
pub fn regions(text: &str) -> Result<BTreeMap<String, String>, String> {
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
pub fn drop_spec_clauses(body: &str) -> String {
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

pub fn squeeze(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Rewrites a named return `->(r:T)` (white space removed) as `->T`. `T`
/// runs to the matching `)`, so a tuple type such as `(u8,bool)` is kept.
pub fn unname_return(squeezed: &str) -> String {
    const NAMED: &str = "->(r:";
    let mut out = String::new();
    let mut rest = squeezed;
    while let Some(at) = rest.find(NAMED) {
        out.push_str(&rest[..at]);
        out.push_str("->");
        let after = &rest[at + NAMED.len()..];
        let mut depth = 0usize;
        let close = after.char_indices().find_map(|(i, c)| match c {
            '(' => {
                depth += 1;
                None
            }
            ')' if depth == 0 => Some(i),
            ')' => {
                depth -= 1;
                None
            }
            _ => None,
        });
        if let Some(close) = close {
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

pub fn normalize_rust(body: &str) -> String {
    squeeze(body)
}

pub fn normalize_verus(body: &str) -> String {
    unname_return(&squeeze(&drop_spec_clauses(body)))
}

/// Fails unless the Rust text and the Verus text mirror the same regions.
pub fn compare(rust: &str, verus: &str) -> Result<(), String> {
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

pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn mirrors(source: &str, proof: &str) -> TestResult {
    let rust = std::fs::read_to_string(root().join(source))?;
    let verus = std::fs::read_to_string(root().join(proof))?;
    compare(&rust, &verus).map_err(|e| format!("update {proof} to match {source}: {e}"))?;
    Ok(())
}

/// Each Verus module and the Rust file it mirrors.
pub const PAIRS: [(&str, &str); 18] = [
    ("classify", "crates/mud-rules/src/roll.rs"),
    ("injury", "crates/mud-rules/src/damage.rs"),
    ("wounds", "crates/mud-rules/src/stats.rs"),
    ("schedule", "crates/mud-world/src/schedule.rs"),
    ("chatter", "crates/mud-supervisor/src/chatter.rs"),
    ("budget", "crates/mud-supervisor/src/budget.rs"),
    ("typing", "crates/mud-supervisor/src/turing.rs"),
    ("connections", "crates/mud-server/src/limits.rs"),
    ("hooks", "crates/mud-world/src/hooks.rs"),
    ("login_route", "crates/mud-accounts/src/route.rs"),
    ("throttle", "crates/mud-accounts/src/throttle.rs"),
    ("rewards", "crates/mud-world/src/rewards.rs"),
    ("side", "crates/mud-world/src/side.rs"),
    ("durability", "crates/mud-world/src/durability.rs"),
    ("persist", "crates/mud-supervisor/src/persist.rs"),
    ("replay", "crates/mud-supervisor/src/replay.rs"),
    ("recovery", "crates/mud-rules/src/recovery.rs"),
    ("admit", "crates/mud-supervisor/src/admit.rs"),
];

/// The Verus file of a module named in [`PAIRS`].
pub fn proof_path(module: &str) -> String {
    format!("formal/verus/src/{module}.rs")
}

/// Fails unless the Verus module `module` mirrors its Rust file.
pub fn proof_mirrors(module: &str) -> TestResult {
    let (_, source) = PAIRS
        .iter()
        .find(|(m, _)| *m == module)
        .ok_or_else(|| format!("{module} is not in PAIRS"))?;
    mirrors(source, &proof_path(module))
}

/// Fails unless every module in `formal/verus/src` is in [`PAIRS`].
pub fn every_proof_module_is_listed() -> TestResult {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(root().join("formal/verus/src"))? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if name != "lib.rs" {
            found.push(format!("formal/verus/src/{name}"));
        }
    }
    found.sort();
    let mut want: Vec<String> = PAIRS.iter().map(|(m, _)| proof_path(m)).collect();
    want.sort();
    if found != want {
        return Err(
            format!("add the new proof module to PAIRS: found {found:?}, listed {want:?}").into(),
        );
    }
    Ok(())
}
