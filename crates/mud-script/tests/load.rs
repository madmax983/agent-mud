//! Loading and checking a pack's scripts: every problem, at once.
#![expect(clippy::unwrap_used, reason = "test helpers fail loudly")]

use std::path::Path;
use std::sync::Arc;

use mud_script::{ScriptError, ScriptHost, ScriptSources};
use mud_world::WorldPack;

fn pack() -> Arc<WorldPack> {
    Arc::new(WorldPack::crypt_v01().unwrap())
}

fn problems(manifest: &str, files: &[(&str, &str)]) -> Vec<String> {
    let mut s = ScriptSources {
        manifest: Some(manifest.into()),
        ..ScriptSources::default()
    };
    for (p, t) in files {
        s.files.insert((*p).into(), (*t).into());
    }
    match ScriptHost::from_sources(&s, &pack()) {
        Err(e) => e.problems(),
        Ok(_) => Vec::new(),
    }
}

fn write(dir: &Path, path: &str, text: &str) {
    let p = dir.join(path);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

#[test]
fn every_problem_is_reported_at_once() {
    let manifest = r#"
[limits]
max_effects = 40
[minds.nobody]
file = "m.rhai"
[minds.giant_rat_1]
file = "m.rhai"
[rooms.nowhere]
file = "r.rhai"
on_enter = true
[rooms.town_square]
file = "r.rhai"
[items.ghost]
file = "i.rhai"
on_take = true
[items.torch]
file = "i.rhai"
on_use = true
[[verbs]]
name = "Pray!"
usage = "pray"
file = "v.rhai"
[[verbs]]
name = "look"
usage = "look"
file = "v.rhai"
[[verbs]]
name = "dance"
usage = "dance"
rooms = ["moon"]
file = "v.rhai"
[[verbs]]
name = "dance"
usage = "dance"
file = "v.rhai"
"#;
    let p = problems(
        manifest,
        &[
            ("v.rhai", "fn on_dance(ctx) { [] }"),
            ("i.rhai", "fn on_use(a, b) { [] }"),
        ],
    );
    for want in [
        "scripts.toml limits.max_effects = 40 is outside 1..=16",
        "scripts.toml minds.nobody: unknown cast entry",
        "scripts.toml minds.giant_rat_1: only ambient, persistent and turing entities have minds",
        "scripts.toml rooms.nowhere: unknown room",
        "scripts.toml rooms.town_square: declares no hook (on_enter or on_speech)",
        "scripts.toml items.ghost: unknown item",
        "scripts.toml items.torch: the item already has a use (slot); on_use would never run",
        "scripts.toml verbs[0] \"Pray!\": a verb is 2 to 16 lowercase letters or underscores",
        "scripts.toml verbs[1] \"look\" shadows a built-in verb",
        "scripts.toml verbs[2] names unknown room \"moon\"",
        "scripts.toml verbs[3] \"dance\" is declared twice",
        "i.rhai: fn on_use takes 2 parameters; hooks take ctx",
    ] {
        assert!(p.iter().any(|x| x == want), "missing {want:?} in {p:#?}");
    }
    let bad = problems("[rooms.x\n", &[]);
    assert!(bad[0].starts_with("scripts.toml: "), "{bad:?}");
}

#[test]
fn paths_cannot_escape_the_scripts_folder() {
    let p = problems(
        "[rooms.dark_alley]\nfile = \"../world.rhai\"\non_speech = true\n",
        &[],
    );
    assert!(
        p.contains(&"scripts.toml: file \"../world.rhai\" must be a relative path of [a-z0-9_/-] ending in .rhai".to_owned()),
        "{p:?}"
    );
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(outside.path(), "evil.rhai", "fn on_speech(ctx) { [] }");
    write(
        dir.path(),
        "scripts.toml",
        "[rooms.dark_alley]\nfile = \"evil.rhai\"\non_speech = true\n",
    );
    std::fs::create_dir_all(dir.path().join("scripts")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        outside.path().join("evil.rhai"),
        dir.path().join("scripts/evil.rhai"),
    )
    .unwrap();
    let err = ScriptHost::load(dir.path(), &pack()).unwrap_err();
    assert!(
        err.problems()
            .contains(&"evil.rhai: resolves outside the pack's scripts folder".to_owned()),
        "{err}"
    );
}

#[test]
fn oversized_and_too_many_files_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut manifest = String::new();
    for i in 0..66 {
        let name = format!("v{i:02}.rhai");
        let body = if i == 0 {
            format!("fn on_aa(ctx) {{ [] }}\n// {}", "x".repeat(70_000))
        } else {
            "fn on_aa(ctx) { [] }".to_owned()
        };
        write(dir.path(), &format!("scripts/{name}"), &body);
        let verb = format!(
            "[[verbs]]\nname = \"va{}\"\nusage = \"v\"\nfile = \"{name}\"\nfn = \"on_aa\"\n",
            letters(i)
        );
        manifest += &verb;
    }
    write(dir.path(), "scripts.toml", &manifest);
    let err = ScriptHost::load(dir.path(), &pack()).unwrap_err();
    let p = err.problems();
    assert!(
        p.iter()
            .any(|x| x.starts_with("v00.rhai: is ") && x.ends_with(" bytes; the limit is 65536")),
        "{p:?}"
    );
    assert!(p.contains(&"more than 64 script files".to_owned()), "{p:?}");
}

/// A distinct lowercase suffix for verb `i`.
fn letters(i: usize) -> String {
    let a = u8::try_from(i / 26).unwrap();
    let b = u8::try_from(i % 26).unwrap();
    format!("{}{}", char::from(b'a' + a), char::from(b'a' + b))
}

#[test]
fn a_missing_hook_fn_names_its_file() {
    let p = problems(
        "[rooms.dark_alley]\nfile = \"a.rhai\"\non_speech = true\n[rooms.ossuary_crypt]\nfile = \"missing.rhai\"\non_enter = true\n",
        &[("a.rhai", "fn on_enter(ctx) { [] }")],
    );
    assert!(
        p.contains(&"a.rhai: no fn on_speech(ctx) for rooms.dark_alley.on_speech".to_owned()),
        "{p:?}"
    );
    assert!(
        p.contains(&"missing.rhai: no such file".to_owned()),
        "{p:?}"
    );
}

#[test]
fn a_verb_cannot_shadow_a_built_in() {
    for v in ["read", "go", "me", "north", "kill", "ne"] {
        let p = problems(
            &format!("[[verbs]]\nname = \"{v}\"\nusage = \"x\"\nfile = \"a.rhai\"\n"),
            &[("a.rhai", "fn x(ctx) { [] }")],
        );
        assert!(
            p.iter().any(|x| x.contains("shadows a built-in verb")),
            "{v}: {p:?}"
        );
    }
}

#[test]
fn on_use_cannot_shadow_an_engine_use() {
    for (item, why) in [
        ("grate_key", "key"),
        ("rusted_lockbox", "container"),
        ("old_coin_pouch", "currency"),
        ("bandage", "heal"),
        ("dagger", "slot"),
    ] {
        let p = problems(
            &format!("[items.{item}]\nfile = \"a.rhai\"\non_use = true\n"),
            &[("a.rhai", "fn on_use(ctx) { [] }")],
        );
        assert!(
            p.contains(&format!("scripts.toml items.{item}: the item already has a use ({why}); on_use would never run")),
            "{p:?}"
        );
    }
}

#[test]
fn a_mob_has_no_mind_script() {
    let p = problems(
        "[minds.skeletal_warden]\nfile = \"a.rhai\"\n",
        &[("a.rhai", "fn on_wake(ctx) { [] }")],
    );
    assert_eq!(
        p,
        [
            "scripts.toml minds.skeletal_warden: only ambient, persistent and turing entities have minds"
        ]
    );
}

#[test]
fn config_floats_are_refused() {
    let p = problems(
        "[rooms.dark_alley]\nfile = \"a.rhai\"\non_speech = true\nconfig = { odds = 0.5 }\n",
        &[("a.rhai", "fn on_speech(ctx) { [] }")],
    );
    assert_eq!(
        p,
        [
            "scripts.toml rooms.dark_alley: config holds a float or a date at odds; scripts use integers"
        ]
    );
}

#[test]
fn a_pack_without_scripts_toml_loads_empty() {
    let dir = tempfile::tempdir().unwrap();
    let host = ScriptHost::load(dir.path(), &pack()).unwrap();
    assert!(host.hook_keys().is_empty() && host.minds().is_empty());
    assert!(
        host.summary()
            .ends_with("(0 files: 0 minds, 0 rooms, 0 items, 0 verbs)")
    );
    assert_eq!(host.dry_run(&pack()), Vec::<String>::new());
    assert_eq!(host.current().warnings, Vec::<String>::new());
}

#[test]
fn a_scripts_folder_without_a_manifest_is_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "scripts/a.rhai", "fn x(ctx) { [] }");
    let s = ScriptSources::read_dir(dir.path()).unwrap();
    assert_eq!(
        s.warnings,
        ["scripts/ exists but there is no scripts.toml; no script runs"]
    );
    write(
        dir.path(),
        "scripts.toml",
        "[rooms.dark_alley]\nfile = \"b.rhai\"\non_speech = true\n",
    );
    write(dir.path(), "scripts/b.rhai", "fn on_speech(ctx) { [] }");
    let s = ScriptSources::read_dir(dir.path()).unwrap();
    assert_eq!(s.warnings, ["scripts/a.rhai: not used by scripts.toml"]);
    assert!(ScriptHost::load(dir.path(), &pack()).is_ok());
}

#[test]
fn reloads_read_the_folder_again_and_refuse_runtime_changes() {
    let dir = tempfile::tempdir().unwrap();
    let pack = pack();
    write(
        dir.path(),
        "scripts.toml",
        "[minds.guard_01]\nfile = \"h.rhai\"\nruntime = \"rhai\"\n",
    );
    write(dir.path(), "scripts/h.rhai", "fn on_wake(ctx) { [] }");
    let host = ScriptHost::load(dir.path(), &pack).unwrap();
    let v1 = host.version();
    write(
        dir.path(),
        "scripts/h.rhai",
        "fn on_wake(ctx) { [\"say hi\"] }",
    );
    let next = host.compile_from_source(&pack).unwrap();
    assert_ne!(next.version, v1);
    host.install(next);
    assert_eq!(host.reloads(), 1);
    write(
        dir.path(),
        "scripts.toml",
        "[minds.guard_01]\nfile = \"h.rhai\"\n",
    );
    let err = host.compile_from_source(&pack).unwrap_err();
    assert_eq!(
        err.problems(),
        ["minds.guard_01: runtime changes need a restart"]
    );
    write(
        dir.path(),
        "scripts.toml",
        "[minds.guard_01]\nfile = \"h.rhai\"\nruntime = \"rhai\"\n",
    );
    write(dir.path(), "scripts/h.rhai", "fn on_wake(ctx) { ctx.nope }");
    let err = host.compile_from_source(&pack).unwrap_err();
    assert!(
        err.problems()[0].starts_with("dry run minds.guard_01.on_wake: Property not found: nope"),
        "{err}"
    );
    let bundled = ScriptHost::bundled_crypt_v01(&pack).unwrap();
    assert!(matches!(
        bundled.compile_from_source(&pack),
        Err(ScriptError::Bundled)
    ));
}

#[test]
fn script_views_hold_no_floats() {
    use mud_world::hooks::HookId;
    let pack = pack();
    let world = mud_world::World::new(Arc::clone(&pack), 1);
    for room in pack.rooms.keys() {
        let input = world.probe_input(room, &HookId::Enter(room.clone()), 0);
        let v = serde_json::to_value(&input).unwrap();
        assert!(no_floats(&v), "{room}");
    }
    for id in pack.cast.keys() {
        let v = serde_json::to_value(world.actor_facts(id).unwrap()).unwrap();
        assert!(no_floats(&v), "{id}");
        assert!(no_floats(&world.look_view(id)), "{id}");
    }
}

fn no_floats(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Number(n) => n.is_i64() || n.is_u64(),
        serde_json::Value::Array(a) => a.iter().all(no_floats),
        serde_json::Value::Object(m) => m.values().all(no_floats),
        _ => true,
    }
}
