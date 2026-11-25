//! littleguy wiring: the config, the socket protocol, and a real daemon
//! when `LG_BIN` names one.

use std::sync::Arc;

use mud_world::{Game, ManualClock, World, WorldPack};
use tokio::net::UnixListener;

use super::*;
use crate::gate::Gate;
use crate::router::Priority;

fn persona() -> Persona {
    Persona {
        name: "Torin".into(),
        persona: "A gruff smith.".into(),
        goal: "Get ore.".into(),
        body: "You regain 1 HP every 5 minutes while in the world.".into(),
        token: "secret-token".into(),
    }
}

#[test]
fn the_config_points_littleguy_at_the_mud() {
    let cfg = LittleguyConfig::default();
    let text = render_config(&cfg, &persona().text(), 4123, false);
    let v: toml::Table = toml::from_str(&text).unwrap();
    assert_eq!(v["owner"].as_str(), Some("mud"));
    assert!(v["system"].as_str().unwrap().contains("Get ore."));
    assert!(
        v["system"]
            .as_str()
            .unwrap()
            .contains("You regain 1 HP every 5 minutes"),
        "the pack's body rule, not one of the prompt's own"
    );
    // A littleguy agent books its own wakes: its prompt carries the habit.
    assert!(
        v["system"]
            .as_str()
            .unwrap()
            .ends_with(&littleguy_habits(ToolNames::LITTLEGUY, true)),
        "the wake habit"
    );
    assert!(v["system"].as_str().unwrap().contains("world__mud_wake_me"));
    let server = &v["mcp"]["server"][0];
    assert_eq!(server["url"].as_str(), Some("http://127.0.0.1:3000/mcp"));
    assert_eq!(server["bearer_env"].as_str(), Some(TOKEN_ENV));
    assert_eq!(server["labels"][0].as_str(), Some("untrusted_input"));
    assert_eq!(
        v["ui"]["bind"].as_str(),
        Some("127.0.0.1:4123"),
        "a real port: littleguy's sign-in link is built from it"
    );
    assert_eq!(v["tools"]["web_fetch"].as_bool(), Some(false));
    assert!(
        !text.contains("secret-token"),
        "the token never goes in a file"
    );
    assert_eq!(v["max_steps"].as_integer(), Some(64));
}

#[test]
fn zero_max_steps_leaves_a_wake_unbounded() {
    // littleguy bounds an op only when `max_steps` is set (ADR 0034 there).
    let cfg = LittleguyConfig {
        max_steps: 0,
        ..LittleguyConfig::default()
    };
    let v: toml::Table =
        toml::from_str(&render_config(&cfg, &persona().text(), 4123, false)).unwrap();
    assert!(!v.contains_key("max_steps"));
}

/// The default config of torin, parsed.
fn torin_config(quiet: bool) -> toml::Table {
    let text = render_config(&LittleguyConfig::default(), &persona().text(), 4123, quiet);
    toml::from_str(&text).unwrap()
}

#[test]
fn the_config_turns_on_notes_compaction_and_research() {
    let v = torin_config(false);
    assert_eq!(v["tools"]["files"].as_bool(), Some(true), "notes");
    assert_eq!(v["tools"]["plans"].as_bool(), Some(true));
    assert_eq!(v["tools"]["runs"].as_bool(), Some(false));
    assert_eq!(v["tools"]["web_search"].as_bool(), Some(false));
    assert_eq!(v["memory"]["enabled"].as_bool(), Some(true));
    assert_eq!(v["memory"]["reflect"].as_bool(), Some(true));
    assert_eq!(v["window"].as_integer(), Some(30));
    assert_eq!(v["max_request_bytes"].as_integer(), Some(240_000));
    assert_eq!(v["compaction"]["at_percent"].as_integer(), Some(80));
    assert_eq!(v["compaction"]["max_resets"].as_integer(), Some(3));
    assert_eq!(v["heartbeat"]["research"].as_bool(), Some(true));
    assert_eq!(v["heartbeat"]["every_minutes"].as_integer(), Some(240));
    assert_eq!(
        v["heartbeat"]["prompt"].as_str(),
        Some(heartbeat_prompt(ToolNames::LITTLEGUY, true).as_str())
    );
    assert_eq!(v["flow"]["rule_of_two"].as_bool(), Some(true));
    // Each switch is the operator's.
    let off = LittleguyConfig {
        notes: false,
        research: false,
        compaction: None,
        max_request_bytes: 0,
        window: 12,
        ..LittleguyConfig::default()
    };
    let v: toml::Table =
        toml::from_str(&render_config(&off, &persona().text(), 4123, false)).unwrap();
    assert_eq!(v["tools"]["files"].as_bool(), Some(false));
    assert_eq!(v["heartbeat"]["research"].as_bool(), Some(false));
    assert!(!v.contains_key("compaction") && !v.contains_key("max_request_bytes"));
    assert_eq!(v["window"].as_integer(), Some(12));
}

#[test]
fn the_mud_server_is_split_by_effect() {
    let v = torin_config(false);
    let servers = v["mcp"]["server"].as_array().unwrap();
    assert_eq!(servers.len(), 2);
    let tools = |s: &toml::Value| -> Vec<String> {
        s["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap().to_owned())
            .collect()
    };
    let (world, mud) = (&servers[0], &servers[1]);
    assert_eq!(world["id"].as_str(), Some("world"));
    assert_eq!(world["effect"].as_str(), Some("internal"));
    assert_eq!(
        tools(world),
        ["mud_look", "mud_sheet", "mud_wake_me", "mud_cancel_wake"]
    );
    assert_eq!(mud["id"].as_str(), Some("mud"));
    assert_eq!(mud["effect"].as_str(), Some("write"));
    assert_eq!(tools(mud), ["mud_events", "mud_command"]);
    for s in servers {
        assert_eq!(s["url"].as_str(), Some("http://127.0.0.1:3000/mcp"));
        assert_eq!(s["bearer_env"].as_str(), Some(TOKEN_ENV));
        assert_eq!(s["labels"][0].as_str(), Some("untrusted_input"));
    }
    let text = render_config(&LittleguyConfig::default(), &persona().text(), 4123, false);
    assert!(
        !text.contains("secret-token"),
        "the token never goes in a file"
    );
}

/// The `<server>__<tool>` names in `text`.
fn served_names(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_ascii_lowercase() || c == '_'))
        .filter(|w| w.contains("__"))
        .map(str::to_owned)
        .collect()
}

/// lg's note tools, which it serves only with `tools.files` and `[memory]
/// enabled` (lg `daemon/reload.rs`, `own_tools`).
const NOTE_TOOLS: [&str; 4] = ["write_note", "read_note", "list_notes", "delete_note"];

#[test]
fn every_tool_named_in_the_prompts_exists_under_that_name() {
    let notes_off = LittleguyConfig {
        notes: false,
        ..LittleguyConfig::default()
    };
    // Loud, notes off, and quiet (memory off): no prompt names a note
    // tool that lg does not serve.
    for (cfg, quiet) in [
        (LittleguyConfig::default(), false),
        (notes_off, false),
        (LittleguyConfig::default(), true),
    ] {
        let v: toml::Table =
            toml::from_str(&render_config(&cfg, &persona().text(), 4123, quiet)).unwrap();
        let notes_served = v["tools"]["files"].as_bool() == Some(true)
            && v["memory"]["enabled"].as_bool() == Some(true);
        assert_eq!(notes_served, cfg.notes && !quiet);
        let system = v["system"].as_str().unwrap();
        let tick = v
            .get("heartbeat")
            .map_or("", |h| h["prompt"].as_str().unwrap());
        assert_eq!(tick.is_empty(), quiet, "a quiet config has no tick");
        for text in [system, tick] {
            for tool in NOTE_TOOLS {
                assert!(
                    notes_served || !text.contains(tool),
                    "{tool} is named but not served (notes {}, quiet {quiet}): {text}",
                    cfg.notes
                );
            }
        }
        if notes_served {
            assert!(system.contains("write_note") && tick.contains("write_note"));
        }
    }
    let v = torin_config(false);
    let mut served = Vec::new();
    for s in v["mcp"]["server"].as_array().unwrap() {
        let id = s["id"].as_str().unwrap();
        for t in s["tools"].as_array().unwrap() {
            served.push(format!("{id}__{}", t.as_str().unwrap()));
        }
    }
    let system = v["system"].as_str().unwrap();
    let tick = v["heartbeat"]["prompt"].as_str().unwrap();
    let mut named = served_names(system);
    named.extend(served_names(tick));
    assert_ne!(named.len(), 0);
    for n in &named {
        assert!(
            served.contains(n),
            "{n} is named but not served: {served:?}"
        );
    }
    for n in ToolNames::LITTLEGUY.all() {
        assert!(named.iter().any(|m| m == n), "{n} is never named");
        assert!(served.iter().any(|m| m == n), "{n} is not served");
    }
    for text in [system, tick] {
        let rest = text.replace("mud__mud_command", "");
        assert!(!rest.contains("mud_command"), "a bare mud_command: {text}");
    }
}

#[test]
fn the_config_asks_for_no_owner_gated_feature() {
    for quiet in [false, true] {
        let v = torin_config(quiet);
        for key in [
            "council",
            "skills",
            "sandbox",
            "approvals",
            "scripts",
            "patches",
            "research",
            "hooks",
            "followups",
            "allow_actions",
        ] {
            assert!(!v.contains_key(key), "[{key}] is never written");
        }
        assert!(!v["tools"].as_table().unwrap().contains_key("allow_actions"));
    }
}

#[test]
fn refine_is_written_only_when_asked() {
    let render = |refine| -> toml::Table {
        let cfg = LittleguyConfig {
            refine,
            ..LittleguyConfig::default()
        };
        toml::from_str(&render_config(&cfg, &persona().text(), 4123, false)).unwrap()
    };
    assert!(!render(RefineMode::Off).contains_key("refine"));
    for mode in [RefineMode::Menu, RefineMode::Free] {
        let v = render(mode);
        assert_eq!(v["refine"]["messages"].as_integer(), Some(30), "{mode:?}");
        assert_eq!(v["refine"]["dimension"].as_str(), Some("inv"), "{mode:?}");
        assert_eq!(v["refine"].as_table().unwrap().len(), 2, "{mode:?}");
    }
    assert_eq!(
        LittleguyConfig::default().refine,
        RefineMode::Menu,
        "the code default"
    );
    // A quiet config keeps it: quiet only stops ticks and memory.
    let quiet: toml::Table = toml::from_str(&render_config(
        &LittleguyConfig::default(),
        &persona().text(),
        4123,
        true,
    ))
    .unwrap();
    assert!(quiet.contains_key("refine"));
    assert!(!RefineMode::Off.is_on() && RefineMode::Menu.is_on() && RefineMode::Free.is_on());
}

#[tokio::test]
async fn a_quiet_config_drops_the_heartbeat_and_memory_and_keeps_the_port() {
    let quiet = torin_config(true);
    assert!(!quiet.contains_key("heartbeat"), "no tick while quiet");
    assert_eq!(quiet["memory"]["enabled"].as_bool(), Some(false));
    let loud = torin_config(false);
    // lg serves no note tool without memory, so the quiet system text
    // names none; the rest of it stays.
    let quiet_system = quiet["system"].as_str().unwrap();
    assert!(!quiet_system.contains("write_note"), "{quiet_system}");
    assert!(loud["system"].as_str().unwrap().contains("write_note"));
    assert!(
        quiet_system.ends_with(&littleguy_habits(ToolNames::LITTLEGUY, false)),
        "{quiet_system}"
    );
    for key in ["tools", "mcp", "ui", "compaction", "flow", "window"] {
        assert_eq!(quiet[key], loud[key], "{key} stays");
    }
    // On a mind, a rewrite keeps the daemon's pages port.
    let dir = tempfile::tempdir().unwrap();
    let cfg = LittleguyConfig {
        data_dir: dir.path().to_path_buf(),
        ..LittleguyConfig::default()
    };
    let torin = EntityId::from("torin");
    let mind = LittleguyMind::new(cfg, HashMap::from([(torin.clone(), persona())]));
    assert!(!mind.is_quiet(&torin));
    // Before any start, the flag waits for the first render.
    mind.set_quiet(&torin, true).await.unwrap();
    assert!(mind.is_quiet(&torin));
    assert!(
        !mind.config_path(&torin).exists(),
        "nothing runs: nothing written"
    );
    std::fs::create_dir_all(mind.entity_dir(&torin)).unwrap();
    let sock = dir.path().join("fake.sock");
    mind.daemons.lock().await.insert(
        torin.clone(),
        Daemon {
            child: stand_in(&sock).await,
            socket: sock.clone(),
            ui_port: 4999,
            approvals: ApprovalCheck::Off,
        },
    );
    let read = || -> toml::Table {
        toml::from_str(&std::fs::read_to_string(mind.config_path(&torin)).unwrap()).unwrap()
    };
    mind.set_quiet(&torin, true).await.unwrap();
    let v = read();
    assert!(!v.contains_key("heartbeat"));
    assert_eq!(v["ui"]["bind"].as_str(), Some("127.0.0.1:4999"));
    mind.set_quiet(&torin, false).await.unwrap();
    let v = read();
    assert!(v.contains_key("heartbeat"));
    assert_eq!(v["memory"]["enabled"].as_bool(), Some(true));
    assert_eq!(
        v["ui"]["bind"].as_str(),
        Some("127.0.0.1:4999"),
        "the port stays"
    );
    assert!(!mind.is_quiet(&torin));
    assert!(
        mind.set_quiet(&EntityId::from("nobody"), true)
            .await
            .is_err()
    );
    assert_eq!(mind.brief(&torin).unwrap().goal, "Get ore.");
    assert!(mind.brief(&EntityId::from("nobody")).is_none());
    mind.shutdown().await;
}

#[test]
fn config_writes_are_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lg.toml");
    let loud = render_config(&LittleguyConfig::default(), &persona().text(), 4123, false);
    let quiet = render_config(&LittleguyConfig::default(), &persona().text(), 4123, true);
    write_config(&path, &loud).unwrap();
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let (path, done) = (path.clone(), Arc::clone(&done));
        let (loud, quiet) = (loud.clone(), quiet.clone());
        std::thread::spawn(move || {
            let mut reads = 0_u32;
            while !done.load(std::sync::atomic::Ordering::SeqCst) {
                let text = std::fs::read_to_string(&path).unwrap();
                assert!(
                    text == loud || text == quiet,
                    "a partial file: {} bytes",
                    text.len()
                );
                toml::from_str::<toml::Table>(&text).unwrap();
                reads += 1;
            }
            reads
        })
    };
    for i in 0..200 {
        write_config(&path, if i % 2 == 0 { &quiet } else { &loud }).unwrap();
    }
    done.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(reader.join().unwrap() > 0);
    let left: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(left, ["lg.toml"], "no temp file stays");
}

#[test]
fn a_long_wake_line_stays_under_lgs_cap() {
    let quotes = "\"".repeat(500);
    let views: Vec<Value> = (0..300_u64)
        .map(|seq| json!({ "seq": seq, "kind": "say", "who": "Ada", "untrusted_text": quotes }))
        .collect();
    let ctx = WakeContext {
        entity: EntityId::from("torin"),
        name: "Torin".into(),
        priority: Priority::Booked,
        reason: "booked wake".into(),
        events: Vec::new(),
        event_views: views,
        look: json!({ "title": "The Forge" }),
        you: serde_json::Value::Null,
        macro_tick: false,
        macro_ticks: 0,
        login: false,
        wake_id: "w".repeat(200),
        now_ms: NOW,
        goals: Vec::new(),
        steward: Vec::new(),
        booked: (1..=5)
            .map(|id| crate::wakes::Booking {
                id,
                due_ms: NOW,
                note: "\"".repeat(280),
            })
            .collect(),
    };
    let text = wake_message(ToolNames::LITTLEGUY, &ctx);
    let mut line =
        serde_json::to_string(&json!({ "kind": "chat", "message_id": ctx.wake_id, "text": text }))
            .unwrap();
    line.push('\n');
    assert!(line.len() < 65_536, "{} bytes on the wire", line.len());
    assert!(line.len() <= LG_MAX_LINE_BYTES);
    assert!(serde_json::to_string(&text).unwrap().len() <= crate::mind::WAKE_TEXT_WIRE_BYTES);
    let events_line = text.lines().find(|l| l.starts_with("events: ")).unwrap();
    let shown: Vec<Value> =
        serde_json::from_str(&events_line["events: ".len()..]).expect("every event shown is whole");
    assert!(!shown.is_empty() && shown.len() < 300, "{}", shown.len());
    let seqs: Vec<u64> = shown.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    let first = 300 - u64::try_from(shown.len()).unwrap();
    assert_eq!(
        seqs,
        (first..300).collect::<Vec<_>>(),
        "the newest, in order"
    );
    for e in &shown {
        assert_eq!(
            e["untrusted_text"].as_str().unwrap().len(),
            500,
            "none is cut"
        );
    }
    let omitted = 300 - shown.len();
    assert!(
        text.contains(&format!(
            "\n{omitted} older events are not shown; read them with mud__mud_events.\n"
        )),
        "{}",
        &text[text.len() - 300..]
    );
    assert!(text.contains("booked: [{\"id\":1,"), "the notes stay");
    // A short wake shows everything and no omission line.
    let mut short = ctx;
    short.event_views.truncate(3);
    let m = wake_message(ToolNames::LITTLEGUY, &short);
    assert!(!m.contains("not shown"), "{m}");
}

#[tokio::test]
async fn chat_refuses_an_oversized_line_before_sending() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("lg.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    // 33 000 quotes take 66 000 bytes once escaped.
    let text = "\"".repeat(33_000);
    let e = chat(&sock, "wake-torin-1", &text).await.unwrap_err();
    assert!(e.to_string().contains("longer than lg accepts"), "{e}");
    assert!(
        tokio::time::timeout(Duration::from_millis(200), listener.accept())
            .await
            .is_err(),
        "no connection, so no byte"
    );
    // Just under the cap goes out.
    let sock = dir.path().join("lg2.sock");
    let server = fake_daemon(&sock, r#"{"kind":"reply","text":"ok"}"#);
    let fits = "x".repeat(LG_MAX_LINE_BYTES - 100);
    assert!(chat(&sock, "w", &fits).await.is_ok());
    let req = server.await.unwrap();
    assert!(req.len() <= LG_MAX_LINE_BYTES);
}

fn fake_daemon(path: &Path, answer: &'static str) -> tokio::task::JoinHandle<String> {
    let listener = UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut req = String::new();
        reader.read_line(&mut req).await.unwrap();
        let mut stream = reader.into_inner();
        stream
            .write_all(b"{\"kind\":\"delta\",\"text\":\"pre\"}\n")
            .await
            .unwrap();
        stream.write_all(answer.as_bytes()).await.unwrap();
        stream.write_all(b"\n").await.unwrap();
        req
    })
}

#[tokio::test]
async fn chat_speaks_the_socket_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("lg.sock");
    let server = fake_daemon(
        &sock,
        r#"{"kind":"reply","text":"posted the bounty","usage":{"calls":2,"input_tokens":900,"output_tokens":40,"cost_usd_micros":12,"unpriced_calls":0}}"#,
    );
    let r = chat(&sock, "wake-torin-1", "hello").await.unwrap();
    assert_eq!(r.text.as_deref(), Some("posted the bounty"));
    assert_eq!(r.usage.tokens(), 940);
    assert_eq!(r.usage.calls, 2);
    let req: Value = serde_json::from_str(&server.await.unwrap()).unwrap();
    assert_eq!(req["kind"], "chat");
    assert_eq!(req["message_id"], "wake-torin-1");
}

#[tokio::test]
async fn ui_link_returns_a_loopback_sign_in_link() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("lg.sock");
    let _s = fake_daemon_raw(
        &sock,
        r#"{"kind":"connect_link","link":"http://127.0.0.1:4123/login?token=abc"}"#,
    );
    assert_eq!(
        ui_link(&sock).await.unwrap(),
        "http://127.0.0.1:4123/login?token=abc"
    );
    let sock = dir.path().join("evil.sock");
    let _s = fake_daemon_raw(
        &sock,
        r#"{"kind":"connect_link","link":"https://evil.example/login"}"#,
    );
    assert!(
        ui_link(&sock).await.is_err(),
        "never a link off this machine"
    );
    let sock = dir.path().join("off.sock");
    let _s = fake_daemon_raw(
        &sock,
        r#"{"kind":"error","message":"the web pages do not serve in this daemon"}"#,
    );
    let e = ui_link(&sock).await.unwrap_err();
    assert!(e.to_string().contains("do not serve"), "{e}");
}

/// `ui_link` is a loop job in lg: it answers after the chat or tick that
/// runs ahead of it, so it waits as long as a wake may run.
#[tokio::test(start_paused = true)]
async fn ui_link_waits_behind_a_running_wake() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("lg.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut req = String::new();
        reader.read_line(&mut req).await.unwrap();
        // A wake chat of 100 s runs ahead of the link.
        tokio::time::sleep(Duration::from_secs(100)).await;
        let mut stream = reader.into_inner();
        stream
            .write_all(
                b"{\"kind\":\"connect_link\",\"link\":\"http://127.0.0.1:4123/login?token=abc\"}\n",
            )
            .await
            .unwrap();
        req
    });
    let link = ui_link(&sock).await;
    assert_eq!(
        link.unwrap(),
        "http://127.0.0.1:4123/login?token=abc",
        "the link comes after the wake"
    );
    assert_eq!(server.await.unwrap().trim(), r#"{"kind":"ui_link"}"#);
}

/// A daemon set quiet before its first start starts quiet: `ensure`
/// writes lg.toml before it spawns, so a missing binary still leaves it.
#[tokio::test]
async fn a_daemon_set_quiet_before_its_start_starts_quiet() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = LittleguyConfig {
        bin: dir.path().join("no-such-lg"),
        data_dir: dir.path().to_path_buf(),
        ..LittleguyConfig::default()
    };
    let torin = EntityId::from("torin");
    let mind = LittleguyMind::new(cfg, HashMap::from([(torin.clone(), persona())]));
    mind.set_quiet(&torin, true).await.unwrap();
    assert!(mind.ensure(&torin).await.is_err(), "no binary");
    let v: toml::Table =
        toml::from_str(&std::fs::read_to_string(mind.config_path(&torin)).unwrap()).unwrap();
    assert!(!v.contains_key("heartbeat"), "no tick while quiet");
    assert_eq!(v["memory"]["enabled"].as_bool(), Some(false));
    assert!(!v["system"].as_str().unwrap().contains("write_note"));
    // Loud again: the next start renders loud.
    mind.set_quiet(&torin, false).await.unwrap();
    assert!(mind.ensure(&torin).await.is_err());
    let v: toml::Table =
        toml::from_str(&std::fs::read_to_string(mind.config_path(&torin)).unwrap()).unwrap();
    assert!(v.contains_key("heartbeat"));
    assert_eq!(v["memory"]["enabled"].as_bool(), Some(true));
}

/// Each start asks the new daemon `approvals` once and keeps what it
/// answered, so a daemon with approvals on is seen.
#[tokio::test]
async fn each_start_checks_that_approvals_are_off() {
    let dir = tempfile::tempdir().unwrap();
    // A stand-in `lg` that only runs; the test serves its socket.
    let bin = dir.path().join("fake-lg");
    std::fs::write(&bin, "#!/bin/sh\nexec sleep 30\n").unwrap();
    let mut perms = std::fs::metadata(&bin).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&bin, perms).unwrap();
    let cfg = LittleguyConfig {
        bin,
        data_dir: dir.path().to_path_buf(),
        ..LittleguyConfig::default()
    };
    let torin = EntityId::from("torin");
    let mind = Arc::new(LittleguyMind::new(
        cfg,
        HashMap::from([(torin.clone(), persona())]),
    ));
    assert_eq!(mind.approval_check(&torin).await, None, "not started");
    let start = tokio::spawn({
        let (mind, torin) = (Arc::clone(&mind), torin.clone());
        async move { mind.ensure(&torin).await }
    });
    // Once the process runs, open its socket.
    let pid = mind.entity_dir(&torin).join(PID_FILE);
    while !pid.exists() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let listener = UnixListener::bind(mind.entity_dir(&torin).join("lg.sock")).unwrap();
    let (stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("the start asks approvals")
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut req = String::new();
    reader.read_line(&mut req).await.unwrap();
    assert_eq!(req.trim(), r#"{"kind":"approvals"}"#);
    reader
        .into_inner()
        .write_all(
            b"{\"kind\":\"approvals\",\"entries\":[{\"id\":3,\"tool\":\"mud__mud_command\",\"text\":\"say hi\"}]}\n",
        )
        .await
        .unwrap();
    start.await.unwrap().unwrap();
    assert_eq!(
        mind.approval_check(&torin).await,
        Some(ApprovalCheck::On { waiting: 1 })
    );
    mind.shutdown().await;
    assert_eq!(mind.approval_check(&torin).await, None, "stopped");
}

#[tokio::test]
async fn the_approvals_check_reads_off_on_and_no_answer() {
    let dir = tempfile::tempdir().unwrap();
    let torin = EntityId::from("torin");
    let answer = |name: &str, line: &'static str| {
        let sock = dir.path().join(name);
        let listener = UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut req = String::new();
            reader.read_line(&mut req).await.unwrap();
            let mut stream = reader.into_inner();
            stream.write_all(line.as_bytes()).await.unwrap();
            stream.write_all(b"\n").await.unwrap();
        });
        sock
    };
    let off = answer(
        "off.sock",
        r#"{"kind":"off","message":"approvals are off"}"#,
    );
    assert_eq!(check_approvals(&torin, &off).await, ApprovalCheck::Off);
    let on = answer("on.sock", r#"{"kind":"approvals","entries":[]}"#);
    assert_eq!(
        check_approvals(&torin, &on).await,
        ApprovalCheck::On { waiting: 0 },
        "on, even with nothing waiting yet"
    );
    let odd = answer("odd.sock", r#"{"kind":"error","message":"no"}"#);
    assert!(matches!(
        check_approvals(&torin, &odd).await,
        ApprovalCheck::Unknown(m) if m.contains("no")
    ));
    assert!(matches!(
        check_approvals(&torin, &dir.path().join("none.sock")).await,
        ApprovalCheck::Unknown(_)
    ));
}

#[tokio::test]
async fn a_daemon_that_is_not_running_has_no_link() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = LittleguyConfig {
        data_dir: dir.path().to_path_buf(),
        ..LittleguyConfig::default()
    };
    let torin = EntityId::from("torin");
    let mind = LittleguyMind::new(cfg, HashMap::from([(torin.clone(), persona())]));
    assert!(mind.serves(&torin));
    assert!(!mind.serves(&EntityId::from("marta")));
    let e = mind.ui_link(&torin).await.unwrap_err();
    assert!(e.to_string().contains("not running yet"), "{e}");
}

/// A daemon that answers one request with `answer`, and nothing else.
fn fake_daemon_raw(path: &Path, answer: &'static str) -> tokio::task::JoinHandle<()> {
    let listener = UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut req = String::new();
        reader.read_line(&mut req).await.unwrap();
        assert_eq!(req.trim(), r#"{"kind":"ui_link"}"#);
        let mut stream = reader.into_inner();
        stream.write_all(answer.as_bytes()).await.unwrap();
        stream.write_all(b"\n").await.unwrap();
    })
}

#[tokio::test]
async fn waiting_for_an_approval_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("lg.sock");
    let _s = fake_daemon(&sock, r#"{"kind":"waiting","approval":7}"#);
    let e = chat(&sock, "w", "x").await.unwrap_err();
    assert!(e.to_string().contains("approval 7"));
    assert!(chat(&dir.path().join("none.sock"), "w", "x").await.is_err());
}

#[tokio::test]
async fn a_reply_after_the_wake_was_given_up_is_still_charged() {
    // Regression: the daemon kept spending after a wake timed out, but its
    // reply (and the usage in it) went to a dropped socket.
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("lg.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut req = String::new();
        reader.read_line(&mut req).await.unwrap();
        go_rx.await.unwrap();
        let mut stream = reader.into_inner();
        // The peer may be gone; the write must not panic the daemon.
        let _ = stream
            .write_all(b"{\"kind\":\"reply\",\"text\":\"late\",\"usage\":{\"calls\":3,\"input_tokens\":5000,\"output_tokens\":200,\"cost_usd_micros\":40}}\n")
            .await;
    });
    let torin = EntityId::from("torin");
    let sink = Arc::new(UsageSink::default());
    let cut = tokio::time::timeout(
        Duration::from_millis(100),
        chat_charged(
            sock.clone(),
            "wake-torin-9".into(),
            "hello".into(),
            torin.clone(),
            Arc::clone(&sink),
            no_ledger(&dir),
        ),
    )
    .await;
    assert!(cut.is_err(), "the wake timed out");
    go_tx.send(()).unwrap();
    server.await.unwrap();
    let mut late = Usage::default();
    for _ in 0..100 {
        late = sink.take(&torin);
        if !late.is_zero() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(late.tokens(), 5_200);
    assert_eq!(late.calls, 3);
    assert!(
        sink.take_answered_late(&torin),
        "the late reply is an answer: the wake's close drops what it read"
    );
    assert!(!sink.take_answered_late(&torin), "taken once");
}

#[tokio::test]
async fn a_late_no_answer_or_error_is_not_an_answer() {
    // The op read its events, then the model failed: the wake that gave up
    // on it still owes them.
    for answer in [
        r#"{"kind":"no_answer","reason":"provider error"}"#,
        r#"{"kind":"waiting","approval":4}"#,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("lg.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut req = String::new();
            reader.read_line(&mut req).await.unwrap();
            go_rx.await.unwrap();
            let mut stream = reader.into_inner();
            let _ = stream.write_all(format!("{answer}\n").as_bytes()).await;
        });
        let torin = EntityId::from("torin");
        let sink = Arc::new(UsageSink::default());
        let in_flight = InFlight::default();
        let charge = Charge {
            until_settled: Some(Arc::new(in_flight.start(&torin))),
            ..no_ledger(&dir)
        };
        let cut = tokio::time::timeout(
            Duration::from_millis(100),
            chat_charged(
                sock.clone(),
                "wake-torin-9".into(),
                "hello".into(),
                torin.clone(),
                Arc::clone(&sink),
                charge,
            ),
        )
        .await;
        assert!(cut.is_err(), "the wake timed out");
        go_tx.send(()).unwrap();
        server.await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), in_flight.settled(&torin))
            .await
            .expect("the op was read");
        assert!(!sink.take_answered_late(&torin), "{answer}");
    }
}

#[tokio::test]
async fn a_reply_in_time_is_not_charged_twice() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("lg.sock");
    let _server = fake_daemon(
        &sock,
        r#"{"kind":"reply","text":"ok","usage":{"calls":1,"input_tokens":10,"output_tokens":2,"cost_usd_micros":0}}"#,
    );
    let torin = EntityId::from("torin");
    let sink = Arc::new(UsageSink::default());
    let r = chat_charged(
        sock,
        "w".into(),
        "x".into(),
        torin.clone(),
        Arc::clone(&sink),
        no_ledger(&dir),
    )
    .await
    .unwrap();
    assert_eq!(r.usage.tokens(), 12);
    assert!(!r.unanswered);
    assert!(sink.take(&torin).is_zero());
    assert!(
        !sink.take_answered_late(&torin),
        "an answer in time is in the reply"
    );
}

/// A charge with no ledger file: the daemon's reported usage stands.
fn no_ledger(dir: &tempfile::TempDir) -> Charge {
    Charge {
        meter: Arc::new(LedgerMeter::new(dir.path().join("no-ledger.db"))),
        now_ms: NOW,
        until_settled: None,
    }
}

/// A charge read from a ledger in `dir`, with `tokens` already spent.
fn ledger_with(dir: &tempfile::TempDir, tokens: &[i64]) -> Charge {
    let (path, conn) = crate::ledger::fake::ledger(dir.path());
    for (i, t) in tokens.iter().enumerate() {
        crate::ledger::fake::call(&conn, i64::try_from(i).unwrap(), *t, 0, None, NOW - 1);
    }
    Charge {
        meter: Arc::new(LedgerMeter::new(path)),
        now_ms: NOW,
        until_settled: None,
    }
}

/// Midday, some day.
const NOW: u64 = 20_000 * 86_400_000 + 43_200_000;

#[tokio::test]
async fn a_no_answer_is_charged_from_the_ledger() {
    // Regression: "I used every tool round" came back as a NoAnswer with
    // no usage, so every token of the op went uncharged.
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("lg.sock");
    let _s = fake_daemon(
        &sock,
        r#"{"kind":"no_answer","reason":"I used every tool round that one message gets. Ask me to go on"}"#,
    );
    let torin = EntityId::from("torin");
    let sink = Arc::new(UsageSink::default());
    let charge = ledger_with(&dir, &[3_000, 4_000]);
    let r = chat_charged(
        sock,
        "w".into(),
        "x".into(),
        torin.clone(),
        Arc::clone(&sink),
        charge,
    )
    .await
    .unwrap();
    assert!(r.unanswered, "a no_answer is not an answer");
    assert!(r.text.unwrap().contains("every tool round"));
    assert_eq!(r.usage.tokens(), 7_000);
    assert_eq!(r.usage.calls, 2);
    assert!(sink.take(&torin).is_zero(), "charged once, in the reply");
}

#[tokio::test]
async fn the_ledger_replaces_reported_usage_so_nothing_is_charged_twice() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("lg.sock");
    let _s = fake_daemon(
        &sock,
        r#"{"kind":"reply","text":"ok","usage":{"calls":1,"input_tokens":10,"output_tokens":2,"cost_usd_micros":0}}"#,
    );
    let torin = EntityId::from("torin");
    let sink = Arc::new(UsageSink::default());
    let r = chat_charged(
        sock,
        "w".into(),
        "x".into(),
        torin.clone(),
        Arc::clone(&sink),
        ledger_with(&dir, &[12, 500]),
    )
    .await
    .unwrap();
    assert_eq!(r.usage.tokens(), 512, "the ledger has the whole op");
    assert!(sink.take(&torin).is_zero());
}

#[tokio::test]
async fn a_wake_that_waits_for_approval_still_pays() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("lg.sock");
    let _s = fake_daemon(&sock, r#"{"kind":"waiting","approval":3}"#);
    let torin = EntityId::from("torin");
    let sink = Arc::new(UsageSink::default());
    let e = chat_charged(
        sock,
        "w".into(),
        "x".into(),
        torin.clone(),
        Arc::clone(&sink),
        ledger_with(&dir, &[900]),
    )
    .await
    .unwrap_err();
    assert!(e.to_string().contains("approval 3"));
    assert_eq!(
        sink.take(&torin).tokens(),
        900,
        "an error wake's spend goes to the sink"
    );
}

#[tokio::test]
async fn spend_between_wakes_is_owed_once() {
    let dir = tempfile::tempdir().unwrap();
    let torin = EntityId::from("torin");
    let charge = ledger_with(&dir, &[250]);
    assert_eq!(charge.settle(&torin, Usage::default()).await.tokens(), 250);
    assert!(charge.settle(&torin, Usage::default()).await.is_zero());
}

#[tokio::test]
async fn no_daemon_starts_after_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = LittleguyConfig {
        bin: dir.path().join("no-such-lg"),
        data_dir: dir.path().to_path_buf(),
        ..LittleguyConfig::default()
    };
    let torin = EntityId::from("torin");
    let mind = LittleguyMind::new(cfg, HashMap::from([(torin.clone(), persona())]));
    mind.shutdown().await;
    assert!(mind.is_closed());
    let e = mind.ensure(&torin).await.unwrap_err();
    assert!(e.to_string().contains("shutting down"), "{e}");
    assert!(
        !dir.path().join("torin").exists(),
        "nothing was started or written"
    );
    mind.shutdown().await;
}

/// A stand-in for a daemon: a shell whose command line names `arg`.
///
/// It returns once `/proc` shows that command line. Right after a spawn
/// the kernel may not have set the new program's arguments yet, and
/// `reap_orphan` then reads an empty command line and leaves the stand-in
/// alone.
async fn stand_in(arg: &Path) -> tokio::process::Child {
    let child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg("sleep 30; true")
        .arg(arg)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = child.id().unwrap();
    if Path::new("/proc").exists() {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !names(pid, arg) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the stand-in shows its command line");
    }
    child
}

/// Whether the command line of process `pid` names `arg`.
fn names(pid: u32, arg: &Path) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmd| {
        cmd.split(|b| *b == 0)
            .any(|a| a == arg.as_os_str().as_encoded_bytes())
    })
}

#[tokio::test]
async fn a_stand_in_names_its_socket_once_it_is_returned() {
    if !Path::new("/proc").exists() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("lg.sock");
    for _ in 0..20 {
        let mut child = stand_in(&sock).await;
        assert!(names(child.id().unwrap(), &sock), "the stand-in has exec'd");
        child.kill().await.unwrap();
    }
}

#[tokio::test]
async fn an_orphan_daemon_from_an_earlier_run_is_stopped() {
    if !Path::new("/proc").exists() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("lg.sock");
    let mut orphan = stand_in(&sock).await;
    std::fs::write(dir.path().join(PID_FILE), orphan.id().unwrap().to_string()).unwrap();
    reap_orphan(dir.path(), &sock).await;
    let status = tokio::time::timeout(Duration::from_secs(15), orphan.wait())
        .await
        .expect("the orphan was stopped")
        .unwrap();
    assert!(!status.success());
    assert!(!dir.path().join(PID_FILE).exists());
}

#[tokio::test]
async fn a_recycled_pid_that_is_not_our_daemon_is_left_alone() {
    if !Path::new("/proc").exists() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut stranger = stand_in(&dir.path().join("somebody-else.sock")).await;
    std::fs::write(
        dir.path().join(PID_FILE),
        stranger.id().unwrap().to_string(),
    )
    .unwrap();
    reap_orphan(dir.path(), &dir.path().join("lg.sock")).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(300), stranger.wait())
            .await
            .is_err(),
        "an unrelated process keeps running"
    );
    stranger.kill().await.unwrap();
}

/// A wake that times out does not settle until its op is read: the
/// entity stays busy, so a late command lands in its own turn.
#[tokio::test]
async fn a_timed_out_wake_settles_only_once_its_op_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let torin = EntityId::from("torin");
    let cfg = LittleguyConfig {
        bin: dir.path().join("no-such-lg"),
        data_dir: dir.path().to_path_buf(),
        ..LittleguyConfig::default()
    };
    let mind = LittleguyMind::new(cfg, HashMap::from([(torin.clone(), persona())]));
    // A daemon that answers only when told, behind a running stand-in.
    let sock = dir.path().join("fake.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut req = String::new();
        reader.read_line(&mut req).await.unwrap();
        go_rx.await.unwrap();
        let _ = reader
            .into_inner()
            .write_all(b"{\"kind\":\"reply\",\"text\":\"late\"}\n")
            .await;
    });
    mind.daemons.lock().await.insert(
        torin.clone(),
        Daemon {
            child: stand_in(&sock).await,
            socket: sock.clone(),
            ui_port: 4123,
            approvals: ApprovalCheck::Off,
        },
    );
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let game = Game::new(
        World::new(Arc::clone(&pack), 1),
        None,
        Arc::new(ManualClock::new(NOW)),
    );
    let typing = Gate::new(Arc::clone(&game), 1, 0.0);
    let ctx = WakeContext {
        entity: torin.clone(),
        name: "Torin".into(),
        priority: Priority::Booked,
        reason: "test".into(),
        events: Vec::new(),
        event_views: Vec::new(),
        look: serde_json::Value::Null,
        you: serde_json::Value::Null,
        macro_tick: false,
        macro_ticks: 0,
        login: false,
        wake_id: "wake-torin-1".into(),
        now_ms: NOW,
        goals: Vec::new(),
        steward: Vec::new(),
        booked: Vec::new(),
    };
    let tools = MudTools::new(game, typing, torin.clone());
    let cut = tokio::time::timeout(Duration::from_millis(200), mind.wake(ctx, tools)).await;
    assert!(cut.is_err(), "the wake timed out");
    let mut settled = mind.settled(&torin);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut settled)
            .await
            .is_err(),
        "the op is still out: not settled"
    );
    go_tx.send(()).unwrap();
    server.await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), settled)
        .await
        .expect("settled once the op was read");
    mind.shutdown().await;
}

/// A daemon that holds its first op until told, and answers the rest at
/// once: the chats it was sent, the signal that answers the first, and
/// its task.
fn holding_daemon(
    sock: &Path,
) -> (
    Arc<std::sync::atomic::AtomicUsize>,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = UnixListener::bind(sock).unwrap();
    let chats = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
    let counted = Arc::clone(&chats);
    let server = tokio::spawn(async move {
        let mut go = Some(go_rx);
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let mut reader = BufReader::new(stream);
            let mut req = String::new();
            reader.read_line(&mut req).await.unwrap();
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(go) = go.take() {
                go.await.unwrap();
            }
            let _ = reader
                .into_inner()
                .write_all(b"{\"kind\":\"reply\",\"text\":\"done\"}\n")
                .await;
        }
    });
    (chats, go_tx, server)
}

/// The owner's log: a wake given up while its op still ran was followed at
/// once by a second chat, so two ops of one daemon overlapped. The entity
/// stays busy until the op is read: a tell that comes meanwhile waits, and
/// its chat starts only after the first op answered.
#[tokio::test]
async fn no_second_chat_starts_while_an_op_is_live() {
    use std::sync::atomic::Ordering;

    use crate::runtime::{ObserverEvent, Supervisor, SupervisorConfig};
    use crate::scripted::FallbackMind;

    let dir = tempfile::tempdir().unwrap();
    let torin = EntityId::from("torin");
    let cfg = LittleguyConfig {
        bin: dir.path().join("no-such-lg"),
        data_dir: dir.path().to_path_buf(),
        ..LittleguyConfig::default()
    };
    let mind = LittleguyMind::new(cfg, HashMap::from([(torin.clone(), persona())]));
    let sock = dir.path().join("fake.sock");
    let (chats, go_tx, server) = holding_daemon(&sock);
    mind.daemons.lock().await.insert(
        torin.clone(),
        Daemon {
            child: stand_in(&sock).await,
            socket: sock.clone(),
            ui_port: 4123,
            approvals: ApprovalCheck::Off,
        },
    );
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let clock = Arc::new(ManualClock::new(NOW));
    let game = Game::new(World::new(Arc::clone(&pack), 1), None, clock.clone());
    let typing = Gate::new(Arc::clone(&game), 1, 0.0);
    let mind = Arc::new(mind);
    let sup = Supervisor::new(
        Arc::clone(&game),
        typing,
        HashMap::from([(torin.clone(), Arc::clone(&mind) as Arc<dyn Mind>)]),
        Arc::new(FallbackMind::from_cast(&pack.cast)),
        SupervisorConfig {
            wake_timeout_ms: 100,
            first_macro_tick_secs: [3_600, 3_600],
            ..SupervisorConfig::default()
        },
    );
    let mut obs = sup.subscribe();
    let mut rx = game.subscribe();
    sup.fire_macro_tick(&torin);
    let first = sup.step();
    assert_eq!(first.len(), 1);
    // The wake gives up; its op is still out.
    let timed_out = loop {
        if let ObserverEvent::WakeFinished { ok, .. } = obs.recv().await.unwrap().as_ref() {
            break !*ok;
        }
    };
    assert!(timed_out, "the first wake timed out");
    let ada = game.login("ada").unwrap();
    assert!(game.execute(&ada, "tell torin got ore").ok);
    let torin_started = |obs: &mut tokio::sync::broadcast::Receiver<Arc<ObserverEvent>>| {
        std::iter::from_fn(|| obs.try_recv().ok())
            .filter(
                |e| matches!(&**e, ObserverEvent::WakeStarted { entity, .. } if entity == &torin),
            )
            .count()
    };
    for _ in 0..10 {
        while let Ok(ev) = rx.try_recv() {
            sup.observe(&ev);
        }
        clock.advance(1_000);
        // Other NPCs' wakes (ada's arrival) end at once: they have no mind.
        for h in sup.step() {
            h.await.unwrap();
        }
        assert_eq!(torin_started(&mut obs), 0, "busy while the op is live");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(chats.load(Ordering::SeqCst), 1, "no second chat");
    go_tx.send(()).unwrap();
    for h in first {
        h.await.unwrap();
    }
    clock.advance(1_000);
    for h in sup.step() {
        h.await.unwrap();
    }
    assert_eq!(torin_started(&mut obs), 1, "the tell's wake goes out now");
    assert_eq!(chats.load(Ordering::SeqCst), 2);
    mind.shutdown().await;
    server.abort();
}

/// Runs a real `lg serve` when `LG_BIN` names one (the CI job that builds
/// littleguy sets it). It checks that littleguy accepts the generated
/// config, opens its socket, and answers a wake.
#[tokio::test(flavor = "multi_thread")]
async fn a_real_lg_serve_answers_a_wake() {
    let Ok(bin) = std::env::var("LG_BIN") else {
        eprintln!("LG_BIN is not set: skipping the real littleguy check");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let cfg = LittleguyConfig {
        bin: PathBuf::from(bin),
        data_dir: dir.path().to_path_buf(),
        // Nothing listens here; a server that does not start costs only its tools.
        mcp_url: "http://127.0.0.1:9/mcp".into(),
        heartbeat_minutes: 0,
        ..LittleguyConfig::default()
    };
    let torin = EntityId::from("torin");
    let mind = LittleguyMind::new(cfg, HashMap::from([(torin.clone(), persona())]));
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let game = Game::new(World::new(pack, 1), None, Arc::new(ManualClock::new(1)));
    let typing = Gate::new(Arc::clone(&game), 1, 0.0);
    let ctx = WakeContext {
        entity: torin.clone(),
        name: "Torin".into(),
        priority: Priority::MacroTick,
        reason: "macro-tick".into(),
        events: vec![],
        event_views: vec![],
        look: game.look(&torin),
        you: serde_json::Value::Null,
        macro_tick: true,
        macro_ticks: 1,
        login: false,
        wake_id: "wake-torin-0-1".into(),
        now_ms: NOW,
        goals: Vec::new(),
        steward: Vec::new(),
        booked: Vec::new(),
    };
    let r = mind
        .wake(ctx, MudTools::new(game, typing, torin.clone()))
        .await;
    let log = std::fs::read_to_string(dir.path().join("torin/lg.log")).unwrap_or_default();
    let r = r.unwrap_or_else(|e| panic!("{e}\n--- lg.log ---\n{log}"));
    assert!(
        r.text.unwrap_or_default().contains("offline"),
        "the offline model echoes"
    );
    // The meter reads the live daemon's database through its own
    // read-only handle while the daemon still holds it.
    let live = LedgerMeter::new(dir.path().join("torin/lg.db"));
    assert!(
        matches!(live.charge(NOW), Ok(Some(_))),
        "the ledger of a running lg serve is readable"
    );
    // The observer's link signs a browser in to the daemon's pages.
    let link = mind.ui_link(&torin).await.unwrap();
    let rest = link.strip_prefix("http://127.0.0.1:").unwrap();
    let (port, path) = rest.split_once('/').unwrap();
    assert_ne!(port, "0", "a link to a real port: {link}");
    let mut http = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    http.write_all(
        format!("GET /{path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
            .as_bytes(),
    )
    .await
    .unwrap();
    let mut status = String::new();
    BufReader::new(http).read_line(&mut status).await.unwrap();
    assert!(
        status.starts_with("HTTP/1.1 2") || status.starts_with("HTTP/1.1 3"),
        "the sign-in link works: {status}"
    );
    let pid: u32 = std::fs::read_to_string(dir.path().join("torin").join(PID_FILE))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    mind.shutdown().await;
    assert!(!dir.path().join("torin").join(PID_FILE).exists());
    if Path::new("/proc").exists() {
        // Killed and reaped by `shutdown`: no zombie, no live daemon.
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "lg serve {pid} is gone"
        );
    }
}

/// `text` without its terminal colour codes (`ESC [ ... m`).
fn without_colour(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[test]
fn colour_codes_are_stripped() {
    assert_eq!(
        without_colour("\u{1b}[3mapplied\u{1b}[0m\u{1b}[2m=\u{1b}[0m[\"memory\"]"),
        r#"applied=["memory"]"#
    );
}

/// The supervisor is a real daemon's owner on its socket: reads, a
/// decision, and a quiet reload that lg applies live.
#[tokio::test(flavor = "multi_thread")]
async fn a_real_lg_serve_takes_owner_requests() {
    use owner::{OwnerRequest as Req, OwnerResponse as Resp, PlanDecision, request};
    let Ok(bin) = std::env::var("LG_BIN") else {
        eprintln!("LG_BIN is not set: skipping the real littleguy owner check");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let cfg = LittleguyConfig {
        bin: PathBuf::from(bin),
        data_dir: dir.path().to_path_buf(),
        mcp_url: "http://127.0.0.1:9/mcp".into(),
        ..LittleguyConfig::default()
    };
    let torin = EntityId::from("torin");
    let mind = LittleguyMind::new(cfg, HashMap::from([(torin.clone(), persona())]));
    let started = mind.ensure(&torin).await;
    let log = || std::fs::read_to_string(dir.path().join("torin/lg.log")).unwrap_or_default();
    let sock = started.unwrap_or_else(|e| panic!("{e}\n--- lg.log ---\n{}", log()));
    assert_eq!(mind.running_socket(&torin).await.as_ref(), Some(&sock));
    assert_eq!(
        mind.approval_check(&torin).await,
        Some(ApprovalCheck::Off),
        "the start found approvals off"
    );
    let t = Duration::from_secs(30);
    let ask = |r: Req| {
        let sock = sock.clone();
        async move { request(&sock, &r, t).await.unwrap() }
    };
    assert!(
        matches!(ask(Req::Plans).await, Resp::Plans { .. }),
        "{}",
        log()
    );
    assert!(
        matches!(ask(Req::Notes).await, Resp::Notes { .. }),
        "{}",
        log()
    );
    assert!(matches!(ask(Req::Approvals).await, Resp::Off { .. }));
    let decided = ask(Req::Decide {
        decision: PlanDecision::Accept,
        id: 99,
    })
    .await;
    assert!(
        matches!(&decided, Resp::Error { message } if message.contains("not a proposed goal")),
        "{decided:?}"
    );
    let overlays = ask(Req::Overlays).await;
    assert!(
        matches!(overlays, Resp::Off { .. } | Resp::Overlays { .. }),
        "{overlays:?}"
    );
    let port = |text: String| -> toml::Table { toml::from_str(&text).unwrap() };
    let before = port(std::fs::read_to_string(mind.config_path(&torin)).unwrap());
    mind.set_quiet(&torin, true).await.unwrap();
    // lg looks at its config every 2 s and reloads a changed one itself,
    // so here its watcher applies the rewrite before the explicit reload.
    tokio::time::sleep(Duration::from_millis(2_600)).await;
    let quiet = ask(Req::Reload).await;
    let Resp::Reloaded {
        applied, restart, ..
    } = &quiet
    else {
        panic!("{quiet:?}\n{}", log());
    };
    // Any `reloaded` answer is success: this reload or lg's watcher before
    // it applied memory and heartbeat, and lg logs each reload it applies.
    let by_us = applied.iter().any(|a| a == "memory") && applied.iter().any(|a| a == "heartbeat");
    let by_watcher = without_colour(&log()).lines().any(|l| {
        l.contains("the config reloaded") && l.contains(r#"applied=["memory", "heartbeat"]"#)
    });
    assert!(by_us || by_watcher, "{quiet:?}\n{}", log());
    assert!(restart.is_empty(), "{quiet:?}");
    // The state says so too: lg serves no notes without memory.
    assert!(
        matches!(ask(Req::Notes).await, Resp::Off { .. }),
        "quiet: notes are off\n{}",
        log()
    );
    let after = port(std::fs::read_to_string(mind.config_path(&torin)).unwrap());
    assert_eq!(before["ui"], after["ui"], "the pages keep their port");
    mind.set_quiet(&torin, false).await.unwrap();
    let loud = ask(Req::Reload).await;
    assert!(
        matches!(&loud, Resp::Reloaded { restart, .. } if restart.is_empty()),
        "{loud:?}"
    );
    assert!(
        matches!(ask(Req::Notes).await, Resp::Notes { .. }),
        "loud: notes are back\n{}",
        log()
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        matches!(ask(Req::Plans).await, Resp::Plans { .. }),
        "still answers after the reloads:\n{}",
        log()
    );
    mind.shutdown().await;
    assert!(mind.running_socket(&torin).await.is_none());
}

#[test]
fn a_littleguy_mind_primes_from_its_daemons_ledger_at_boot() {
    let dir = tempfile::tempdir().unwrap();
    let torin = EntityId::from("torin");
    let cfg = LittleguyConfig {
        data_dir: dir.path().to_path_buf(),
        ..LittleguyConfig::default()
    };
    let mind = LittleguyMind::new(cfg, HashMap::from([(torin.clone(), persona())]));
    assert_eq!(mind.spend_source(), SpendSource::Ledger);
    assert_eq!(mind.prime(&torin, 1_000), None, "no daemon database yet");
    let entity_dir = mind.entity_dir(&torin);
    std::fs::create_dir_all(&entity_dir).unwrap();
    let (_, conn) = crate::ledger::fake::ledger(&entity_dir);
    let day = 20_000 * 86_400_000;
    crate::ledger::fake::call(&conn, 1, 70, 30, Some(4), day + 10);
    let primed = mind.prime(&torin, day + 20).unwrap();
    assert_eq!((primed.tokens(), primed.calls), (100, 1));
    // A broken file primes nothing; the boot goes on.
    let other = EntityId::from("marta");
    let broken = mind.entity_dir(&other);
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join("lg.db"), b"not a database").unwrap();
    assert_eq!(mind.prime(&other, day + 20), None);
}

/// One raw owner line, for a request the owner client never sends (the
/// human's `overlay_rollback`).
async fn raw_owner_line(socket: &Path, line: &str) -> serde_json::Value {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut stream = tokio::net::UnixStream::connect(socket).await.unwrap();
    stream.write_all(line.as_bytes()).await.unwrap();
    stream.write_all(b"\n").await.unwrap();
    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    reader.read_line(&mut buf).await.unwrap();
    serde_json::from_str(buf.trim()).unwrap()
}

#[tokio::test]
async fn a_real_lg_with_refine_takes_overlay_requests() {
    use owner::{OwnerRequest as Req, OwnerResponse as Resp, request};
    let Ok(bin) = std::env::var("LG_BIN") else {
        eprintln!("LG_BIN is not set: skipping the real littleguy overlay check");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let cfg = LittleguyConfig {
        bin: PathBuf::from(bin),
        data_dir: dir.path().to_path_buf(),
        mcp_url: "http://127.0.0.1:9/mcp".into(),
        refine: RefineMode::Menu,
        ..LittleguyConfig::default()
    };
    let torin = EntityId::from("torin");
    let mind = LittleguyMind::new(cfg, HashMap::from([(torin.clone(), persona())]));
    let log = || std::fs::read_to_string(dir.path().join("torin/lg.log")).unwrap_or_default();
    let sock = match mind.ensure(&torin).await {
        Ok(sock) => sock,
        Err(e) => {
            let text = format!("{e}\n{}", log());
            if text.contains("needs the `refine` feature") {
                eprintln!("this lg has no refine feature: skipping the overlay check\n{text}");
                return;
            }
            panic!("{text}");
        }
    };
    let t = Duration::from_secs(30);
    let ask = |r: Req| {
        let sock = sock.clone();
        async move { request(&sock, &r, t).await.unwrap() }
    };
    let listed = ask(Req::Overlays).await;
    assert!(
        matches!(&listed, Resp::Overlays { active: None, .. }),
        "{listed:?}\n{}",
        log()
    );
    let put = |text: &str| Req::OverlayPut {
        overlay: crate::steward::vet::overlay_json(text),
    };
    let Resp::OverlayStored { version: baseline } = ask(put("")).await else {
        panic!("{}", log());
    };
    let on = ask(Req::OverlayActivate { version: baseline }).await;
    assert_eq!(
        on,
        Resp::OverlayActive {
            version: baseline,
            previous: None
        }
    );
    let menu = crate::steward::vet::default_menu();
    let text = crate::steward::vet::render_guardrails(&menu, &["brevity".to_owned()]);
    let Resp::OverlayStored { version: a } = ask(put(&text)).await else {
        panic!("{}", log());
    };
    let shown = ask(Req::OverlayShow { version: a }).await;
    let Resp::Overlay { overlay, entry } = &shown else {
        panic!("{shown:?}");
    };
    assert!(
        crate::steward::vet::overlay_matches(overlay, &text),
        "{overlay}"
    );
    assert_eq!(entry.author, "operator:mud");
    let on = ask(Req::OverlayActivate { version: a }).await;
    assert_eq!(
        on,
        Resp::OverlayActive {
            version: a,
            previous: Some(baseline)
        }
    );
    // The human's Roll back, sent by the test (never by the Steward).
    let back = raw_owner_line(&sock, r#"{"kind":"overlay_rollback"}"#).await;
    assert_eq!(back["kind"], "overlay_active", "{back}");
    assert_eq!(
        back["version"], baseline,
        "one Roll back lands on the baseline"
    );
    let listed = ask(Req::Overlays).await;
    assert!(
        matches!(&listed, Resp::Overlays { active: Some(v), .. } if *v == baseline),
        "{listed:?}"
    );
    mind.shutdown().await;
}
