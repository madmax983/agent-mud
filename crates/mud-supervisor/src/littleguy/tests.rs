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
        token: "secret-token".into(),
    }
}

#[test]
fn the_config_points_littleguy_at_the_mud() {
    let cfg = LittleguyConfig::default();
    let text = render_config(&cfg, &persona(), 4123);
    let v: toml::Table = toml::from_str(&text).unwrap();
    assert_eq!(v["owner"].as_str(), Some("mud"));
    assert!(v["system"].as_str().unwrap().contains("Get ore."));
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
    let v: toml::Table = toml::from_str(&render_config(&cfg, &persona(), 4123)).unwrap();
    assert!(!v.contains_key("max_steps"));
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
    assert!(sink.take(&torin).is_zero());
}

/// A charge with no ledger file: the daemon's reported usage stands.
fn no_ledger(dir: &tempfile::TempDir) -> Charge {
    Charge {
        meter: Arc::new(LedgerMeter::new(dir.path().join("no-ledger.db"))),
        now_ms: NOW,
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
fn stand_in(arg: &Path) -> tokio::process::Child {
    tokio::process::Command::new("sh")
        .arg("-c")
        .arg("sleep 30; true")
        .arg(arg)
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

#[tokio::test]
async fn an_orphan_daemon_from_an_earlier_run_is_stopped() {
    if !Path::new("/proc").exists() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("lg.sock");
    let mut orphan = stand_in(&sock);
    std::fs::write(dir.path().join(PID_FILE), orphan.id().unwrap().to_string()).unwrap();
    reap_orphan(dir.path(), &sock).await;
    let status = tokio::time::timeout(Duration::from_secs(5), orphan.wait())
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
    let mut stranger = stand_in(&dir.path().join("somebody-else.sock"));
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
        macro_tick: true,
        macro_ticks: 1,
        login: false,
        wake_id: "wake-torin-0-1".into(),
        now_ms: NOW,
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
