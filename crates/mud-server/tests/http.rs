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
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

/// The password every test character gets.
const PW: &str = "lantern-and-jerkin";

async fn boot() -> (SocketAddr, Arc<Built>) {
    boot_with(None).await
}

/// Boots the server, with `admin_token` as the observer token if given.
async fn boot_with(admin_token: Option<&str>) -> (SocketAddr, Arc<Built>) {
    boot_cfg(admin_token, |_| {}).await
}

/// Boots the server with `tweak` applied to the default config.
async fn boot_cfg(
    admin_token: Option<&str>,
    tweak: impl FnOnce(&mut MudConfig),
) -> (SocketAddr, Arc<Built>) {
    let (addr, built, _) = boot_full(admin_token, tweak).await;
    (addr, built)
}

/// [`boot_cfg`], also giving the server's app state.
async fn boot_full(
    admin_token: Option<&str>,
    tweak: impl FnOnce(&mut MudConfig),
) -> (SocketAddr, Arc<Built>, autumn_web::AppState) {
    boot_edit(admin_token, tweak, |_| {}).await
}

/// [`boot_full`], with `edit` applied to what `build` made before the
/// server takes it.
async fn boot_edit(
    admin_token: Option<&str>,
    tweak: impl FnOnce(&mut MudConfig),
    edit: impl FnOnce(&mut Built),
) -> (SocketAddr, Arc<Built>, autumn_web::AppState) {
    let state = Arc::new(std::sync::Mutex::new(None::<autumn_web::AppState>));
    let seen = Arc::clone(&state);
    let mut cfg = MudConfig::default();
    cfg.world.db = None;
    cfg.supervisor.typing_scale = 0.0;
    cfg.accounts.bcrypt_cost = 4;
    tweak(&mut cfg);
    let mut built = build(&cfg).await.unwrap();
    edit(&mut built);
    if let Some(t) = admin_token {
        built.mud = Arc::new(web::Mud {
            admin_token: t.to_owned(),
            ..(*built.mud).clone()
        });
    }
    let built = Arc::new(built);
    let mud = Arc::clone(&built.mud);
    let mcp = mud_mcp::router_full(
        Arc::clone(&built.mud.game),
        Arc::clone(&built.tokens),
        Arc::new(TypingGate(Arc::clone(built.mud.sup.gate()))),
        Arc::new(mud_server::SupervisorWakes(Arc::clone(&built.mud.sup))),
        mud_mcp::default_config(),
    );
    let router = TestApp::new()
        .config(settings(&cfg).unwrap())
        .routes(web::routes())
        .state_initializer(move |s| {
            mud_server::install(s, &mud);
            *seen.lock().unwrap() = Some(s.clone());
        })
        .build()
        .into_router()
        .merge(mcp);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum_serve(listener, router).await });
    tokio::time::sleep(Duration::from_millis(30)).await;
    let state = state.lock().unwrap().clone().unwrap();
    (addr, built, state)
}

async fn axum_serve(listener: TcpListener, router: autumn_web::reexports::axum::Router) {
    // The peer address reaches handlers as it does under autumn's server.
    autumn_web::reexports::axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
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

/// A socket whose upgrade request carries `headers`.
async fn ws_with(addr: SocketAddr, headers: &[(&'static str, &str)]) -> Ws {
    let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
    for (k, v) in headers {
        req.headers_mut().insert(*k, v.parse().unwrap());
    }
    tokio_tungstenite::connect_async(req).await.unwrap().0
}

/// The next JSON message, or fails after 3 s.
async fn next_json(ws: &mut Ws) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let msg = tokio::time::timeout_at(deadline, ws.next())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for a message"))
            .unwrap()
            .unwrap();
        if let Message::Text(t) = msg {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

/// Reads messages until one has `kind`; returns it.
async fn until_kind(ws: &mut Ws, kind: &str) -> Value {
    loop {
        let v = next_json(ws).await;
        if v["kind"] == kind {
            return v;
        }
    }
}

/// Makes `name` a character over `ws` and waits for the room. Returns
/// the resume token the server issued, if any.
async fn register(ws: &mut Ws, name: &str, remember: bool) -> Option<String> {
    send(ws, json!({ "line": name })).await;
    until(ws, "Choose a password").await;
    send(ws, json!({ "line": PW, "remember": remember })).await;
    until(ws, "Type it again").await;
    send(ws, json!({ "line": PW, "remember": remember })).await;
    let mut token = None;
    loop {
        let v = next_json(ws).await;
        if v["kind"] == "remember" {
            token = v["token"].as_str().map(str::to_owned);
        }
        if v["text"]
            .as_str()
            .is_some_and(|t| t.contains("Town Square"))
        {
            return token;
        }
    }
}

/// Logs `name` in with the test password over `ws`.
async fn sign_in(ws: &mut Ws, name: &str) {
    send(ws, json!({ "line": name })).await;
    until(ws, "Password:").await;
    send(ws, json!({ "line": PW })).await;
    until(ws, "Town Square").await;
}

/// The close code the server sends, waiting up to 3 s.
async fn close_code(ws: &mut Ws) -> Option<u16> {
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(Ok(msg)) = ws.next().await {
            if let Message::Close(frame) = msg {
                return frame.map(|f| u16::from(f.code));
            }
        }
        None
    })
    .await
    .unwrap_or_else(|_| panic!("the server never closed the socket"))
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
    let mut probe = ws(addr).await;
    send(&mut probe, json!({ "login": "kestrel" })).await;
    until(&mut probe, "Password:").await;
    send(&mut probe, json!({ "cmd": PW })).await;
    let refused = until(&mut probe, "Wrong password.").await;
    assert_eq!(refused["kind"], "error");
    let mut ada = ws(addr).await;
    register(&mut ada, "ada", false).await;
    let mut bo = ws(addr).await;
    register(&mut bo, "bo", false).await;
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
    send(&mut ada, json!({ "cmd": "tell bo meet me at the gate" })).await;
    let sent = until(&mut ada, "You tell").await;
    assert_eq!(sent["kind"], "tell", "the sender's echo is a tell line");
    let heard = until(&mut bo, "tells you").await;
    assert_eq!(heard["kind"], "tell", "the terminal colours a tell apart");
    send(&mut bo, json!({ "cmd": "dance" })).await;
    assert_eq!(until(&mut bo, "unknown verb").await["kind"], "error");
    drop(bo);
    until(&mut ada, "bo fades from view").await;
}

/// Waits up to 3 s for the server to close `ws`.
async fn closed(ws: &mut Ws) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(Ok(msg)) = ws.next().await {
            if matches!(msg, Message::Close(_)) {
                break;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the server never closed the socket"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_terminal_that_gives_no_name_is_closed() {
    let (addr, _) = boot_cfg(None, |c| {
        c.server.login_timeout_ms = 100;
        c.server.idle_timeout_ms = 0;
    })
    .await;
    let mut ws = ws(addr).await;
    assert_eq!(until(&mut ws, "Too slow").await["kind"], "error");
    assert_eq!(close_code(&mut ws).await, Some(1000));
}

#[tokio::test(flavor = "multi_thread")]
async fn one_address_gets_its_share_of_terminals() {
    let (addr, built) = boot_cfg(None, |c| c.server.max_connections_per_address = 1).await;
    let mut first = ws(addr).await;
    register(&mut first, "ada", false).await;
    let mut second = ws(addr).await;
    let refused = until(&mut second, "Too many connections from your address").await;
    assert_eq!(refused["kind"], "error");
    assert_eq!(close_code(&mut second).await, Some(1008));
    assert_eq!(built.mud.limiter.open(), 1);
    drop(first);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while built.mud.limiter.open() > 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the slot was never freed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut third = ws(addr).await;
    register(&mut third, "bo", false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_terminal_is_closed_after_login() {
    let (addr, built) = boot_cfg(None, |c| {
        c.server.login_timeout_ms = 0;
        c.server.idle_timeout_ms = 200;
    })
    .await;
    let mut ws = ws(addr).await;
    register(&mut ws, "ada", false).await;
    until(&mut ws, "Idle too long").await;
    closed(&mut ws).await;
    let ada = mud_world::EntityId::new("pc_ada");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while built
        .mud
        .game
        .read(|w| w.entity(&ada).is_some_and(|e| e.online))
    {
        assert!(tokio::time::Instant::now() < deadline, "ada never left");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_terminal_message_closes_the_socket() {
    let (addr, built) = boot().await;
    let mut ws = ws(addr).await;
    register(&mut ws, "ada", false).await;
    let huge = "x".repeat(mud_server::MAX_WS_MESSAGE_BYTES * 4);
    let _ = ws
        .send(Message::Text(
            json!({ "cmd": format!("say {huge}") }).to_string().into(),
        ))
        .await;
    let closed = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match ws.next().await {
                None | Some(Err(_) | Ok(Message::Close(_))) => return true,
                Some(Ok(Message::Text(t))) if t.contains("You say") => return false,
                Some(Ok(_)) => {}
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(closed, "an oversized message must not be executed");
    let said = built
        .mud
        .game
        .events(&mud_world::EntityId::from("pc_ada"), mud_world::Seq(0), 50);
    assert!(!said.to_string().contains("xxxxxxxx"));
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
async fn the_observer_feed_ends_when_shutdown_begins() {
    // Regression: an open feed (like an agent's MCP session) kept a
    // graceful shutdown waiting.
    let (addr, built) = boot().await;
    let token = built.mud.admin_token.clone();
    let mut resp = http()
        .get(format!("http://{addr}/observer/feed?token={token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    built.mud.shutdown.cancel();
    let ended = tokio::time::timeout(Duration::from_secs(3), async {
        while let Ok(Some(_)) = resp.chunk().await {}
    })
    .await;
    assert!(ended.is_ok(), "the feed closed");
}

#[tokio::test(flavor = "multi_thread")]
async fn littleguy_pages_need_the_token_and_a_littleguy() {
    let (addr, built) = boot().await;
    let anon = http()
        .get(format!("http://{addr}/observer/agents/torin/littleguy"))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);
    let token = &built.mud.admin_token;
    // The default config scripts every agent: nobody has littleguy pages.
    let scripted = http()
        .get(format!(
            "http://{addr}/observer/agents/torin/littleguy?token={token}"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(scripted.status(), 404);
}

#[tokio::test(flavor = "multi_thread")]
async fn autumn_starting_its_shutdown_cancels_ours() {
    let (_addr, built, state) = boot_full(None, |_| {}).await;
    assert!(!built.mud.shutdown.is_cancelled());
    state.trigger_shutdown_for_test();
    tokio::time::timeout(Duration::from_secs(3), built.mud.shutdown.cancelled())
        .await
        .unwrap();
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

#[tokio::test(flavor = "multi_thread")]
async fn a_null_resume_gets_the_name_prompt() {
    let (addr, _) = boot().await;
    let mut c = ws(addr).await;
    send(&mut c, json!({ "resume": null })).await;
    assert_eq!(
        next_json(&mut c).await,
        json!({ "kind": "prompt", "text": "What is your name?" })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_remembered_browser_resumes_without_a_password() {
    let (addr, _) = boot().await;
    let mut first = ws(addr).await;
    let token = register(&mut first, "ada", true).await.unwrap();
    drop(first);
    for _ in 0..2 {
        let mut again = ws(addr).await;
        send(&mut again, json!({ "resume": token })).await;
        assert_eq!(until_kind(&mut again, "authed").await["name"], "ada");
        until(&mut again, "Town Square").await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_remembered_browser_resumes_while_its_address_is_held() {
    let (addr, _) = boot_cfg(None, |c| {
        c.accounts.throttle.address.free = 0;
        c.accounts.throttle.name.free = 0;
    })
    .await;
    let mut first = ws(addr).await;
    let token = register(&mut first, "ada", true).await.unwrap();
    drop(first);
    let mut probe = ws(addr).await;
    send(&mut probe, json!({ "line": "kestrel" })).await;
    until(&mut probe, "Password:").await;
    send(&mut probe, json!({ "line": "wrong-pass-123" })).await;
    until(&mut probe, "Wrong password.").await;
    let mut held = ws(addr).await;
    send(&mut held, json!({ "line": "bo" })).await;
    until(&mut held, "Too many failed logins from your address").await;
    let mut back = ws(addr).await;
    send(&mut back, json!({ "resume": token })).await;
    until(&mut back, "Town Square").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_token_is_forgotten_by_the_client() {
    let (addr, _) = boot().await;
    let mut c = ws(addr).await;
    send(&mut c, json!({ "resume": "no-such-series:no-such-token" })).await;
    let mut seen = Vec::new();
    for _ in 0..3 {
        seen.push(next_json(&mut c).await);
    }
    assert!(seen.contains(&json!({ "kind": "forget" })), "{seen:?}");
    assert!(seen.contains(&json!({ "kind": "error", "text": "Your saved login has expired." })));
    assert!(seen.contains(&json!({ "kind": "prompt", "text": "What is your name?" })));
}

#[tokio::test(flavor = "multi_thread")]
async fn password_prompts_are_marked_secret() {
    let (addr, _) = boot().await;
    let mut c = ws(addr).await;
    send(&mut c, json!({ "line": "ada" })).await;
    assert_eq!(
        next_json(&mut c).await,
        json!({ "kind": "prompt", "secret": "new-password",
                "text": "Nobody here is called ada. Choose a password to make a new character (8 or more characters):" })
    );
    let mut k = ws(addr).await;
    send(&mut k, json!({ "line": "kestrel" })).await;
    assert_eq!(
        next_json(&mut k).await,
        json!({ "kind": "prompt", "text": "Password:", "secret": "current-password" })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_password_sent_as_cmd_is_never_executed() {
    let (addr, built) = boot().await;
    let mut first = ws(addr).await;
    register(&mut first, "ada", false).await;
    drop(first);
    let mut audit = built.mud.game.subscribe_audit();
    let mut old = ws(addr).await;
    send(&mut old, json!({ "login": "ada" })).await;
    until(&mut old, "Password:").await;
    send(&mut old, json!({ "cmd": "say hunter2" })).await;
    until(&mut old, "Wrong password.").await;
    let heard = built
        .mud
        .game
        .events(&mud_world::EntityId::from("pc_ada"), mud_world::Seq(0), 100)
        .to_string();
    assert!(!heard.contains("hunter2"), "{heard}");
    while let Ok(a) = audit.try_recv() {
        assert!(!serde_json::to_string(&*a).unwrap().contains("hunter2"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deliberate_close_is_code_1000_and_a_refusal_is_1008() {
    let (addr, _) = boot_cfg(None, |c| c.server.max_connections_per_address = 1).await;
    let mut c = ws(addr).await;
    register(&mut c, "ada", false).await;
    let mut refused = ws(addr).await;
    assert_eq!(close_code(&mut refused).await, Some(1008), "the limiter");
    send(&mut c, json!({ "line": "quit" })).await;
    until(&mut c, "Farewell.").await;
    assert_eq!(close_code(&mut c).await, Some(1000));
    let mut wrong = ws(addr).await;
    send(&mut wrong, json!({ "line": "kestrel" })).await;
    for _ in 0..3 {
        send(&mut wrong, json!({ "line": "wrong-pass-123" })).await;
    }
    until(&mut wrong, "Wrong password. Farewell.").await;
    assert_eq!(close_code(&mut wrong).await, Some(1008));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cross_site_origin_is_refused() {
    let (addr, _) = boot().await;
    let mut evil = ws_with(addr, &[("origin", "https://evil.example")]).await;
    assert_eq!(
        next_json(&mut evil).await,
        json!({ "kind": "error", "text": "This terminal only accepts connections from its own page." })
    );
    assert_eq!(close_code(&mut evil).await, Some(1008));
    let own = format!("http://{addr}");
    let mut ok = ws_with(addr, &[("origin", own.as_str())]).await;
    send(&mut ok, json!({})).await;
    until(&mut ok, "What is your name?").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn accounts_persist_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("world.db");
    let (addr, built) = boot_cfg(None, |c| c.world.db = Some(db.clone())).await;
    let mut c = ws(addr).await;
    register(&mut c, "ada", false).await;
    drop(c);
    drop(built);
    assert!(dir.path().join("accounts.db").exists());
    let (addr, _) = boot_cfg(None, |c| c.world.db = Some(db.clone())).await;
    let mut back = ws(addr).await;
    sign_in(&mut back, "ada").await;
    let mut wrong = ws(addr).await;
    send(&mut wrong, json!({ "line": "ada" })).await;
    until(&mut wrong, "Password:").await;
    send(&mut wrong, json!({ "line": "not-the-password" })).await;
    until(&mut wrong, "Wrong password.").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_passwords_from_one_address_are_held() {
    let (addr, _) = boot().await;
    for _ in 0..2 {
        let mut c = ws(addr).await;
        send(&mut c, json!({ "line": "kestrel" })).await;
        for _ in 0..3 {
            send(&mut c, json!({ "line": "wrong-pass-123" })).await;
        }
        until(&mut c, "Wrong password. Farewell.").await;
    }
    let mut c = ws(addr).await;
    send(&mut c, json!({ "line": "kestrel" })).await;
    let held = until(&mut c, "Too many failed logins for that name").await;
    assert_eq!(held["kind"], "error");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forwarded_header_never_lifts_the_limits_of_a_direct_client() {
    // No trusted proxies: a client-supplied header names nobody, so the
    // TCP peer is the key whatever the header says.
    let (addr, _) = boot_cfg(None, |c| c.accounts.new_characters_per_address_per_day = 1).await;
    let mut first = ws_with(addr, &[("x-forwarded-for", "203.0.113.9")]).await;
    register(&mut first, "ada", false).await;
    for fwd in [
        ("x-forwarded-for", "198.51.100.1"),
        ("forwarded", "for=198.51.100.2"),
        ("x-real-ip", "198.51.100.3"),
    ] {
        let mut c = ws_with(addr, &[fwd]).await;
        send(&mut c, json!({ "line": "bo" })).await;
        until(&mut c, "Too many new characters from your address today").await;
    }
    let mut direct = ws(addr).await;
    send(&mut direct, json!({ "line": "cy" })).await;
    until(
        &mut direct,
        "Too many new characters from your address today",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forwarded_header_never_lifts_the_address_hold() {
    let (addr, _) = boot_cfg(None, |c| {
        c.accounts.throttle.address.free = 0;
    })
    .await;
    let fwd = [("x-forwarded-for", "203.0.113.9")];
    let mut c = ws_with(addr, &fwd).await;
    send(&mut c, json!({ "line": "kestrel" })).await;
    until(&mut c, "Password:").await;
    send(&mut c, json!({ "line": "wrong-pass-123" })).await;
    until(&mut c, "Wrong password.").await;
    let mut again = ws_with(addr, &[("x-forwarded-for", "198.51.100.7")]).await;
    send(&mut again, json!({ "line": "nobody" })).await;
    until(&mut again, "Too many failed logins from your address").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_observer_counts_logins_with_the_token_only() {
    let (addr, built) = boot().await;
    let mut c = ws(addr).await;
    register(&mut c, "ada", false).await;
    let token = built.mud.admin_token.clone();
    let page = http()
        .get(format!("http://{addr}/observer/entities?token={token}"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        page.contains("Logins: 1 ok · 0 failed · 0 held · 1 new · 0 resumed"),
        "{page}"
    );
    assert!(!page.contains("127.0.0.1"));
    let denied = http()
        .get(format!("http://{addr}/observer/entities"))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_booked_wake_survives_a_restart_and_the_observer_shows_it_escaped() {
    use mud_mcp::WakeBooker as _;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("world.db");
    let torin = mud_world::EntityId::from("torin");
    let note = "<b>is the ore in?</b>";
    let first = {
        let db = db.clone();
        let (_, built) = boot_cfg(None, move |c| c.world.db = Some(db)).await;
        let wakes = mud_server::SupervisorWakes(Arc::clone(&built.mud.sup));
        let r = wakes.wake_me(&torin, 30, note);
        assert_eq!(r["ok"], true, "{r}");
        r["booked"]["id"].as_u64().unwrap()
    };
    // A second server on the same file: the booking came back from it.
    let (addr, built) = boot_cfg(None, move |c| c.world.db = Some(db)).await;
    let pending = built.mud.sup.wakes().pending(&torin);
    assert_eq!(pending.len(), 1);
    assert_eq!((pending[0].id, pending[0].note.as_str()), (first, note));
    let token = built.mud.admin_token.clone();
    let page = http()
        .get(format!("http://{addr}/observer/entities?token={token}"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        page.contains("booked: &lt;b&gt;is the ore in?&lt;/b&gt;"),
        "the note shows, escaped: {page}"
    );
    assert!(!page.contains(note), "never as markup");
    let next = mud_server::SupervisorWakes(Arc::clone(&built.mud.sup)).wake_me(&torin, 5, "x");
    assert!(
        next["booked"]["id"].as_u64().unwrap() > first,
        "ids never repeat"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_stay_signed_in_box_follows_remember_days() {
    for (days, shown) in [(30, true), (0, false)] {
        let (addr, _) = boot_cfg(None, |c| c.accounts.remember_days = days).await;
        let page = http()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(
            page.contains("stay signed in on this browser"),
            shown,
            "{days}"
        );
        assert!(page.contains("Type logout to make this browser forget you."));
    }
}

/// Waits up to `secs` for a message whose text holds `needle`.
async fn until_within(ws: &mut Ws, needle: &str, secs: u64) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
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

#[tokio::test(flavor = "multi_thread")]
async fn a_signed_out_or_locked_browser_is_closed_at_revalidation() {
    let (addr, built) = boot_cfg(None, |c| c.accounts.revalidate_secs = 5).await;
    let accounts = &built.mud.accounts;
    let mut ada = ws(addr).await;
    register(&mut ada, "ada", false).await;
    let mut bo = ws(addr).await;
    register(&mut bo, "bo", false).await;
    let now = built.mud.game.now_ms();
    let key = |n| mud_accounts::NameKey::parse(n).unwrap();
    accounts.admin_signout(&key("ada"), now + 1).await.unwrap();
    accounts
        .admin_set_locked(&key("bo"), true, now + 1)
        .await
        .unwrap();
    for c in [&mut ada, &mut bo] {
        let bye = until_within(c, "Your account changed elsewhere. Farewell.", 8).await;
        assert_eq!(bye["kind"], "system");
        assert_eq!(close_code(c).await, Some(1008));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_feed_opens_with_what_the_boot_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("world.db");
    let torin = mud_world::EntityId::from("torin");
    let note = "zq-private reminder";
    {
        let db = db.clone();
        let (_, built) = boot_cfg(None, move |c| c.world.db = Some(db)).await;
        // Booked three days ago for a minute later: far too late now.
        let then = built.mud.game.now_ms() - 3 * 86_400_000;
        built.mud.sup.wakes().book(&torin, then, 1, note).unwrap();
    }
    let (addr, built) = boot_cfg(None, move |c| c.world.db = Some(db)).await;
    assert!(built.mud.sup.wakes().pending(&torin).is_empty(), "dropped");
    let token = built.mud.admin_token.clone();
    let mut resp = http()
        .get(format!("http://{addr}/observer/feed?token={token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while !seen.contains("booking_dropped") {
        let chunk = tokio::time::timeout_at(deadline, resp.chunk())
            .await
            .expect("the boot report never came")
            .unwrap()
            .unwrap();
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(seen.contains("\"entity\":\"torin\""), "{seen}");
    assert!(
        !seen.contains("zq-private"),
        "the note stays out of the feed"
    );
}

/// A littleguy-like mind: its op outlives the wake and charges its spend
/// to the sink once read, after the wake's line.
#[derive(Debug, Default)]
struct SpendsLate {
    in_flight: mud_supervisor::mind::InFlight,
}

impl mud_supervisor::mind::Mind for SpendsLate {
    fn kind(&self) -> &'static str {
        "spends-late"
    }

    fn wake(
        &self,
        ctx: mud_supervisor::mind::WakeContext,
        tools: mud_supervisor::mind::MudTools,
    ) -> mud_supervisor::mind::BoxFuture<
        '_,
        Result<mud_supervisor::mind::MindReply, mud_supervisor::mind::MindError>,
    > {
        let guard = self.in_flight.start(&ctx.entity);
        let (sink, who) = (Arc::clone(tools.usage_sink()), ctx.entity);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            sink.add(
                &who,
                mud_supervisor::mind::Usage {
                    calls: 3,
                    input_tokens: 41_000,
                    output_tokens: 900,
                    cost_usd_micros: 0,
                },
            );
            drop(guard);
        });
        Box::pin(std::future::pending())
    }

    fn settled(&self, entity: &mud_world::EntityId) -> mud_supervisor::mind::BoxFuture<'_, ()> {
        self.in_flight.settled(entity)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_feed_shows_a_timed_out_wakes_late_spend() {
    // ADR 0014: the spend of an op read after its wake timed out reaches
    // the observer page as a `late` line, not as "0 tok".
    let torin = mud_world::EntityId::from("torin");
    let who = torin.clone();
    let (addr, built, _) = boot_edit(
        None,
        |_| {},
        move |b| {
            use mud_supervisor::runtime::{Supervisor, SupervisorConfig};
            let game = Arc::clone(&b.mud.game);
            let pack = mud_world::WorldPack::crypt_v01().unwrap();
            let mind: Arc<dyn mud_supervisor::mind::Mind> = Arc::new(SpendsLate::default());
            let sup = Supervisor::new(
                Arc::clone(&game),
                mud_supervisor::gate::Gate::new(Arc::clone(&game), 5, 0.0),
                std::collections::HashMap::from([(who, mind)]),
                Arc::new(mud_supervisor::scripted::FallbackMind::from_cast(
                    &pack.cast,
                )),
                SupervisorConfig {
                    wake_timeout_ms: 50,
                    first_macro_tick_secs: [3_600, 3_600],
                    ..SupervisorConfig::default()
                },
            );
            b.mud = Arc::new(web::Mud {
                sup,
                ..(*b.mud).clone()
            });
        },
    )
    .await;
    let token = built.mud.admin_token.clone();
    let js = http()
        .get(format!("http://{addr}/static/mud/observer.js"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        js.contains("m.kind==='late_spend'") && js.contains("tok after its wake"),
        "the page formats the line"
    );
    let mut resp = http()
        .get(format!("http://{addr}/observer/feed?token={token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // The feed is open: a wake that times out and charges late.
    let sup = Arc::clone(&built.mud.sup);
    let tick = sup.fire_macro_tick(&torin);
    assert!(
        matches!(tick, mud_supervisor::router::RouteNote::Queued { .. }),
        "{tick:?}"
    );
    for h in sup.step() {
        h.await.unwrap();
    }
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !seen.contains("late_spend") {
        let chunk = tokio::time::timeout_at(deadline, resp.chunk())
            .await
            .expect("the late spend never reached the feed")
            .unwrap()
            .unwrap();
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    let line = seen
        .split("\n\n")
        .find(|e| e.contains("late_spend"))
        .unwrap();
    assert!(line.contains("event: wake"), "the wake category: {line}");
    assert!(line.contains("\"entity\":\"torin\""), "{line}");
    assert!(line.contains("\"input_tokens\":41000"), "{line}");
    assert!(line.contains("\"output_tokens\":900"), "{line}");
    let done = seen.find("wake_finished").expect("the wake's own line");
    assert!(
        done < seen.find("late_spend").unwrap(),
        "after the wake's line"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn booking_events_are_escaped() {
    use mud_mcp::WakeBooker as _;
    let (addr, built) = boot().await;
    let token = built.mud.admin_token.clone();
    let js = http()
        .get(format!("http://{addr}/static/mud/observer.js"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        js.contains("k==='booking'") && js.contains("'booking','steward'].forEach"),
        "the page listens for booking lines"
    );
    assert!(
        js.contains("d.textContent=") && !js.contains("innerHTML"),
        "rows are text, never markup"
    );
    let mut resp = http()
        .get(format!("http://{addr}/observer/feed?token={token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let torin = mud_world::EntityId::from("torin");
    let wakes = mud_server::SupervisorWakes(Arc::clone(&built.mud.sup));
    let note = "<script>alert('zq-note')</script>";
    let r = wakes.wake_me(&torin, 30, note);
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["booked"]["untrusted_text"], note, "data for the agent");
    let id = r["booked"]["id"].as_u64().unwrap();
    assert_eq!(wakes.cancel_wake(&torin, id)["ok"], true);
    let guard = mud_world::EntityId::from("guard_01");
    assert_eq!(wakes.wake_me(&guard, 5, "x")["error_code"], "no_wakes");
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while !seen.contains("\"action\":\"cancelled\"") {
        let chunk = tokio::time::timeout_at(deadline, resp.chunk())
            .await
            .expect("the booking lines never came")
            .unwrap()
            .unwrap();
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(seen.contains("event: booking"), "{seen}");
    assert!(seen.contains("\"action\":\"booked\""), "{seen}");
    assert!(seen.contains("\"entity\":\"torin\""), "{seen}");
    assert!(!seen.contains("zq-note"), "the note stays out of the feed");
    assert!(!seen.contains("<script"), "no markup in the feed: {seen}");
    assert!(
        !seen.contains("guard_01"),
        "a refused booking is no booking"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn steward_events_are_escaped() {
    let (addr, built) = boot().await;
    let token = built.mud.admin_token.clone();
    let js = http()
        .get(format!("http://{addr}/static/mud/observer.js"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        js.contains("k==='steward'") && js.contains("'steward'].forEach"),
        "the page listens for steward lines"
    );
    assert!(js.contains("d.textContent=") && !js.contains("innerHTML"));
    let mut resp = http()
        .get(format!("http://{addr}/observer/feed?token={token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let title = "<script>alert('zq-goal')</script> & co";
    built
        .mud
        .sup
        .publish_steward(mud_supervisor::steward::StewardEvent {
            entity: mud_world::EntityId::from("torin"),
            item: "goal".into(),
            id: Some(4),
            action: "declined".into(),
            code: "declined_council".into(),
            votes: Some([1, 2]),
            text: Some(title.into()),
            reasons: vec!["<b>no</b>".into()],
        });
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while !seen.contains("declined_council") {
        let chunk = tokio::time::timeout_at(deadline, resp.chunk())
            .await
            .expect("the steward line never came")
            .unwrap()
            .unwrap();
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert!(seen.contains("event: steward"), "{seen}");
    assert!(!seen.contains('<'), "no markup on the wire: {seen}");
    let data = seen
        .lines()
        .skip_while(|l| *l != "event: steward")
        .find_map(|l| l.strip_prefix("data: "))
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(data).unwrap();
    assert_eq!(v["text"], title, "the same text to a JSON reader");
    assert_eq!(v["kind"], "steward");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_autonomy_panel_escapes_goal_titles() {
    let (addr, built) = boot().await;
    let token = built.mud.admin_token.clone();
    let torin = mud_world::EntityId::from("torin");
    built.mud.sup.set_autonomy(
        &torin,
        mud_supervisor::steward::AutonomyView {
            active: vec![mud_supervisor::steward::GoalView {
                id: 4,
                title: "<script>alert(1)</script>".into(),
                why: "x".into(),
            }],
            proposed: vec![mud_supervisor::steward::GoalView {
                id: 5,
                title: "<img src=x onerror=alert(2)>".into(),
                why: "y".into(),
            }],
            ideas: 2,
            quiet: true,
            intent: Some("accept".into()),
            ..Default::default()
        },
    );
    let html = http()
        .get(format!("http://{addr}/observer/entities?token={token}"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
        "{html}"
    );
    assert!(html.contains("&lt;img src=x"), "{html}");
    assert!(!html.contains("<script>alert") && !html.contains("<img src"));
    assert!(html.contains("quiet (hard budget)") && html.contains("sending: accept"));
    assert!(html.contains("2 ideas"));
}
