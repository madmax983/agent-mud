//! Secrets never reach a trace log: not the password, not a resume token,
//! not tungstenite's frame traces (which hold both), at the loudest level.
//!
//! Its own test binary: it installs the global `LogTracer`, so
//! tungstenite's `log` records reach the same subscriber the server uses.
#![expect(clippy::unwrap_used, reason = "test helpers fail loudly")]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use autumn_web::test::TestApp;
use futures::{SinkExt, StreamExt};
use mud_server::config::MudConfig;
use mud_server::{build, settings, web};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

const PW: &str = "lantern-and-jerkin";

#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Buffer {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl tracing_subscriber::fmt::MakeWriter<'_> for Buffer {
    type Writer = Self;
    fn make_writer(&self) -> Self {
        self.clone()
    }
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn boot() -> SocketAddr {
    let mut cfg = MudConfig::default();
    cfg.world.db = None;
    cfg.supervisor.typing_scale = 0.0;
    cfg.accounts.bcrypt_cost = 4;
    let built = Arc::new(build(&cfg).await.unwrap());
    let mud = Arc::clone(&built.mud);
    let router = TestApp::new()
        .config(settings(&cfg).unwrap())
        .routes(web::routes())
        .state_initializer(move |s| mud_server::install(s, &mud))
        .build()
        .into_router();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        autumn_web::reexports::axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .ok();
    });
    addr
}

async fn ws(addr: SocketAddr) -> Ws {
    tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap()
        .0
}

async fn send(ws: &mut Ws, v: Value) {
    ws.send(Message::Text(v.to_string().into())).await.unwrap();
}

/// Reads until a message whose text holds `needle` (or whose kind is
/// `needle`); returns every message read.
async fn until(ws: &mut Ws, needle: &str) -> Vec<Value> {
    let mut seen = Vec::new();
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(3), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let Message::Text(t) = msg {
            let v: Value = serde_json::from_str(&t).unwrap();
            let hit = v["kind"] == needle || v["text"].as_str().is_some_and(|s| s.contains(needle));
            seen.push(v);
            if hit {
                return seen;
            }
        }
    }
}

#[tokio::test]
async fn secrets_never_reach_a_trace_log() {
    tracing_log::LogTracer::init().unwrap();
    let buf = Buffer::default();
    let _log =
        tracing::subscriber::set_default(mud_server::logging::subscriber("trace", buf.clone()));
    let addr = boot().await;
    // A new character, remembered.
    let mut c = ws(addr).await;
    send(&mut c, json!({ "line": "ada" })).await;
    until(&mut c, "Choose a password").await;
    send(&mut c, json!({ "line": PW, "remember": true })).await;
    until(&mut c, "Type it again").await;
    send(&mut c, json!({ "line": PW, "remember": true })).await;
    let seen = until(&mut c, "Town Square").await;
    let token = seen
        .iter()
        .find(|v| v["kind"] == "remember")
        .and_then(|v| v["token"].as_str())
        .unwrap()
        .to_owned();
    drop(c);
    // A password login, and a wrong one.
    let mut c = ws(addr).await;
    send(&mut c, json!({ "line": "ada" })).await;
    until(&mut c, "Password:").await;
    send(&mut c, json!({ "line": "not-the-password" })).await;
    until(&mut c, "Wrong password.").await;
    send(&mut c, json!({ "line": PW })).await;
    until(&mut c, "Town Square").await;
    drop(c);
    // A resume.
    let mut c = ws(addr).await;
    send(&mut c, json!({ "resume": token })).await;
    until(&mut c, "Town Square").await;
    drop(c);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let log = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(log.contains("mud::auth"), "the auth target logs: {log}");
    let (_, raw) = token.split_once(':').unwrap();
    for secret in [PW, "not-the-password", raw, "$2b$"] {
        assert!(!log.contains(secret), "the log holds {secret}");
    }
}
