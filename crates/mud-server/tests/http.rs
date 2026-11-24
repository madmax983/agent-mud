//! The server over real HTTP: terminal WebSocket, observer, SSE, and /mcp.
#![expect(clippy::unwrap_used, reason = "test helpers fail loudly")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use autumn_web::test::TestApp;
use futures::{SinkExt, StreamExt};
use mud_server::config::MudConfig;
use mud_server::{Built, TypingGate, build, settings, web};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

async fn boot() -> (SocketAddr, Arc<Built>) {
    boot_with(None).await
}

/// Boots the server, with `admin_token` as the observer token if given.
async fn boot_with(admin_token: Option<&str>) -> (SocketAddr, Arc<Built>) {
    let mut cfg = MudConfig::default();
    cfg.world.db = None;
    cfg.supervisor.typing_scale = 0.0;
    let mut built = build(&cfg).unwrap();
    if let Some(t) = admin_token {
        built.mud = Arc::new(web::Mud {
            admin_token: t.to_owned(),
            ..(*built.mud).clone()
        });
    }
    let built = Arc::new(built);
    let mud = Arc::clone(&built.mud);
    let mcp = mud_mcp::router(
        Arc::clone(&built.mud.game),
        Arc::clone(&built.tokens),
        Arc::new(TypingGate(Arc::clone(built.mud.sup.gate()))),
    );
    let router = TestApp::new()
        .config(settings(&cfg).unwrap())
        .routes(web::routes())
        .state_initializer(move |s| s.insert_extension((*mud).clone()))
        .build()
        .into_router()
        .merge(mcp);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum_serve(listener, router).await });
    tokio::time::sleep(Duration::from_millis(30)).await;
    (addr, built)
}

async fn axum_serve(listener: TcpListener, router: autumn_web::reexports::axum::Router) {
    autumn_web::reexports::axum::serve(listener, router)
        .await
        .ok();
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn ws(addr: SocketAddr) -> Ws {
    tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .unwrap()
        .0
}

async fn send(ws: &mut Ws, v: Value) {
    ws.send(Message::Text(v.to_string().into())).await.unwrap();
}

/// Reads lines until one contains `needle`, or fails after 3 s.
async fn until(ws: &mut Ws, needle: &str) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let msg = tokio::time::timeout_at(deadline, ws.next())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {needle:?}"))
            .unwrap()
            .unwrap();
        if let Message::Text(t) = msg {
            let v: Value = serde_json::from_str(&t).unwrap();
            if v["text"].as_str().is_some_and(|s| s.contains(needle)) {
                return v;
            }
        }
    }
}

fn http() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::new()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_terminal_page_serves() {
    let (addr, _) = boot().await;
    let body = http()
        .get(format!("http://{addr}/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("Harrow's Rest"));
    assert!(body.contains("/static/mud/terminal.js"));
    assert!(
        !body.contains("new WebSocket"),
        "no inline script: the CSP forbids it"
    );
    let js = http()
        .get(format!("http://{addr}/static/mud/terminal.js"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        js.headers()["content-type"],
        "text/javascript; charset=utf-8"
    );
    assert!(js.text().await.unwrap().contains("new WebSocket"));
    let ok = http()
        .get(format!("http://{addr}/healthz"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(ok, "ok");
}

#[tokio::test(flavor = "multi_thread")]
async fn two_players_share_a_room_over_websockets() {
    let (addr, _) = boot().await;
    let mut ada = ws(addr).await;
    send(&mut ada, json!({ "login": "kestrel" })).await;
    let refused = until(&mut ada, "taken").await;
    assert_eq!(refused["kind"], "error");
    send(&mut ada, json!({ "login": "ada" })).await;
    until(&mut ada, "Town Square").await;
    let mut bo = ws(addr).await;
    send(&mut bo, json!({ "login": "bo" })).await;
    until(&mut bo, "Town Square").await;
    until(&mut ada, "bo arrives").await;
    send(&mut ada, json!({ "cmd": "say <b>hello</b> bo" })).await;
    let mine = until(&mut ada, "You say").await;
    assert_eq!(mine["kind"], "narrative");
    let theirs = until(&mut bo, "ada says").await;
    assert_eq!(theirs["kind"], "event");
    assert_eq!(
        theirs["text"], "ada says, \"<b>hello</b> bo\"",
        "raw text; the page renders it with textContent"
    );
    send(&mut bo, json!({ "cmd": "dance" })).await;
    assert_eq!(until(&mut bo, "unknown verb").await["kind"], "error");
    drop(bo);
    until(&mut ada, "bo fades from view").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_observer_needs_the_token_and_shows_private_goals() {
    let (addr, built) = boot().await;
    let token = built.mud.admin_token.clone();
    let denied = http()
        .get(format!("http://{addr}/observer"))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    let wrong = http()
        .get(format!("http://{addr}/observer/entities?token=nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);
    let page = http()
        .get(format!("http://{addr}/observer?token={token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let frag = http()
        .get(format!("http://{addr}/observer/entities?token={token}"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(frag.contains("Torin"));
    assert!(frag.contains("out of ore"), "the private goal shows");
    assert!(
        frag.contains("turing"),
        "the observer knows who is an agent"
    );
    let tick: Value = http()
        .post(format!(
            "http://{addr}/observer/macro-tick/torin?token={token}"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(tick["decision"], "queued");
}

/// The value of the first `attr="…"` in `html`, with `&amp;` undone.
fn attr(html: &str, attr: &str) -> String {
    let start = html.find(&format!("{attr}=\"")).unwrap() + attr.len() + 2;
    let len = html[start..].find('"').unwrap();
    html[start..start + len].replace("&amp;", "&")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_observer_links_carry_a_token_with_url_special_characters() {
    let token = "abcdef0123+xyz&a=b#frag%2B/ é";
    let (addr, _) = boot_with(Some(token)).await;
    let encoded: String = token
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    let page = http()
        .get(format!("http://{addr}/observer?token={encoded}"))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let page = page.text().await.unwrap();
    let entities = attr(&page, "hx-get");
    let frag = http()
        .get(format!("http://{addr}{entities}"))
        .send()
        .await
        .unwrap();
    assert_eq!(frag.status(), 200, "the entities link: {entities}");
    let frag = frag.text().await.unwrap();
    let tick = attr(&frag, "hx-post");
    let tick = http()
        .post(format!("http://{addr}{tick}"))
        .send()
        .await
        .unwrap();
    assert_eq!(tick.status(), 200, "the macro-tick link");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_feed_streams_world_events() {
    let (addr, built) = boot().await;
    let token = built.mud.admin_token.clone();
    let mut resp = http()
        .get(format!("http://{addr}/observer/feed?token={token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ada = built.mud.game.login("ada").unwrap();
    built.mud.game.execute(&ada, "say the feed sees this");
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while !seen.contains("the feed sees this") {
        let chunk = tokio::time::timeout_at(deadline, resp.chunk())
            .await
            .expect("feed timed out")
            .unwrap()
            .unwrap();
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(seen.contains("event: world"));
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_is_mounted_behind_bearer_tokens() {
    let (addr, built) = boot().await;
    let resp = http()
        .post(format!("http://{addr}/mcp"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let token = built
        .tokens
        .issue(mud_world::EntityId::from("torin"))
        .unwrap();
    let resp = http()
        .post(format!("http://{addr}/mcp"))
        .bearer_auth(token)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}
