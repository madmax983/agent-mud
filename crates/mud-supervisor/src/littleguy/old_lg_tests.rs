//! An `lg` older than the MUD: stand-ins that refuse a part of `lg.toml`
//! the way an lg built before that part existed does (`deny_unknown_fields`
//! and lg's `error: the config is not valid: ...`).

use std::sync::Arc;

use mud_world::{Game, ManualClock, World, WorldPack};
use tokio::net::UnixListener;

use super::*;
use crate::gate::Gate;
use crate::router::Priority;

const NOW: u64 = 20_000 * 86_400_000 + 43_200_000;

/// What the stand-in refuses: a line it finds by `pattern`, the field lg
/// names, its column and the carets under it.
struct Refuse {
    pattern: &'static str,
    field: &'static str,
    column: usize,
    carets: &'static str,
}

const COMPACTION: Refuse = Refuse {
    pattern: r"^\[compaction\]$",
    field: "compaction",
    column: 2,
    carets: " ^^^^^^^^^^",
};

const MAX_REQUEST_BYTES: Refuse = Refuse {
    pattern: "^max_request_bytes = ",
    field: "max_request_bytes",
    column: 1,
    carets: "^^^^^^^^^^^^^^^^^",
};

/// `[ui]` holds the pages' port: the MUD cannot leave it out.
const UI: Refuse = Refuse {
    pattern: r"^\[ui\]$",
    field: "ui",
    column: 2,
    carets: " ^^",
};

/// Writes a stand-in `lg` to `dir` and returns its path. It counts its
/// starts in `starts` beside its config, refuses the first of `refuse` its
/// config holds, and otherwise runs `then` (with its arguments in `$@`).
fn old_lg(dir: &Path, refuse: &[Refuse], then: &str) -> PathBuf {
    let mut script = String::from(
        "#!/bin/sh\n\
         prev=; for a in \"$@\"; do [ \"$prev\" = --config ] && config=\"$a\"; prev=\"$a\"; done\n\
         echo start >> \"$(dirname \"$config\")/starts\"\n\
         refuse() {\n\
         n=$(grep -n -m1 -e \"$1\" \"$config\" | cut -d: -f1)\n\
         [ -n \"$n\" ] || return 0\n\
         line=$(sed -n \"${n}p\" \"$config\")\n\
         pad=$(printf '%s' \"$n\" | sed 's/./ /g')\n\
         printf 'error: the config is not valid: TOML parse error at line %s, column %s\\n%s |\\n%s | %s\\n%s | %s\\nunknown field `%s`, expected one of `owner`, `system`, `window`, `max_steps`, `tools`, `memory`, `heartbeat`\\n\\n' \"$n\" \"$3\" \"$pad\" \"$n\" \"$line\" \"$pad\" \"$4\" \"$2\" >&2\n\
         exit 2\n\
         }\n",
    );
    for r in refuse {
        script.push_str(
            &[
                "refuse",
                &format!("'{}'", r.pattern),
                r.field,
                &r.column.to_string(),
                &format!("'{}'", r.carets),
            ]
            .join(" "),
        );
        script.push('\n');
    }
    script.push_str(then);
    script.push('\n');
    let bin = dir.join("old-lg");
    std::fs::write(&bin, script).unwrap();
    let mut perms = std::fs::metadata(&bin).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&bin, perms).unwrap();
    bin
}

/// Runs until killed, once its config is accepted.
const RUNS: &str = "exec sleep 30";

/// Crashes with a message that names no field.
const CRASHES: &str = "echo 'error: the database is locked' >&2; exit 2";

fn persona(name: &str) -> Persona {
    Persona {
        name: name.into(),
        persona: "A gruff smith.".into(),
        goal: "Get ore.".into(),
        body: "You regain 1 HP every 5 minutes while in the world.".into(),
        token: "secret-token".into(),
    }
}

fn mind(dir: &Path, bin: PathBuf) -> Arc<LittleguyMind> {
    let cfg = LittleguyConfig {
        bin,
        data_dir: dir.join("agents"),
        start_timeout_secs: 10,
        ..LittleguyConfig::default()
    };
    Arc::new(LittleguyMind::new(
        cfg,
        HashMap::from([
            (EntityId::from("torin"), persona("Torin")),
            (EntityId::from("marta"), persona("Marta")),
        ]),
    ))
}

/// How often the stand-in of `entity` started.
fn starts(mind: &LittleguyMind, entity: &EntityId) -> usize {
    std::fs::read_to_string(mind.entity_dir(entity).join("starts")).map_or(0, |s| s.lines().count())
}

/// Serves the socket of `entity`'s stand-in once it runs on an accepted
/// config, as lg would: approvals are off, and a chat gets a reply.
fn serve(mind: &Arc<LittleguyMind>, entity: &EntityId) -> tokio::task::JoinHandle<()> {
    let dir = mind.entity_dir(entity);
    tokio::spawn(async move {
        // The stand-in that runs is the one whose config was accepted: it
        // wrote a start and did not exit.
        loop {
            let pid = std::fs::read_to_string(dir.join(PID_FILE)).unwrap_or_default();
            let running = pid.trim().parse::<u32>().is_ok_and(|p| {
                std::fs::read(format!("/proc/{p}/cmdline")).is_ok_and(|c| c.starts_with(b"sleep"))
            });
            if running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let listener = UnixListener::bind(dir.join("lg.sock")).unwrap();
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let mut reader = BufReader::new(stream);
            let mut req = String::new();
            reader.read_line(&mut req).await.unwrap();
            let answer: &[u8] = if req.contains("\"approvals\"") {
                b"{\"kind\":\"off\",\"message\":\"approvals are off\"}\n"
            } else {
                b"{\"kind\":\"reply\",\"text\":\"ok\"}\n"
            };
            let _ = reader.into_inner().write_all(answer).await;
        }
    })
}

fn wake_ctx(entity: &EntityId) -> (WakeContext, MudTools) {
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let game = Game::new(
        World::new(Arc::clone(&pack), 1),
        None,
        Arc::new(ManualClock::new(NOW)),
    );
    let typing = Gate::new(Arc::clone(&game), 1, 0.0);
    let ctx = WakeContext {
        entity: entity.clone(),
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
        wake_id: format!("wake-{entity}-1"),
        now_ms: NOW,
        goals: Vec::new(),
        steward: Vec::new(),
        booked: Vec::new(),
    };
    let tools = MudTools::new(game, typing, entity.clone());
    (ctx, tools)
}

fn config_of(mind: &LittleguyMind, entity: &EntityId) -> toml::Table {
    toml::from_str(&std::fs::read_to_string(mind.config_path(entity)).unwrap()).unwrap()
}

/// The incident: an lg built before littleguy #108 refuses `[compaction]`.
/// The MUD drops it for that binary, warns once, and the wake plays.
#[tokio::test]
async fn an_lg_without_compaction_runs_without_it_and_warns_once() {
    let dir = tempfile::tempdir().unwrap();
    let bin = old_lg(dir.path(), &[COMPACTION], RUNS);
    let mind = mind(dir.path(), bin.clone());
    let (torin, marta) = (EntityId::from("torin"), EntityId::from("marta"));
    let served = serve(&mind, &torin);
    let (ctx, tools) = wake_ctx(&torin);
    let reply = tokio::time::timeout(Duration::from_secs(20), mind.wake(ctx, tools))
        .await
        .expect("the wake ends")
        .expect("the wake plays on the older lg");
    assert_eq!(reply.text.as_deref(), Some("ok"));
    assert_eq!(starts(&mind, &torin), 2, "refused once, then started");
    let v = config_of(&mind, &torin);
    assert!(!v.contains_key("compaction"), "left out");
    assert_eq!(
        v["max_request_bytes"].as_integer(),
        Some(240_000),
        "the rest stays"
    );
    assert_eq!(v["heartbeat"]["research"].as_bool(), Some(true));
    assert_eq!(compat::lacks(&bin), BTreeSet::from([Optional::Compaction]));
    let notices = mind.take_notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    let n = &notices[0];
    assert_eq!(
        (n.entity.as_str(), n.code.as_str()),
        ("torin", "lg_lacks_section")
    );
    for part in [
        "`compaction`",
        &bin.display().to_string(),
        LG_MIN_COMMIT,
        README_INSTALL,
        "littleguy #108",
        "compaction = false",
    ] {
        assert!(n.text.contains(part), "{part} in {}", n.text);
    }
    assert!(!n.text.contains("secret-token"), "no token in a notice");
    assert!(mind.take_notices().is_empty(), "taken once");
    // A quiet or loud rewrite keeps it out.
    mind.set_quiet(&torin, true).await.unwrap();
    assert!(!config_of(&mind, &torin).contains_key("compaction"));
    mind.set_quiet(&torin, false).await.unwrap();
    assert!(!config_of(&mind, &torin).contains_key("compaction"));
    // Another agent on the same binary starts without it at once, and no
    // second warning comes.
    let served_marta = serve(&mind, &marta);
    tokio::time::timeout(Duration::from_secs(20), mind.ensure(&marta))
        .await
        .expect("the start ends")
        .unwrap();
    assert_eq!(starts(&mind, &marta), 1);
    assert!(!config_of(&mind, &marta).contains_key("compaction"));
    assert!(
        mind.take_notices().is_empty(),
        "warned once per binary and part"
    );
    mind.shutdown().await;
    served.abort();
    served_marta.abort();
}

/// Two missing parts: each is dropped once, in turn.
#[tokio::test]
async fn each_missing_part_is_dropped_once() {
    let dir = tempfile::tempdir().unwrap();
    let bin = old_lg(dir.path(), &[MAX_REQUEST_BYTES, COMPACTION], RUNS);
    let mind = mind(dir.path(), bin.clone());
    let torin = EntityId::from("torin");
    let served = serve(&mind, &torin);
    tokio::time::timeout(Duration::from_secs(20), mind.ensure(&torin))
        .await
        .expect("the start ends")
        .unwrap();
    assert_eq!(starts(&mind, &torin), 3);
    let v = config_of(&mind, &torin);
    assert!(!v.contains_key("compaction") && !v.contains_key("max_request_bytes"));
    assert_eq!(
        compat::lacks(&bin),
        BTreeSet::from([Optional::Compaction, Optional::MaxRequestBytes])
    );
    let codes: Vec<String> = mind.take_notices().into_iter().map(|n| n.text).collect();
    assert_eq!(codes.len(), 2, "{codes:?}");
    assert!(codes[0].contains("`max_request_bytes`") && codes[1].contains("`compaction`"));
    mind.shutdown().await;
    served.abort();
}

/// A part the MUD needs is never dropped: the start fails with an error
/// that names the older lg, the commit it needs and the README.
#[tokio::test]
async fn an_lg_that_refuses_a_needed_section_fails_and_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let bin = old_lg(dir.path(), &[UI], RUNS);
    let mind = mind(dir.path(), bin.clone());
    let torin = EntityId::from("torin");
    let (ctx, tools) = wake_ctx(&torin);
    let Err(MindError::Unavailable(e)) = mind.wake(ctx, tools).await else {
        panic!("the wake fails");
    };
    for part in [
        "older than the MUD needs",
        "`ui`",
        &bin.display().to_string(),
        LG_MIN_COMMIT,
        README_INSTALL,
        "exit status: 2",
    ] {
        assert!(e.contains(part), "{part} in {e}");
    }
    assert_eq!(starts(&mind, &torin), 1, "no second start");
    assert!(compat::lacks(&bin).is_empty());
    assert_eq!(mind.take_notices(), Vec::new());
    assert!(config_of(&mind, &torin).contains_key("ui"), "never dropped");
}

/// A crash whose log names no field fails as it always did: one start,
/// no retry, no notice.
#[tokio::test]
async fn a_crash_with_no_refusal_fails_without_a_retry() {
    let dir = tempfile::tempdir().unwrap();
    let bin = old_lg(dir.path(), &[], CRASHES);
    let mind = mind(dir.path(), bin.clone());
    let torin = EntityId::from("torin");
    for n in 1..=2 {
        let Err(MindError::Unavailable(e)) = mind.ensure(&torin).await else {
            panic!("the start fails");
        };
        assert!(
            e.contains("lg serve for torin exited (exit status: 2); see"),
            "{e}"
        );
        assert!(!e.contains("older"), "{e}");
        assert_eq!(starts(&mind, &torin), n, "one start per try");
    }
    assert!(compat::lacks(&bin).is_empty());
    assert_eq!(mind.take_notices(), Vec::new());
}

/// When the start after a drop still fails, the error says the lg is
/// older than the MUD needs, and no third start is made.
#[tokio::test]
async fn a_second_failed_start_names_the_older_lg() {
    let dir = tempfile::tempdir().unwrap();
    let bin = old_lg(dir.path(), &[COMPACTION], CRASHES);
    let mind = mind(dir.path(), bin.clone());
    let torin = EntityId::from("torin");
    let Err(MindError::Unavailable(e)) = mind.ensure(&torin).await else {
        panic!("the start fails");
    };
    assert_eq!(starts(&mind, &torin), 2, "one re-render, no loop");
    for part in [
        "older than the MUD needs",
        "still exits",
        LG_MIN_COMMIT,
        README_INSTALL,
    ] {
        assert!(e.contains(part), "{part} in {e}");
    }
    assert_eq!(mind.take_notices().len(), 1, "the drop was still warned");
}

/// The incident on a real `lg serve` when `LG_BIN` names one: behind a
/// wrapper that refuses `[compaction]` as an older lg does, the MUD drops
/// it and the real daemon answers the wake.
#[tokio::test(flavor = "multi_thread")]
async fn a_real_lg_behind_an_old_config_reader_answers_a_wake() {
    let Ok(real) = std::env::var("LG_BIN") else {
        eprintln!("LG_BIN is not set: skipping the real littleguy degrade check");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let then = format!("exec '{real}' \"$@\"");
    let bin = old_lg(dir.path(), &[COMPACTION], &then);
    let cfg = LittleguyConfig {
        bin: bin.clone(),
        data_dir: dir.path().join("agents"),
        // Nothing listens here; a server that does not start costs only its tools.
        mcp_url: "http://127.0.0.1:9/mcp".into(),
        heartbeat_minutes: 0,
        ..LittleguyConfig::default()
    };
    let torin = EntityId::from("torin");
    let mind = LittleguyMind::new(cfg, HashMap::from([(torin.clone(), persona("Torin"))]));
    let (ctx, tools) = wake_ctx(&torin);
    let r = mind.wake(ctx, tools).await;
    let log = std::fs::read_to_string(mind.entity_dir(&torin).join("lg.log")).unwrap_or_default();
    let r = r.unwrap_or_else(|e| panic!("{e}\n--- lg.log ---\n{log}"));
    assert!(
        r.text.unwrap_or_default().contains("offline"),
        "the offline model echoes"
    );
    assert_eq!(starts(&mind, &torin), 2);
    assert!(!config_of(&mind, &torin).contains_key("compaction"));
    assert_eq!(mind.take_notices().len(), 1);
    mind.shutdown().await;
}
