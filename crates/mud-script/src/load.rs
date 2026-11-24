//! Reading and checking a pack's scripts.
//!
//! Every problem is collected and reported at once, with the exact
//! messages `docs/scripting.md` lists.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mud_world::hooks::{HookId, VerbInfo};
use mud_world::pack::{EntityKind, Tier};
use mud_world::parser::{BUILTIN_WORDS, ParseError, parse};
use mud_world::{EntityId, WorldPack};
use rhai::AST;
use serde_json::Value;

use crate::convert::config_json;
use crate::error::ScriptError;
use crate::host::{Compiled, HookEntry, MindEntry};
use crate::manifest::Manifest;
use crate::sandbox;

/// The largest script file, in bytes.
pub const FILE_MAX: usize = 65_536;
/// The most script bytes of a pack.
pub const TOTAL_MAX: usize = 524_288;
/// The most script files of a pack.
pub const FILES_MAX: usize = 64;

/// The text of a pack's scripts.
#[derive(Debug, Clone, Default)]
pub struct ScriptSources {
    /// `scripts.toml`, if the pack has one.
    pub manifest: Option<String>,
    /// Each script, by its path under `scripts/`.
    pub files: BTreeMap<String, String>,
    /// Problems found while reading.
    pub problems: Vec<String>,
    /// Things worth saying that are not errors.
    pub warnings: Vec<String>,
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0100_0000_01b3;

fn fnv(mut h: u64, bytes: &[u8]) -> u64 {
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

impl ScriptSources {
    /// The content hash: `rhai-` and 16 hex digits of FNV-1a-64 over the
    /// sorted `(path, 0, bytes, 0)` of `scripts.toml` and every script.
    #[must_use]
    pub fn version(&self) -> String {
        let mut entries: Vec<(String, &str)> = self
            .files
            .iter()
            .map(|(p, t)| (format!("scripts/{p}"), t.as_str()))
            .collect();
        if let Some(m) = &self.manifest {
            entries.push(("scripts.toml".to_owned(), m.as_str()));
        }
        entries.sort();
        let mut h = FNV_OFFSET;
        for (path, text) in entries {
            h = fnv(h, path.as_bytes());
            h = fnv(h, &[0]);
            h = fnv(h, text.as_bytes());
            h = fnv(h, &[0]);
        }
        format!("rhai-{h:016x}")
    }

    /// Reads the scripts of the pack in `pack_dir`.
    ///
    /// # Errors
    /// `scripts.toml` exists but cannot be read. Problems with script
    /// files are collected in `problems` instead.
    pub fn read_dir(pack_dir: &Path) -> Result<Self, ScriptError> {
        let mut out = Self::default();
        let manifest_path = pack_dir.join("scripts.toml");
        let scripts_dir = pack_dir.join("scripts");
        if !manifest_path.exists() {
            if scripts_dir.exists() {
                out.warnings
                    .push("scripts/ exists but there is no scripts.toml; no script runs".into());
            }
            return Ok(out);
        }
        let text = std::fs::read_to_string(&manifest_path).map_err(|source| ScriptError::Io {
            path: manifest_path.display().to_string(),
            source,
        })?;
        let wanted: BTreeSet<String> = toml::from_str::<Manifest>(&text)
            .map(|m| referenced_files(&m))
            .unwrap_or_default();
        out.manifest = Some(text);
        let root = scripts_dir.canonicalize().ok();
        let mut total = 0usize;
        for f in &wanted {
            if valid_path(f).is_err() {
                // The compile step reports the name.
                continue;
            }
            match read_one(root.as_deref(), &scripts_dir, f) {
                Ok(text) => {
                    total += text.len();
                    out.files.insert(f.clone(), text);
                }
                Err(p) => out.problems.push(p),
            }
        }
        if out.files.len() > FILES_MAX {
            out.problems
                .push(format!("more than {FILES_MAX} script files"));
        }
        if total > TOTAL_MAX {
            out.problems.push(format!(
                "the scripts total {total} bytes; the limit is {TOTAL_MAX}"
            ));
        }
        if let Some(root) = &root {
            let mut found = Vec::new();
            list_rhai(root, root, &mut found);
            for f in found {
                if !wanted.contains(&f) {
                    out.warnings
                        .push(format!("scripts/{f}: not used by scripts.toml"));
                }
            }
        }
        Ok(out)
    }
}

fn read_one(root: Option<&Path>, scripts_dir: &Path, f: &str) -> Result<String, String> {
    let path = scripts_dir.join(f);
    let meta = std::fs::symlink_metadata(&path).map_err(|_| format!("{f}: no such file"))?;
    let inside = root.is_some_and(|r| path.canonicalize().is_ok_and(|p| p.starts_with(r)));
    if meta.file_type().is_symlink() || !inside {
        return Err(format!("{f}: resolves outside the pack's scripts folder"));
    }
    let n = usize::try_from(meta.len()).unwrap_or(usize::MAX);
    if n > FILE_MAX {
        return Err(format!("{f}: is {n} bytes; the limit is {FILE_MAX}"));
    }
    std::fs::read_to_string(&path).map_err(|e| format!("{f}: {e}"))
}

fn list_rhai(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for p in paths {
        if p.is_dir() {
            list_rhai(root, &p, out);
        } else if p.extension().is_some_and(|x| x == "rhai")
            && let Ok(rel) = p.strip_prefix(root)
        {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// Every file the manifest names, the library included.
#[must_use]
pub fn referenced_files(m: &Manifest) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    out.extend(m.scripts.lib.clone());
    out.extend(m.minds.values().map(|b| b.file.clone()));
    out.extend(m.rooms.values().map(|b| b.file.clone()));
    out.extend(m.items.values().map(|b| b.file.clone()));
    out.extend(m.verbs.iter().map(|b| b.file.clone()));
    out
}

/// Checks a file name: a relative path of `[a-z0-9_/-]` ending in `.rhai`.
fn valid_path(f: &str) -> Result<(), ()> {
    let stem = f.strip_suffix(".rhai").ok_or(())?;
    let ok = !stem.is_empty()
        && !stem.starts_with('/')
        && !stem.ends_with('/')
        && !stem.contains("//")
        && stem
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '/' | '-'));
    if ok { Ok(()) } else { Err(()) }
}

fn valid_verb_name(v: &str) -> bool {
    (2..=16).contains(&v.len()) && v.chars().all(|c| c.is_ascii_lowercase() || c == '_')
}

/// What a binding points at: its file, its function, its config, and a
/// name for messages.
struct Binding<'a> {
    hook: Option<HookId>,
    mind: Option<EntityId>,
    file: &'a str,
    func: String,
    config: &'a toml::Table,
    place: String,
}

/// Checks the manifest against the pack and compiles every script.
///
/// # Errors
/// Every problem found, at once.
#[expect(clippy::too_many_lines, reason = "one flat list of checks reads best")]
pub fn compile(sources: &ScriptSources, pack: &WorldPack) -> Result<Compiled, ScriptError> {
    let mut problems = sources.problems.clone();
    let version = sources.version();
    let Some(text) = &sources.manifest else {
        return if problems.is_empty() {
            Ok(Compiled::empty(version, sources.warnings.clone()))
        } else {
            Err(ScriptError::Invalid(problems))
        };
    };
    let manifest: Manifest = match toml::from_str(text) {
        Ok(m) => m,
        Err(e) => {
            problems.push(format!("scripts.toml: {e}"));
            return Err(ScriptError::Invalid(problems));
        }
    };
    let limits = manifest.limits.check(&mut problems);

    let mut bindings: Vec<Binding<'_>> = Vec::new();
    for (id, b) in &manifest.minds {
        match pack.cast.get(id) {
            None => problems.push(format!("scripts.toml minds.{id}: unknown cast entry")),
            Some(c)
                if c.kind == EntityKind::Mob
                    || !matches!(c.mind.tier, Tier::Ambient | Tier::Persistent | Tier::Turing) =>
            {
                problems.push(format!(
                    "scripts.toml minds.{id}: only ambient, persistent and turing entities have minds"
                ));
            }
            Some(_) => bindings.push(Binding {
                hook: None,
                mind: Some(id.clone()),
                file: &b.file,
                func: "on_wake".into(),
                config: &b.config,
                place: format!("minds.{id}"),
            }),
        }
    }
    for (id, b) in &manifest.rooms {
        if !pack.rooms.contains_key(id) {
            problems.push(format!("scripts.toml rooms.{id}: unknown room"));
            continue;
        }
        if !b.on_enter && !b.on_speech {
            problems.push(format!(
                "scripts.toml rooms.{id}: declares no hook (on_enter or on_speech)"
            ));
        }
        for (on, hook, func) in [
            (b.on_enter, HookId::Enter(id.clone()), "on_enter"),
            (b.on_speech, HookId::Speech(id.clone()), "on_speech"),
        ] {
            if on {
                bindings.push(Binding {
                    hook: Some(hook),
                    mind: None,
                    file: &b.file,
                    func: func.into(),
                    config: &b.config,
                    place: format!("rooms.{id}"),
                });
            }
        }
    }
    for (id, b) in &manifest.items {
        let Some(def) = pack.items.get(id) else {
            problems.push(format!("scripts.toml items.{id}: unknown item"));
            continue;
        };
        if !b.on_take && !b.on_use {
            problems.push(format!(
                "scripts.toml items.{id}: declares no hook (on_take or on_use)"
            ));
        }
        let engine_use = if def.key.is_some() {
            Some("key")
        } else if def.container {
            Some("container")
        } else if def.currency.is_some() {
            Some("currency")
        } else if def.heal.is_some() {
            Some("heal")
        } else if def.slot.is_some() {
            Some("slot")
        } else {
            None
        };
        if b.on_use
            && let Some(u) = engine_use
        {
            problems.push(format!(
                "scripts.toml items.{id}: the item already has a use ({u}); on_use would never run"
            ));
        }
        for (on, hook, func) in [
            (b.on_take, HookId::Take(id.clone()), "on_take"),
            (b.on_use, HookId::Use(id.clone()), "on_use"),
        ] {
            if on {
                bindings.push(Binding {
                    hook: Some(hook),
                    mind: None,
                    file: &b.file,
                    func: func.into(),
                    config: &b.config,
                    place: format!("items.{id}"),
                });
            }
        }
    }
    let mut verbs = BTreeMap::new();
    for (i, v) in manifest.verbs.iter().enumerate() {
        let name = &v.name;
        if !valid_verb_name(name) {
            problems.push(format!(
                "scripts.toml verbs[{i}] {name:?}: a verb is 2 to 16 lowercase letters or underscores"
            ));
            continue;
        }
        if BUILTIN_WORDS.contains(&name.as_str())
            || !matches!(parse(name), Err(ParseError::UnknownVerb(_)))
        {
            problems.push(format!(
                "scripts.toml verbs[{i}] {name:?} shadows a built-in verb"
            ));
            continue;
        }
        if verbs.contains_key(name) {
            problems.push(format!(
                "scripts.toml verbs[{i}] {name:?} is declared twice"
            ));
            continue;
        }
        for r in &v.rooms {
            if !pack.rooms.contains_key(r) {
                problems.push(format!(
                    "scripts.toml verbs[{i}] names unknown room {:?}",
                    r.as_str()
                ));
            }
        }
        if v.usage.chars().count() > 60
            || v.usage.trim().is_empty()
            || v.usage.chars().any(mud_world::parser::forbidden_in_text)
        {
            problems.push(format!(
                "scripts.toml verbs[{i}] {name:?}: usage is 1 to 60 printable characters"
            ));
        }
        verbs.insert(
            name.clone(),
            VerbInfo {
                name: name.clone(),
                usage: v.usage.clone(),
                rooms: v.rooms.clone(),
            },
        );
        bindings.push(Binding {
            hook: Some(HookId::Verb(name.clone())),
            mind: None,
            file: &v.file,
            func: v.function(),
            config: &v.config,
            place: format!("verbs[{i}]"),
        });
    }

    let mut files = BTreeSet::new();
    files.extend(manifest.scripts.lib.clone());
    for b in &bindings {
        files.insert(b.file.to_owned());
    }
    for f in &files {
        if valid_path(f).is_err() {
            problems.push(format!(
                "scripts.toml: file {f:?} must be a relative path of [a-z0-9_/-] ending in .rhai"
            ));
        }
    }
    let mut configs = BTreeMap::new();
    for b in &bindings {
        match config_json(b.config) {
            Ok(v) => {
                configs.insert(b.place.clone(), Arc::new(v));
            }
            Err(key) => problems.push(format!(
                "scripts.toml {}: config holds a float or a date at {key}; scripts use integers",
                b.place
            )),
        }
    }

    let engine = sandbox::engine(&limits);
    let compile_one = |f: &str, problems: &mut Vec<String>| -> Option<AST> {
        let Some(text) = sources.files.get(f) else {
            let missing = format!("{f}: no such file");
            let reported = problems.iter().any(|p| p.starts_with(&format!("{f}: ")));
            if valid_path(f).is_ok() && !reported {
                problems.push(missing);
            }
            return None;
        };
        match engine.compile(text) {
            Ok(ast) => {
                if let Err(e) = sandbox::refuse_top_level(&ast) {
                    problems.push(format!("{f}: {e}"));
                    return None;
                }
                Some(ast)
            }
            Err(e) => {
                let p = e.position();
                problems.push(format!(
                    "{f}:{}:{}: {}",
                    p.line().unwrap_or(0),
                    p.position().unwrap_or(0),
                    e.err_type()
                ));
                None
            }
        }
    };
    let lib = manifest
        .scripts
        .lib
        .as_deref()
        .and_then(|f| compile_one(f, &mut problems));
    let mut asts: BTreeMap<String, Arc<AST>> = BTreeMap::new();
    for f in &files {
        if Some(f) == manifest.scripts.lib.as_ref() {
            if let Some(l) = &lib {
                asts.insert(f.clone(), Arc::new(l.clone()));
            }
            continue;
        }
        if let Some(ast) = compile_one(f, &mut problems) {
            let merged = lib.as_ref().map_or_else(|| ast.clone(), |l| l.merge(&ast));
            asts.insert(f.clone(), Arc::new(merged));
        }
    }

    let mut hooks = BTreeMap::new();
    let mut minds = BTreeMap::new();
    for b in &bindings {
        let Some(ast) = asts.get(b.file) else {
            continue;
        };
        let what = b
            .hook
            .as_ref()
            .map_or_else(|| format!("{}.on_wake", b.place), HookId::key);
        if let Some(p) = check_fn(ast, b.file, &b.func, &what) {
            problems.push(p);
            continue;
        }
        let config = configs
            .get(&b.place)
            .cloned()
            .unwrap_or_else(|| Arc::new(Value::Object(serde_json::Map::new())));
        if let Some(h) = &b.hook {
            hooks.insert(
                h.clone(),
                HookEntry {
                    ast: Arc::clone(ast),
                    func: b.func.clone(),
                    config,
                },
            );
        } else if let Some(id) = &b.mind {
            let m = &manifest.minds[id];
            minds.insert(
                id.clone(),
                MindEntry {
                    ast: Arc::clone(ast),
                    config,
                    runtime: m.runtime,
                    memory_schema: m.memory_schema,
                },
            );
        }
    }
    if !problems.is_empty() {
        return Err(ScriptError::Invalid(problems));
    }
    Ok(Compiled {
        version,
        files: files.len(),
        limits,
        engine,
        hooks,
        minds,
        verbs,
        warnings: sources.warnings.clone(),
    })
}

fn check_fn(ast: &AST, file: &str, func: &str, what: &str) -> Option<String> {
    let found: Vec<usize> = ast
        .iter_functions()
        .filter(|f| f.name == func)
        .map(|f| f.params.len())
        .collect();
    if found.contains(&1) {
        return None;
    }
    Some(found.first().map_or_else(
        || format!("{file}: no fn {func}(ctx) for {what}"),
        |n| format!("{file}: fn {func} takes {n} parameters; hooks take ctx"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_verb_names() {
        assert!(valid_path("rooms/ossuary.rhai").is_ok());
        assert!(valid_path("a-b_c/d1.rhai").is_ok());
        for bad in [
            "../x.rhai",
            "/x.rhai",
            "x.lua",
            ".rhai",
            "A.rhai",
            "a//b.rhai",
            "a/.rhai",
        ] {
            assert!(valid_path(bad).is_err(), "{bad}");
        }
        assert!(valid_verb_name("pray") && valid_verb_name("lay_low"));
        assert!(!valid_verb_name("p") && !valid_verb_name("Pray") && !valid_verb_name("pray1"));
    }

    #[test]
    fn the_version_changes_with_any_byte() {
        let mut s = ScriptSources {
            manifest: Some("[scripts]\n".into()),
            ..ScriptSources::default()
        };
        s.files.insert("a.rhai".into(), "fn f(ctx) { 1 }".into());
        let v1 = s.version();
        assert!(v1.starts_with("rhai-") && v1.len() == 21, "{v1}");
        s.files.insert("a.rhai".into(), "fn f(ctx) { 2 }".into());
        assert_ne!(s.version(), v1);
        let mut t = s.clone();
        t.files.insert("b.rhai".into(), String::new());
        assert_ne!(t.version(), s.version(), "a new empty file counts");
        assert_eq!(s.version(), s.clone().version());
    }
}
