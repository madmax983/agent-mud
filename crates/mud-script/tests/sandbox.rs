//! The sandbox: what a script can and cannot do.
#![expect(clippy::unwrap_used, reason = "test helpers fail loudly")]

use std::sync::Arc;

use mud_script::{EventActor, MindPlan, MindWake, ScriptEvent, ScriptHost, ScriptSources, sandbox};
use mud_world::hooks::{HookId, HookReply, ScriptEffect, WorldDice};
use mud_world::{EntityId, RoomId, World, WorldPack};
use serde_json::json;

fn pack() -> Arc<WorldPack> {
    Arc::new(WorldPack::crypt_v01().unwrap())
}

/// A host whose verb `probe` runs `fn on_probe(ctx) { <body> }`, with
/// `extra` appended to the file.
fn host_with(body: &str, extra: &str) -> ScriptHost {
    let manifest = "[[verbs]]\nname = \"probe\"\nusage = \"probe\"\nfile = \"probe.rhai\"\nconfig = { n = 3 }\n";
    let mut s = ScriptSources {
        manifest: Some(manifest.into()),
        ..ScriptSources::default()
    };
    s.files.insert(
        "probe.rhai".into(),
        format!("fn on_probe(ctx) {{\n{body}\n}}\n{extra}"),
    );
    ScriptHost::from_sources(&s, &pack()).unwrap()
}

/// Runs the probe verb with `text` as the player's arguments.
fn run_with_text(body: &str, extra: &str, text: &str) -> HookReply {
    let host = host_with(body, extra);
    let world = World::new(pack(), 1);
    let hook = HookId::Verb("probe".into());
    let mut input = world.probe_input(&RoomId::from("town_square"), &hook, 1_700_000_000_000);
    input.text = Some(text.to_owned());
    host.current()
        .call_hook(&hook, &input, WorldDice::fixed([3, 4, 5, 6, 1, 2]))
}

fn run(body: &str) -> HookReply {
    run_with_text(body, "", "")
}

/// The fault message of a run that must fault.
fn fault(body: &str) -> String {
    match run(body).outcome {
        Err(e) => e,
        Ok((fx, _)) => panic!("{body:?} should fault, but returned {fx:?}"),
    }
}

/// The effects of a run that must succeed.
fn effects(body: &str) -> Vec<ScriptEffect> {
    match run(body).outcome {
        Ok((fx, _)) => fx,
        Err(e) => panic!("{body:?} faulted: {e}"),
    }
}

/// The one line a run says with `msg`.
fn said(expr: &str) -> String {
    match effects(&format!("{expr}\n[msg(said)]")).as_slice() {
        [ScriptEffect::Msg(t)] => t.clone(),
        other => panic!("{expr:?} returned {other:?}"),
    }
}

/// Everything a reply holds, as text.
fn everything(reply: &HookReply) -> String {
    format!(
        "{:?} {:?}",
        reply.outcome.as_ref().map(|(fx, _)| fx),
        reply.log
    )
}

/// The problems of a script file that must not compile.
fn refused(file: &str) -> Vec<String> {
    let manifest = "[[verbs]]\nname = \"probe\"\nusage = \"probe\"\nfile = \"probe.rhai\"\n";
    let mut s = ScriptSources {
        manifest: Some(manifest.into()),
        ..ScriptSources::default()
    };
    s.files.insert("probe.rhai".into(), file.into());
    match ScriptHost::from_sources(&s, &pack()) {
        Err(e) => e.problems(),
        Ok(_) => panic!("{file:?} should not compile"),
    }
}

fn mind_host(body: &str) -> ScriptHost {
    let manifest = "[minds.guard_01]\nfile = \"mind.rhai\"\n";
    let mut s = ScriptSources {
        manifest: Some(manifest.into()),
        ..ScriptSources::default()
    };
    s.files.insert(
        "mind.rhai".into(),
        format!("fn on_wake(ctx) {{\n{body}\n}}\n"),
    );
    ScriptHost::from_sources(&s, &pack()).unwrap()
}

fn wake(events: Vec<ScriptEvent>, booked: Vec<(u64, u64, String)>, seed: u64) -> MindWake {
    MindWake {
        entity: EntityId::from("guard_01"),
        priority: "room_batch",
        macro_tick: false,
        macro_ticks: 0,
        login: false,
        now_ms: 1_700_000_000_000,
        seed,
        me: json!({ "id": "guard_01" }),
        here: json!({ "room_id": "town_square" }),
        events,
        booked,
        toml_plan: vec!["say hi".into()],
        memory: json!({}),
    }
}

fn speech(seq: u64, text: &str) -> ScriptEvent {
    ScriptEvent {
        seq,
        kind: "speech".into(),
        verb: Some("say".into()),
        tags: Vec::new(),
        room: Some("town_square".into()),
        actor: Some(EventActor {
            id: EntityId::from("pc_ada"),
            name: "ada".into(),
            kind: "player".into(),
        }),
        target: None,
        is_self: false,
        at_me: false,
        text: Some(text.into()),
    }
}

#[test]
fn sleep_time_eval_print_and_imports_are_absent() {
    for call in [
        "sleep(1)",
        "timestamp()",
        "parse_json(\"1\")",
        "exit()",
        "\"a\".split(\"b\")",
        "\"ab\".index_of(\"b\")",
        "\"ab\".replace(\"a\", \"b\")",
    ] {
        let e = fault(&format!("{call}; []"));
        assert!(e.contains("Function not found"), "{call}: {e}");
    }
    for bad in ["eval(\"1\")", "print(1)", "debug(1)"] {
        let p = refused(&format!("fn on_probe(ctx) {{ {bad}; [] }}"));
        assert!(p[0].starts_with("probe.rhai:"), "{bad}: {p:?}");
    }
    let p = refused("import \"x\" as y;\nfn on_probe(ctx) { [] }");
    assert!(p[0].starts_with("probe.rhai:"), "{p:?}");
}

#[test]
fn floats_are_refused() {
    let p = refused("fn on_probe(ctx) { let x = 1.5; [] }");
    assert!(p[0].starts_with("probe.rhai:1:"), "{p:?}");
}

#[test]
fn the_sandbox_limits_are_compiled_in() {
    let e = fault("loop {}");
    assert!(e.contains("Too many operations"), "{e}");
    let e = fault("let i = 0; while true { i += 1; }");
    assert!(e.contains("Too many operations"), "{e}");
    let e = fault("let x = 9223372036854775807; x + 1");
    assert!(e.to_lowercase().contains("overflow"), "{e}");
}

#[test]
fn deep_recursion_hits_the_call_limit() {
    let r = run_with_text("f(0)", "fn f(n) { f(n + 1) }", "");
    let e = r.outcome.unwrap_err();
    assert!(e.contains("Stack overflow"), "{e}");
}

#[test]
fn string_array_and_map_growth_are_capped() {
    let e = fault("let s = \"ab\"; loop { s += s; }");
    assert!(e.contains("Length of string too large"), "{e}");
    let e = fault("let a = []; loop { a.push(1); }");
    assert!(
        e.contains("Size of array") || e.contains("too much work"),
        "{e}"
    );
    // Rhai checks the size of a map it builds or a function returns, not
    // of one that grows in place by index assignment. Every read of a
    // variable over the caps charges its size, so such growth runs out of
    // work long before it runs out of operations.
    let e = fault("let m = #{}; let i = 0; loop { m[`k${i}`] = i; i += 1; }");
    assert!(e.contains("the script did too much work"), "{e}");
    let e = fault("let m = #{}; for i in 0..70 { m[`k${i}`] = i; } let n = #{ a: m }; []");
    assert!(e.contains("Size of object map too large"), "{e}");
    let e = fault("let m = #{}; for i in 0..70 { m[`k${i}`] = i; } m.len(); []");
    assert!(e.contains("Size of object map too large"), "{e}");
}

/// A map grown in place to 300 keys, each holding a 60-element array.
const ARRAY_VALUED_MAP: &str = "let a = []; for i in 0..60 { a.push(i); } let m = #{}; let i = 0; while i < 300 { m[`${i}`] = a; i += 1; }";

#[test]
fn reading_a_variable_over_the_caps_charges_its_size() {
    // Copying the grown map, or passing it to a function, used to clone
    // about 18 000 values per operation without charging anything.
    for tail in [
        "loop { let b = m; }",
        "loop { pass(m); }",
        "for r in 0..700 { let b = m; } [msg(\"ok\")]",
        "m.len(); []",
    ] {
        let r = run_with_text(
            &format!("{ARRAY_VALUED_MAP} {tail}"),
            "fn pass(x) { 1 }",
            "",
        );
        let e = r.outcome.unwrap_err();
        assert!(e.contains("the script did too much work"), "{tail}: {e}");
    }
    // A method call binds `this` without a variable read, so `this` is
    // not part of the language.
    let p = refused("fn grow() { this.x = 1; }\nfn on_probe(ctx) { [] }");
    assert!(p[0].starts_with("probe.rhai:1:"), "{p:?}");
    // Values within the caps are read for free, however often.
    let fx = effects(
        "let m = #{}; for i in 0..60 { m[`k${i}`] = i; } for r in 0..2000 { let b = m; } [msg(\"ok\")]",
    );
    assert_eq!(fx, [ScriptEffect::Msg("ok".into())]);
}

#[test]
fn a_mind_ctx_over_the_caps_is_charged_but_usable() {
    // 32 events make a ctx far over the map cap; Hale-style reads of it
    // stay well inside the work budget.
    let host = mind_host(
        "let n = 0; for ev in ctx.events { if ev.kind == \"speech\" { n += 1; } } let m = ctx.memory; [`say ${n} ${ctx.now_ms > 0}`]",
    );
    let events = (0..32).map(|i| speech(i, "hello")).collect();
    let reply = host.current().on_wake(&wake(events, Vec::new(), 1));
    let plan: MindPlan = reply.outcome.unwrap();
    assert_eq!(plan.lines, ["say 32 true"]);
    // ...but copying it in a loop runs out of work.
    let host = mind_host("loop { let c = ctx; }");
    let events = (0..32).map(|i| speech(i, "hello")).collect();
    let e = host
        .current()
        .on_wake(&wake(events, Vec::new(), 1))
        .outcome
        .unwrap_err();
    assert!(e.contains("the script did too much work"), "{e}");
}

#[test]
fn aggregate_container_sizes_are_capped() {
    let e = fault("let m = #{}; for i in 0..40 { m[`k${i}`] = i; } let x = #{ a: m, b: m }; []");
    assert!(e.contains("too large"), "{e}");
}

#[test]
fn deep_expressions_fail_to_compile() {
    let deep = format!(
        "fn on_probe(ctx) {{ {}1{} }}",
        "(".repeat(70),
        ")".repeat(70)
    );
    let p = refused(&deep);
    assert!(p[0].starts_with("probe.rhai:"), "{p:?}");
}

#[test]
fn expensive_std_natives_are_absent() {
    for body in [
        "[1] == [1]",
        "#{a: 1} == #{a: 1}",
        "[1].filter(\"f\")",
        "[1].map(\"f\")",
        "[3, 1].sort()",
        "[3, 1].index_of(1)",
    ] {
        let e = fault(&format!("let x = {body}; []"));
        assert!(e.contains("Function not found"), "{body}: {e}");
    }
    let p = refused("fn on_probe(ctx) { let f = |x| x; [] }");
    assert!(!p.is_empty(), "closures are off");
}

#[test]
fn curated_natives_charge_work_and_work() {
    let e = fault("let b = []; for i in 0..60 { b.push(i); } let a = []; loop { a += b; a = []; }");
    assert!(e.contains("the script did too much work"), "{e}");
    assert_eq!(
        said(
            "let a = [1, 2]; a += [3]; let b = a + [4]; let said = `${b.len()} ${a.pop()} ${2 in a} ${9 in a}`;"
        ),
        "4 3 true false"
    );
    assert_eq!(
        said(
            "let m = #{ a: 1, b: 2 }; m.remove(\"a\"); let said = `${m.len()} ${\"b\" in m} ${m.keys().len()} ${m.is_empty()}`;"
        ),
        "1 true 1 false"
    );
    assert_eq!(
        said(
            "let t = \"  Hello World \".trim(); let said = `${t.len()} ${t.to_lower()} ${t.to_upper()} ${t.starts_with(\"He\")} ${t.ends_with(\"ld\")} ${t.contains(\"o W\")} ${t.sub_string(6, 5)}`;"
        ),
        "11 hello world HELLO WORLD true true true World"
    );
    assert_eq!(
        said(
            "let n = 0; for x in [1, 2, 3] { n += x; } for i in 0..3 { n += i; } let said = `${n} ${[].is_empty()} ${\"\".is_empty()}`;"
        ),
        "9 true true"
    );
    assert_eq!(
        said(
            "let said = `${hour_utc(1700000000000)} ${minute_of_day_utc(1700000000000)} ${day_utc(1700000000000)}`;"
        ),
        "22 1333 19675"
    );
}

#[test]
fn views_are_read_only_and_name_missing_keys() {
    let e = fault("ctx.room.lit = true; []");
    assert!(
        e.to_lowercase().contains("setter") || e.contains("writable"),
        "{e}"
    );
    let e = fault("let x = ctx.actor.nope; []");
    assert!(e.contains("no property \"nope\""), "{e}");
    assert_eq!(
        said(
            "let said = `${ctx.room.id} ${ctx.actor.kind} ${ctx.config.n} ${\"lit\" in ctx.room} ${ctx.room.exits.len()}`;"
        ),
        "town_square player 3 true 4"
    );
    assert_eq!(
        said(
            "let n = 0; for e in ctx.room.exits { n += 1; } for k in ctx.actor.attrs { n += 1; } let said = `${n}`;"
        ),
        "8"
    );
    assert_eq!(
        said(
            "let said = `${ctx.target == ()} ${ctx.item == ()} ${type_of(ctx.cast)} ${ctx.cast.vich.present}`;"
        ),
        "true true View true"
    );
}

#[test]
fn untrusted_text_never_escapes() {
    let secret = "XYZZY plan";
    let faults = [
        "let x = ctx.text + \"a\"; []",
        "let x = \"a\" + ctx.text; []",
        "let x = ctx.text == \"XYZZY plan\"; []",
        "let x = \"XYZZY plan\" != ctx.text; []",
        "let s = \"a\"; s += ctx.text; []",
        "log(ctx.text); []",
        "[msg(ctx.text)]",
        "[narrate(ctx.text)]",
        "[set_flag(\"a\", ctx.text)]",
        "[mark(\"a\", ctx.text)]",
        "let x = [ctx.text].to_string(); []",
    ];
    for body in faults {
        let r = run_with_text(body, "", secret);
        assert!(r.outcome.is_err(), "{body} must fault");
        assert!(
            !everything(&r).contains("XYZZY"),
            "{body}: {}",
            everything(&r)
        );
    }
    let r = run_with_text("[msg(`${ctx.text} ${ctx.text.to_string()}`)]", "", secret);
    assert!(!everything(&r).contains("XYZZY"), "{}", everything(&r));
    assert!(
        everything(&r).contains("[player text] [player text]"),
        "{}",
        everything(&r)
    );
    let r = run_with_text("[msg(ctx.text.pick([\"xyzzy\", \"plan\"]))]", "", secret);
    assert!(everything(&r).contains("xyzzy"), "the script's own word");
    assert!(!everything(&r).contains("XYZZY"));
    // A mind script: a line, memory or a note cannot carry it either.
    let host = mind_host("[ctx.events[0].text]");
    let r = host.on_wake(&wake(vec![speech(1, secret)], Vec::new(), 1));
    let e = r.outcome.unwrap_err();
    assert!(e.contains("line 0 is a Untrusted"), "{e}");
    let host = mind_host("#{ memory: #{ t: ctx.booked[0].note } }");
    let r = host.on_wake(&wake(Vec::new(), vec![(1, 2, secret.into())], 1));
    let e = r.outcome.unwrap_err();
    assert!(
        e.contains("memory may hold only plain data") && !e.contains("XYZZY"),
        "{e}"
    );
    let host = mind_host("[`say ${ctx.booked[0].note}`]");
    let r = host.on_wake(&wake(Vec::new(), vec![(1, 2, secret.into())], 1));
    assert_eq!(r.outcome.unwrap().lines, ["say [player text]"]);
}

#[test]
fn untrusted_probing_is_capped() {
    let probe = |n: usize| {
        (0..n)
            .map(|i| ["ctx.text.has_word(\"w", &format!("{i:02}"), "\");"].concat())
            .collect::<String>()
    };
    let e = run_with_text(&format!("{} []", probe(17)), "", "hello")
        .outcome
        .unwrap_err();
    assert!(
        e.contains("player text was probed more than 16 times in one call"),
        "{e}"
    );
    assert!(
        run_with_text(&format!("{} []", probe(16)), "", "hello")
            .outcome
            .is_ok()
    );
    let host = mind_host(
        "let n = 0; for ev in ctx.events { if ev.text.has_any([\"aa\", \"bb\", \"cc\", \"dd\"]) { n += 1; } } [`say ${n}`]",
    );
    let events = (0..10).map(|i| speech(i, "dd")).collect();
    let r = host.on_wake(&wake(events, Vec::new(), 1));
    assert_eq!(r.outcome.unwrap().lines, ["say 10"], "the cap is per text");
}

#[test]
fn search_words_are_dictionary_words() {
    for w in ["a", "ab c", "AB", &"a".repeat(33)] {
        let e = run_with_text(&format!("ctx.text.has_word(\"{w}\"); []"), "", "hello")
            .outcome
            .unwrap_err();
        assert!(
            e.contains("a search word is 2 to 32 lowercase letters"),
            "{w}: {e}"
        );
    }
    let e = run_with_text("ctx.text.has_any([1]); []", "", "hello")
        .outcome
        .unwrap_err();
    assert!(e.contains("a search word must be a string"), "{e}");
}

fn one_msg(reply: HookReply) -> String {
    match reply.outcome.unwrap().0.as_slice() {
        [ScriptEffect::Msg(t)] => t.clone(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn pick_returns_the_scripts_own_string() {
    let r = run_with_text(
        "let p = ctx.text.pick([\"fence\", \"vich\"]); [msg(`${p}`)]",
        "",
        "VICH! where's vich",
    );
    assert_eq!(one_msg(r), "vich");
    let r = run_with_text(
        "let p = ctx.text.pick([\"fence\"]); [msg(`${p == ()}`)]",
        "",
        "nothing here",
    );
    assert_eq!(one_msg(r), "true");
}

#[test]
fn has_word_matches_whole_words_only() {
    let body = "[msg(`${ctx.text.has_word(\"vich\")} ${ctx.text.has_word(\"vic\")} ${ctx.text.has_word(\"don't\")} ${ctx.text.is_empty()}`)]";
    assert_eq!(
        one_msg(run_with_text(body, "", "Vich? I don't know")),
        "true false true false"
    );
}

#[test]
fn check_clamps_huge_levels() {
    let t = said(
        "let a = check(9223372036854775807); let b = check(-9223372036854775807 - 1); let said = `${a.success} ${b.success} ${a.total}`;",
    );
    assert_eq!(t, "true false 12", "dice 3,4,5 then 6,1,2");
    assert_eq!(
        said("let r = check(10); let said = `${r.margin} ${r.critical}`;"),
        "-2 false"
    );
}

#[test]
fn script_dice_are_one_to_six() {
    let e = fault("[heal(\"7d\")]");
    assert!(
        e.contains("dice \"7d\": scripts use 1d to 6d with adds"),
        "{e}"
    );
    assert!(fault("[hurt(\"x\")]").contains("scripts use 1d to 6d"));
    assert!(fault("roll(\"0d\"); []").contains("scripts use 1d to 6d"));
    assert_eq!(
        said("let said = `${roll(\"2d+1\")} ${d6()} ${below(10)} ${chance(100)} ${chance(0)}`;"),
        "8 5 6 true false"
    );
    assert!(fault("below(0); []").contains("below(0)"));
    assert!(fault("chance(101); []").contains("chance(101)"));
}

#[test]
fn the_same_seed_gives_the_same_answer() {
    let host = mind_host("[`say ${d6()}${d6()}${d6()}${below(1000)}`]");
    let a = host
        .on_wake(&wake(Vec::new(), Vec::new(), 7))
        .outcome
        .unwrap();
    let b = host
        .on_wake(&wake(Vec::new(), Vec::new(), 7))
        .outcome
        .unwrap();
    let c = host
        .on_wake(&wake(Vec::new(), Vec::new(), 8))
        .outcome
        .unwrap();
    assert_eq!(a, b);
    assert_ne!(a, c);
    let (_, mut dice) = run("d6(); d6(); []").outcome.unwrap();
    assert_eq!(
        mud_rules::DiceRng::d6(&mut dice),
        5,
        "a hook's dice come back advanced by exactly its rolls"
    );
}

#[test]
fn a_panicking_native_is_a_fault() {
    let limits = mud_script::Limits::default();
    let mut engine = sandbox::engine(&limits);
    engine.register_fn("boom", || -> i64 { panic!("inside a native") });
    let ast = engine.compile("fn run_it(ctx) { boom() }").unwrap();
    let state = sandbox::CallState::new(sandbox::CallKind::Hook, WorldDice::fixed([1]), &limits);
    let out = sandbox::call(&engine, &ast, "run_it", rhai::Dynamic::UNIT, state);
    assert_eq!(out.result.unwrap_err(), "the script engine panicked");
}

#[test]
fn top_level_code_is_refused() {
    for file in [
        "fn on_probe(ctx) { [] } 1;",
        "fn on_probe(ctx) { [] } let x = 2;",
        "const A = 1; fn on_probe(ctx) { [] }",
    ] {
        assert_eq!(
            refused(file),
            ["probe.rhai: top-level code is not allowed; put it in a function"],
            "{file}"
        );
    }
}

#[test]
fn effects_are_for_hooks_and_returns_are_checked() {
    let host = mind_host("[msg(\"hi\")]");
    let e = host
        .on_wake(&wake(Vec::new(), Vec::new(), 1))
        .outcome
        .unwrap_err();
    assert!(
        e.contains("msg() is for room, item and verb scripts; a mind script returns command lines"),
        "{e}"
    );
    let e = fault("42");
    assert!(
        e.contains("verbs.probe returned a i64, not an array of effects"),
        "{e}"
    );
    let e = fault("[1]");
    assert!(e.contains("effect 0 is a i64, not an effect"), "{e}");
    assert_eq!(effects("()"), Vec::new());
    let fx = effects(
        "[msg(\"a\"), narrate(\"b\"), refuse(\"c\"), unlock(\"d\"), lock(\"n\"), set_flag(\"f\", 1), set_flag(\"g\", true), set_flag(\"h\", \"x\"), clear_flag(\"f\"), mark(\"m\", 2), unmark(\"m\"), heal(\"1d\"), hurt(\"2d-1\")]",
    );
    assert_eq!(fx.len(), 13);
    assert!(fault("[unlock(\"sideways\")]").contains("not a direction"));
    let r = run("log(\"one\"); for i in 0..20 { log(`l${i}`); } []");
    assert_eq!(r.log.len(), 17);
    assert_eq!(r.log[16], "…(5 more)");
}

#[test]
fn reward_builds_one_ledgered_ask_and_checks_its_shape() {
    use std::collections::BTreeMap;

    use mud_world::{PoolId, ProtoId};
    let fx = effects(
        "[reward(\"warden_bounty\", #{ currency: 20, items: #{ healing_draught: 2 } }), reward(\"tab\", 7), reward(\"tab\", #{})]",
    );
    assert_eq!(
        fx,
        [
            ScriptEffect::Reward {
                pool: PoolId::from("warden_bounty"),
                currency: 20,
                items: BTreeMap::from([(ProtoId::from("healing_draught"), 2)]),
            },
            ScriptEffect::Reward {
                pool: PoolId::from("tab"),
                currency: 7,
                items: BTreeMap::new(),
            },
            ScriptEffect::Reward {
                pool: PoolId::from("tab"),
                currency: 0,
                items: BTreeMap::new(),
            },
        ]
    );
    for (body, why) in [
        (
            "[reward(\"p\", #{ coins: 3 })]",
            "reward(): unknown key \"coins\"; use currency and items",
        ),
        (
            "[reward(\"p\", #{ currency: -1 })]",
            "reward(): currency is an integer of 0 or more",
        ),
        (
            "[reward(\"p\", -1)]",
            "reward(): currency is an integer of 0 or more",
        ),
        (
            "[reward(\"p\", #{ currency: \"lots\" })]",
            "reward(): currency is an integer of 0 or more",
        ),
        (
            "[reward(\"p\", #{ items: [1] })]",
            "reward(): items is a map of item id to count",
        ),
        (
            "[reward(\"p\", #{ items: #{ ale: 0 } })]",
            "reward(): items.ale is a count of 1 to 16",
        ),
        (
            "[reward(\"p\", #{ items: #{ ale: 17 } })]",
            "reward(): items.ale is a count of 1 to 16",
        ),
    ] {
        let e = fault(body);
        assert!(e.contains(why), "{body}: {e}");
    }
    // A hook reads what is left of each pool.
    assert_eq!(
        said(
            "let said = `${ctx.pools.warden_bounty.currency} ${ctx.pools.warden_bounty.per} ${ctx.pools.warden_bounty.items.healing_draught}`;"
        ),
        "60 day 3"
    );
    // A mind cannot mint.
    let e = mind_host("[reward(\"warden_bounty\", 5)]")
        .on_wake(&wake(Vec::new(), Vec::new(), 1))
        .outcome
        .unwrap_err();
    assert!(
        e.contains(
            "reward() is for room, item and verb scripts; a mind script returns command lines"
        ),
        "{e}"
    );
}

#[test]
fn use_up_builds_a_count_of_one_to_sixteen() {
    use mud_world::ProtoId;
    let fx = effects("[use_up(\"warden_blade\"), use_up(\"iron_ore\", 3), use_up(\"ale\", 16)]");
    assert_eq!(
        fx,
        [
            ScriptEffect::UseUp {
                proto: ProtoId::from("warden_blade"),
                count: 1,
            },
            ScriptEffect::UseUp {
                proto: ProtoId::from("iron_ore"),
                count: 3,
            },
            ScriptEffect::UseUp {
                proto: ProtoId::from("ale"),
                count: 16,
            },
        ]
    );
    for n in ["0", "-1", "17"] {
        let e = fault(&format!("[use_up(\"ale\", {n})]"));
        assert!(e.contains("use_up(): n is a count of 1 to 16"), "{n}: {e}");
    }
    // A mind cannot use things up.
    let e = mind_host("[use_up(\"ale\")]")
        .on_wake(&wake(Vec::new(), Vec::new(), 1))
        .outcome
        .unwrap_err();
    assert!(
        e.contains(
            "use_up() is for room, item and verb scripts; a mind script returns command lines"
        ),
        "{e}"
    );
}

#[test]
fn mind_returns_follow_the_contract() {
    let ok = |body: &str| {
        mind_host(body)
            .on_wake(&wake(Vec::new(), Vec::new(), 1))
            .outcome
    };
    assert_eq!(ok("()").unwrap(), MindPlan::default());
    assert_eq!(
        ok("[\"say hi\", \"pray\"]").unwrap_err(),
        "line 1 does not parse: unknown verb \"pray\"; type `help`"
    );
    assert_eq!(
        ok("#{ lines: [\"n\"], memory: #{ a: 1 }, wake_me: #{ minutes: 30, note: \"check\" } }")
            .unwrap(),
        MindPlan {
            lines: vec!["n".into()],
            memory: Some(json!({ "a": 1 })),
            wake_me: Some((30, "check".into())),
        }
    );
    assert!(
        ok("#{ lines: [], zz: 1 }")
            .unwrap_err()
            .contains("unknown key \"zz\"")
    );
    assert!(
        ok("#{ wake_me: #{ minutes: 0, note: \"x\" } }")
            .unwrap_err()
            .contains("minutes must be 1 to 10080")
    );
    assert!(
        ok("#{ wake_me: #{ minutes: 5 } }")
            .unwrap_err()
            .contains("note must be a string")
    );
    assert!(
        ok("[\"n\", \"n\", \"n\", \"n\", \"n\"]")
            .unwrap_err()
            .contains("more than 4 lines")
    );
    assert!(ok("42").unwrap_err().contains("on_wake returned a i64"));
    assert!(
        ok("#{ memory: 1 }")
            .unwrap_err()
            .contains("memory must be a map")
    );
    assert_eq!(ok("ctx.toml_plan").unwrap().lines, ["say hi"]);
    assert!(
        ok("[\"say a\\nb\"]")
            .unwrap_err()
            .contains("control or direction character")
    );
}
