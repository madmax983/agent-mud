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
    let text = render_config(&cfg, &persona());
    let v: toml::Table = toml::from_str(&text).unwrap();
    assert_eq!(v["owner"].as_str(), Some("mud"));
    assert!(v["system"].as_str().unwrap().contains("Get ore."));
    let server = &v["mcp"]["server"][0];
    assert_eq!(server["url"].as_str(), Some("http://127.0.0.1:3000/mcp"));
    assert_eq!(server["bearer_env"].as_str(), Some(TOKEN_ENV));
    assert_eq!(server["labels"][0].as_str(), Some("untrusted_input"));
    assert_eq!(v["ui"]["bind"].as_str(), Some("127.0.0.1:0"));
    assert_eq!(v["tools"]["web_fetch"].as_bool(), Some(false));
    assert!(
        !text.contains("secret-token"),
        "the token never goes in a file"
    );
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
async fn waiting_for_an_approval_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("lg.sock");
    let _s = fake_daemon(&sock, r#"{"kind":"waiting","approval":7}"#);
    let e = chat(&sock, "w", "x").await.unwrap_err();
    assert!(e.to_string().contains("approval 7"));
    assert!(chat(&dir.path().join("none.sock"), "w", "x").await.is_err());
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
        macro_ticks: 1,
        login: false,
        wake_id: "wake-torin-0-1".into(),
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
    mind.shutdown().await;
}
