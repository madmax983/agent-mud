//! The worst known scripts stay cheap at the default limits.
//!
//! Release only: `cargo test --release -p mud-script --test cost`. It
//! guards rhai upgrades and limit changes against a slower worst case.
#![expect(clippy::unwrap_used, reason = "test helpers fail loudly")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use mud_script::{ScriptHost, ScriptSources};
use mud_world::hooks::{HookId, WorldDice};
use mud_world::{RoomId, World, WorldPack};

fn worst(body: &str) -> (Duration, String) {
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let manifest = "[[verbs]]\nname = \"probe\"\nusage = \"probe\"\nfile = \"probe.rhai\"\n";
    let mut s = ScriptSources {
        manifest: Some(manifest.into()),
        ..ScriptSources::default()
    };
    s.files.insert(
        "probe.rhai".into(),
        format!("fn on_probe(ctx) {{\n{body}\n}}\nfn pass(x) {{ 1 }}\n"),
    );
    let host = ScriptHost::from_sources(&s, &pack).unwrap();
    let world = World::new(Arc::clone(&pack), 1);
    let hook = HookId::Verb("probe".into());
    let input = world.probe_input(&RoomId::from("town_square"), &hook, 0);
    let c = host.current();
    let mut slowest = Duration::ZERO;
    let mut why = String::new();
    for _ in 0..5 {
        let t = Instant::now();
        let r = c.call_hook(&hook, &input, WorldDice::fixed([3]));
        slowest = slowest.max(t.elapsed());
        why = format!("{:?}", r.outcome.map(|(fx, _)| fx.len()));
    }
    (slowest, why)
}

/// The hostile loops. Rhai does not check the size of a map that grows in
/// place by index assignment (`m[k] = v`), so the last three grow maps to
/// the sizes that cost the most clones within the operation limit.
const HOSTILE: [&str; 10] = [
    "let m = #{}; let i = 0; while i < 600 { m[`k${i}`] = i; i += 1; } loop { let b = m; }",
    "let m = #{}; let i = 0; while i < 1000 { m[`k${i}`] = i; i += 1; } loop { let b = m; }",
    "let m = #{}; for i in 0..15 { m[`k${i}`] = \"x\"; } let a = []; for i in 0..15 { a.push(m); } loop { let b = a; }",
    "let a = []; for i in 0..63 { a.push(\"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"); } loop { let b = a; }",
    "let m = #{}; for i in 0..63 { m[`k${i}`] = i; } loop { let b = m; }",
    "let m = #{}; for i in 0..63 { m[`k${i}`] = i; } loop { pass(m); }",
    "let s = \"x\"; for i in 0..9 { s += s; } loop { let t = `${s}`; }",
    "let s = \"x\"; for i in 0..9 { s += s; } loop { let t = s + \"y\"; }",
    "let m = #{}; let i = 0; while i < 1600 { m[`k${i}`] = i; i += 1; } loop { let b = m; }",
    "let a = []; let i = 0; while i < 60 { a.push(i); i += 1; } loop { let n = 59 in a; }",
];

#[test]
#[cfg_attr(debug_assertions, ignore = "timings mean something only in release")]
fn the_worst_known_scripts_stay_under_25ms() {
    for body in HOSTILE {
        let (t, outcome) = worst(body);
        println!("{t:?} {outcome} {body}");
        assert!(t < Duration::from_millis(25), "{t:?}: {body}");
    }
}

#[test]
#[cfg_attr(debug_assertions, ignore = "timings mean something only in release")]
fn the_shipped_hooks_run_in_under_a_tenth_of_a_millisecond() {
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let host = ScriptHost::bundled_crypt_v01(&pack).unwrap();
    let world = World::new(Arc::clone(&pack), 1);
    let c = host.current();
    for hook in c.hooks.keys() {
        let room = match hook {
            HookId::Enter(r) | HookId::Speech(r) => r.clone(),
            _ => RoomId::from("ossuary_crypt"),
        };
        let input = world.probe_input(&room, hook, 0);
        let mut best = Duration::MAX;
        for _ in 0..20 {
            let t = Instant::now();
            let _ = c.call_hook(hook, &input, WorldDice::fixed([3]));
            best = best.min(t.elapsed());
        }
        println!("{best:?} {hook}");
        assert!(best < Duration::from_micros(100), "{best:?}: {hook}");
    }
}
