//! `mud.toml.example` stays honest: it parses into [`MudConfig`], it names
//! every key the config accepts (set or commented out), it names no key
//! the config refuses, and each key line says what it is. The README, the
//! example, CI and the code name one minimum littleguy commit.

use std::collections::BTreeSet;

use serde_json::Value;

use super::*;

const EXAMPLE: &str = include_str!("../../../../mud.toml.example");
const README: &str = include_str!("../../../../README.md");
const CI: &str = include_str!("../../../../.github/workflows/ci.yml");

/// The keys of the example: each `[section]` and each `(section, key)`,
/// whether set or commented out. A commented line counts when, past its
/// `#`, it is a header or a `key = value` line, as an operator would
/// uncomment it.
#[derive(Debug, Default)]
struct Keys {
    sections: BTreeSet<String>,
    keys: BTreeSet<(String, String)>,
}

/// A `[a.b]` or `[[a.b]]` header, alone on its line (a comment may follow).
fn header(line: &str) -> Option<&str> {
    let body = line.split('#').next().unwrap_or("").trim();
    let inner = body
        .strip_prefix("[[")
        .and_then(|b| b.strip_suffix("]]"))
        .or_else(|| body.strip_prefix('[').and_then(|b| b.strip_suffix(']')))?;
    inner
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.')
        .then_some(inner)
}

/// The key of a `key = value` line.
fn key(line: &str) -> Option<&str> {
    let (k, v) = line.split_once('=')?;
    let k = k.trim_end();
    (!k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !v.trim().is_empty())
    .then_some(k)
}

/// The line past its leading `#`s and spaces, and whether it had any.
fn uncommented(line: &str) -> (&str, bool) {
    let t = line.trim_start();
    t.strip_prefix('#').map_or((t, false), |rest| {
        (rest.trim_start_matches('#').trim_start(), true)
    })
}

fn example_keys(text: &str) -> Keys {
    let mut out = Keys::default();
    let mut section = String::new();
    for line in text.lines() {
        let (body, _) = uncommented(line);
        if let Some(h) = header(body) {
            section = h.to_owned();
            // `[models.flash]` also shows `[models]`.
            for (i, c) in h.char_indices() {
                if c == '.' {
                    out.sections.insert(h[..i].to_owned());
                }
            }
            out.sections.insert(section.clone());
        } else if let Some(k) = key(body) {
            out.keys.insert((section.clone(), k.to_owned()));
        }
    }
    out
}

/// The keys of the config's serde shape, walked from `v`: an object is a
/// section, an array of objects is a `[[section]]`, anything else (a
/// `null` too: an unset `Option`) is a key.
fn shape_keys(v: &Value, section: &str, out: &mut Keys) {
    let Value::Object(map) = v else {
        return;
    };
    for (k, child) in map {
        let path = if section.is_empty() {
            k.clone()
        } else {
            format!("{section}.{k}")
        };
        match child {
            Value::Object(_) => {
                out.sections.insert(path.clone());
                shape_keys(child, &path, out);
            }
            Value::Array(items) if items.iter().any(Value::is_object) => {
                out.sections.insert(path.clone());
                for item in items {
                    shape_keys(item, &path, out);
                }
            }
            _ => {
                out.keys.insert((section.to_owned(), k.clone()));
            }
        }
    }
}

/// The default config with one entry in each map and list of tables, so
/// the walk reaches the keys of their values. Each entry is one the
/// example shows.
fn populated() -> MudConfig {
    let mut cfg = MudConfig::default();
    cfg.models.insert(
        "flash".into(),
        ModelConfig {
            base_url: String::new(),
            model: String::new(),
            key_env: String::new(),
        },
    );
    cfg.models.insert(
        "council".into(),
        ModelConfig {
            base_url: String::new(),
            model: String::new(),
            key_env: String::new(),
        },
    );
    cfg.littleguy
        .model_names
        .insert("default".into(), String::new());
    cfg.supervisor.budgets.insert("pc_kestrel".into(), 0);
    cfg.littleguy
        .steward
        .guardrail
        .push(mud_supervisor::steward::vet::Snippet {
            id: String::new(),
            text: String::new(),
        });
    cfg
}

fn config_keys(cfg: &MudConfig) -> Keys {
    let mut out = Keys::default();
    shape_keys(&serde_json::to_value(cfg).unwrap(), "", &mut out);
    out
}

fn missing(want: &Keys, have: &Keys) -> Vec<String> {
    let sections = want
        .sections
        .difference(&have.sections)
        .map(|s| format!("[{s}]"));
    let keys = want.keys.difference(&have.keys).map(|(s, k)| {
        if s.is_empty() {
            k.clone()
        } else {
            format!("[{s}] {k}")
        }
    });
    sections.chain(keys).collect()
}

#[test]
fn the_example_parses_into_the_config() {
    let ex: MudConfig = toml::from_str(EXAMPLE).unwrap();
    assert!(ex.littleguy.check().is_ok());
    assert!(ex.littleguy.check_steward(&ex.models).is_ok());
    assert!(ex.supervisor.check_restart().is_ok());
}

/// A field added to the config without a line in the example fails here.
#[test]
fn the_example_names_every_key_the_config_accepts() {
    let gaps = missing(&config_keys(&populated()), &example_keys(EXAMPLE));
    assert!(
        gaps.is_empty(),
        "mud.toml.example does not name these (add each, commented out if it has no good example value, with a comment): {gaps:?}"
    );
}

/// A key removed or renamed in the config, but left in the example (even
/// commented out), fails here.
#[test]
fn the_example_names_no_key_the_config_refuses() {
    let stale = missing(&example_keys(EXAMPLE), &config_keys(&populated()));
    assert!(
        stale.is_empty(),
        "mud.toml.example names keys the config refuses: {stale:?}"
    );
}

/// Each key line says what it is: a comment after it on the line, or a
/// comment line right above it.
#[test]
fn every_key_in_the_example_has_a_comment() {
    let lines: Vec<&str> = EXAMPLE.lines().collect();
    let mut bare = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let (body, _) = uncommented(line);
        if key(body).is_none() || header(body).is_some() {
            continue;
        }
        let trailing = body.split_once('=').is_some_and(|(_, v)| v.contains(" #"));
        let above = i
            .checked_sub(1)
            .and_then(|j| lines.get(j))
            .is_some_and(|prev| {
                let (b, commented) = uncommented(prev);
                commented && key(b).is_none() && header(b).is_none() && !b.is_empty()
            });
        if !trailing && !above {
            bare.push(format!("line {}: {line}", i + 1));
        }
    }
    assert!(
        bare.is_empty(),
        "keys with no comment in mud.toml.example: {bare:#?}"
    );
}

#[test]
fn the_walk_sees_a_field_the_example_lacks() {
    // The check above is only as good as this: a key the example does not
    // show is reported, in its section.
    let mut have = example_keys(EXAMPLE);
    have.keys
        .remove(&("littleguy".to_owned(), "bin".to_owned()));
    assert_eq!(
        missing(&config_keys(&populated()), &have),
        ["[littleguy] bin"]
    );
    let commented = example_keys("[a]\n# b = 1   # c\n# [[a.d]]\n# e = \"f\"\n");
    assert!(commented.keys.contains(&("a".into(), "b".into())));
    assert!(commented.keys.contains(&("a.d".into(), "e".into())));
    assert!(commented.sections.contains("a.d"));
    assert!(
        example_keys("# [littleguy] keys turn it off\n")
            .sections
            .is_empty(),
        "prose"
    );
}

/// The minimum littleguy commit is one string: the code's, which CI pins
/// and the README and the example name.
#[test]
fn one_minimum_littleguy_commit_everywhere() {
    use mud_supervisor::littleguy::compat::{LG_MIN_COMMIT, README_INSTALL};
    assert_eq!(LG_MIN_COMMIT.len(), 40, "a full commit id");
    let pinned: Vec<&str> = CI
        .lines()
        .filter_map(|l| l.trim().strip_prefix("ref: "))
        .collect();
    assert_eq!(
        pinned,
        [LG_MIN_COMMIT],
        "ci.yml pins the littleguy checkout to it"
    );
    assert!(EXAMPLE.contains(LG_MIN_COMMIT), "mud.toml.example names it");
    assert!(README.contains(LG_MIN_COMMIT), "the README names it");
    // Every 40-hex id in the README and the example is that one.
    for (name, text) in [("README.md", README), ("mud.toml.example", EXAMPLE)] {
        for word in text.split(|c: char| !c.is_ascii_hexdigit()) {
            if word.len() == 40 {
                assert_eq!(word, LG_MIN_COMMIT, "{name} names another commit");
            }
        }
    }
    let anchor = README_INSTALL.strip_prefix("README.md#").unwrap();
    assert_eq!(anchor, "install-littleguy");
    assert!(
        README.lines().any(|l| l == "## Install littleguy"),
        "the section the warnings name"
    );
    assert!(EXAMPLE.contains(README_INSTALL), "the example points at it");
}

/// The example says which lg.toml parts the MUD writes, and how to turn
/// off each optional one.
#[test]
fn the_example_lists_the_optional_lg_parts() {
    use mud_supervisor::littleguy::compat::Optional;
    for part in Optional::ALL {
        assert!(
            EXAMPLE.contains(part.path()),
            "{} in mud.toml.example",
            part.path()
        );
        assert!(
            EXAMPLE.contains(part.off_key()),
            "{} in mud.toml.example",
            part.off_key()
        );
        assert!(
            README.contains(part.path()),
            "{} in the README",
            part.path()
        );
    }
}
