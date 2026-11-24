//! The autumn-web app: the terminal, the observer dashboard, and `/mcp`.
//!
//! - `/` is a browser terminal. It speaks JSON over the `/ws` WebSocket:
//!   the client sends `{"login": name}` once, then `{"cmd": line}`; the
//!   server sends `{"kind", "text"}` lines.
//! - `/observer` is the private dashboard: every agent's goal, last wake,
//!   budget and spend, and a live feed of routing decisions, wakes,
//!   self-control rolls and world events. It needs the admin token.
//! - `/mcp` is the MCP endpoint of `mud-mcp`, behind bearer tokens.

use std::convert::Infallible;
use std::fmt::Write as _;
use std::sync::Arc;

use autumn_web::AppState;
use autumn_web::htmx::HTMX_JS_PATH;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::http::StatusCode;
use autumn_web::reexports::axum::response::{IntoResponse, Response};
use autumn_web::sse::{Event as SseEvent, Sse};
use autumn_web::ws::{Message, WebSocket, WsHandler};
use futures::{Stream, StreamExt};
use maud::{DOCTYPE, Markup, PreEscaped};
use mud_supervisor::runtime::Supervisor;
use mud_world::{EntityId, Game};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_stream::wrappers::BroadcastStream;

use crate::config::Timeouts;
use crate::session::{Line, LineKind, Presence, Session};

/// What every route reads: the world, the supervisor, and the admin token.
#[derive(Debug, Clone)]
pub struct Mud {
    /// The world.
    pub game: Arc<Game>,
    /// The supervisor.
    pub sup: Arc<Supervisor>,
    /// Open connections per player.
    pub presence: Arc<Presence>,
    /// The observer's token.
    pub admin_token: String,
    /// How long a terminal may wait for a name, and stay silent after.
    pub timeouts: Timeouts,
}

impl Mud {
    fn admin(&self, given: Option<&str>) -> bool {
        given.is_some_and(|g| constant_eq(g.as_bytes(), self.admin_token.as_bytes()))
    }
}

/// Percent-encodes `s` for a URL query value or path segment: every byte
/// but the RFC 3986 unreserved ones becomes `%XX`.
#[must_use]
pub fn url_escape(s: &str) -> String {
    s.bytes()
        .fold(String::with_capacity(s.len()), |mut out, b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                out.push(char::from(b));
            } else {
                let _ = write!(out, "%{b:02X}");
            }
            out
        })
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn mud(state: &AppState) -> Option<Arc<Mud>> {
    state.extension::<Mud>()
}

/// Every route of the app.
#[must_use]
pub fn routes() -> Vec<autumn_web::Route> {
    routes![
        index,
        terminal,
        observer,
        observer_entities,
        observer_feed,
        macro_tick,
        healthz,
        terminal_js,
        observer_js
    ]
}

const STYLE: &str = r"
:root{--bg:#fbfaf7;--fg:#1d1d1b;--dim:#6b6a65;--acc:#7a4b12;--err:#a1261b;--ev:#26507a;--line:#e3e0d8}
@media (prefers-color-scheme:dark){:root{--bg:#141412;--fg:#e6e3da;--dim:#9a978d;--acc:#e0a458;--err:#ef7d6f;--ev:#8ab4e0;--line:#2c2b27}}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.45 ui-monospace,SFMono-Regular,Menlo,monospace}
main{max-width:960px;margin:0 auto;padding:16px}h1{font-size:18px;margin:0 0 12px}
#out{white-space:pre-wrap;border:1px solid var(--line);border-radius:6px;padding:12px;height:70vh;overflow-y:auto}
.narrative{color:var(--fg)}.event{color:var(--ev)}.error{color:var(--err)}.system{color:var(--acc)}
form{display:flex;gap:8px;margin-top:8px}input{flex:1;font:inherit;padding:8px;background:transparent;color:var(--fg);border:1px solid var(--line);border-radius:6px}
button{font:inherit;padding:8px 14px;border-radius:6px;border:1px solid var(--line);background:transparent;color:var(--fg);cursor:pointer}
table{border-collapse:collapse;width:100%;font-size:13px}td,th{border-bottom:1px solid var(--line);padding:6px;text-align:left;vertical-align:top}
.feed{height:45vh;overflow-y:auto;font-size:12px;border:1px solid var(--line);border-radius:6px;padding:8px;white-space:pre-wrap}
.hard{color:var(--err)}.soft{color:var(--acc)}.muted{color:var(--dim)}
@media (max-width:640px){td:nth-child(n+6),th:nth-child(n+6){display:none}}
";

const TERMINAL_JS: &str = r"
const out=document.getElementById('out'),form=document.getElementById('f'),input=document.getElementById('i');
let ws=null,named=false;
function add(kind,text){const d=document.createElement('div');d.className=kind;d.textContent=text;out.appendChild(d);out.scrollTop=out.scrollHeight;}
function connect(){
  const proto=location.protocol==='https:'?'wss':'ws';
  ws=new WebSocket(proto+'://'+location.host+'/ws');
  ws.onmessage=e=>{const m=JSON.parse(e.data);add(m.kind,m.text);if(m.kind!=='error')named=true;};
  ws.onclose=()=>{add('system','[disconnected — reload to reconnect]');};
  ws.onopen=()=>add('system','What is your name?');
}
form.addEventListener('submit',e=>{e.preventDefault();const v=input.value;input.value='';if(!ws||ws.readyState!==1)return;
  if(!named){ws.send(JSON.stringify({login:v}));}else{add('muted','> '+v);ws.send(JSON.stringify({cmd:v}));}});
connect();
";

const OBSERVER_JS: &str = r"
const feed=document.getElementById('feed'),token=new URLSearchParams(location.search).get('token');
const es=new EventSource('/observer/feed?token='+encodeURIComponent(token));
const t=()=>new Date().toLocaleTimeString();
function row(cls,text){const d=document.createElement('div');d.className=cls;d.textContent=t()+'  '+text;feed.prepend(d);while(feed.childNodes.length>400)feed.lastChild.remove();}
function fmt(k,m){
  if(k==='route')return m.decision==='queued'?`route   ${m.entity}  queued ${m.priority} (event #${m.seq})`:`route   ${m.entity}  dropped: ${m.why} (event #${m.seq})`;
  if(k==='wake'){
    if(m.kind==='wake_started')return `wake    ${m.entity}  ${m.priority} via ${m.mind}: ${m.reason}`;
    if(m.kind==='wake_finished'){const u=m.usage||{};return `done    ${m.entity}  ${m.ok?'ok':'FAILED'} · ${(u.input_tokens||0)+(u.output_tokens||0)} tok · budget ${m.budget}${m.note?' · '+m.note:''}`;}
    return `typing  ${m.entity}  line dropped: combat broke out`;
  }
  if(k==='roll')return `roll    ${m.entity}  ${m.trait_name}(${m.roll.target_number}) rolled ${m.roll.total} → ${m.nudged?'FAILED, nudged':'resisted'}`;
  if(k==='session')return m.kind==='afk'?`afk     ${m.entity}  until ${new Date(m.until_ms).toLocaleTimeString()}`:`session ${m.entity}  ${m.online?'logged in':'logged out'}`;
  return `world   [${m.room||'-'}] ${m.who}${m.human?' (human)':''}: ${m.text}`;
}
['route','wake','roll','world','session'].forEach(k=>es.addEventListener(k,e=>row(k==='roll'?'soft':(k==='world'?'muted':''),fmt(k,JSON.parse(e.data)))));
";

/// The page scripts are files, not inline: the app's CSP allows
/// `script-src 'self'` only.
fn js(body: &'static str) -> Response {
    (
        [
            ("content-type", "text/javascript; charset=utf-8"),
            ("cache-control", "no-cache"),
        ],
        body,
    )
        .into_response()
}

#[get("/static/mud/terminal.js")]
async fn terminal_js() -> Response {
    js(TERMINAL_JS)
}

#[get("/static/mud/observer.js")]
async fn observer_js() -> Response {
    js(OBSERVER_JS)
}

fn page(title: &str, body: &Markup, script: &str) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                style { (PreEscaped(STYLE)) }
                script src=(HTMX_JS_PATH) {}
            }
            body { main { (body) } script src=(script) {} }
        }
    }
}

#[get("/")]
async fn index() -> Markup {
    page(
        "Harrow's Rest",
        &html! {
            h1 { "Harrow's Rest" }
            div #out {}
            form #f autocomplete="off" {
                input #i autofocus placeholder="type a command — help for the list";
                button type="submit" { "Send" }
            }
            p.muted { "Same commands as everyone else in the world. Some of the people you meet may not be people." }
        },
        "/static/mud/terminal.js",
    )
}

#[derive(Deserialize)]
struct ClientMsg {
    login: Option<String>,
    cmd: Option<String>,
}

fn encode(line: &Line) -> Message {
    Message::Text(serde_json::to_string(line).unwrap_or_default().into())
}

async fn send_lines(socket: &mut WebSocket, lines: &[Line]) -> bool {
    for l in lines {
        if socket.send(encode(l)).await.is_err() {
            return false;
        }
    }
    true
}

#[ws("/ws")]
async fn terminal(state: AppState) -> impl WsHandler {
    let app = mud(&state);
    move |mut socket: WebSocket| async move {
        let Some(app) = app else { return };
        // Log in first, in time.
        let session = loop {
            let Some(incoming) = Timeouts::within(app.timeouts.login, socket.recv()).await else {
                let l = Line {
                    kind: LineKind::Error,
                    text: "Too slow. Farewell.".to_owned(),
                };
                let _ = socket.send(encode(&l)).await;
                return;
            };
            let Some(Ok(Message::Text(t))) = incoming else {
                return;
            };
            let name = serde_json::from_str::<ClientMsg>(&t)
                .ok()
                .and_then(|m| m.login)
                .unwrap_or_default();
            match Session::login(Arc::clone(&app.game), Arc::clone(&app.presence), &name) {
                Ok((s, greet)) => {
                    if !send_lines(&mut socket, &greet).await {
                        return;
                    }
                    break s;
                }
                Err(e) => {
                    let l = Line {
                        kind: LineKind::Error,
                        text: format!("{e}. What is your name?"),
                    };
                    if socket.send(encode(&l)).await.is_err() {
                        return;
                    }
                }
            }
        };
        let mut events = app.game.subscribe();
        loop {
            tokio::select! {
                incoming = Timeouts::within(app.timeouts.idle, socket.recv()) => {
                    let Some(incoming) = incoming else {
                        let l = Line {
                            kind: LineKind::Error,
                            text: "Idle too long. Farewell.".to_owned(),
                        };
                        let _ = socket.send(encode(&l)).await;
                        break;
                    };
                    let line = match incoming {
                        Some(Ok(Message::Text(t))) => serde_json::from_str::<ClientMsg>(&t).ok().and_then(|m| m.cmd).unwrap_or_default(),
                        Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                        Some(Ok(_)) => continue,
                    };
                    if !send_lines(&mut socket, &session.command(&line)).await {
                        break;
                    }
                }
                ev = events.recv() => {
                    match ev {
                        Ok(ev) => {
                            if let Some(l) = session.render(&ev)
                                && socket.send(encode(&l)).await.is_err()
                            {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
        drop(session);
    }
}

#[derive(Deserialize)]
struct Admin {
    token: Option<String>,
}

fn forbidden() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        "the observer needs ?token=<admin token>",
    )
        .into_response()
}

#[get("/observer")]
async fn observer(State(state): State<AppState>, Query(q): Query<Admin>) -> Response {
    let Some(app) = mud(&state).filter(|m| m.admin(q.token.as_deref())) else {
        return forbidden();
    };
    let token = url_escape(&app.admin_token);
    page(
        "MUD observer",
        &html! {
            h1 { "Observer — private" }
            p.muted { "Agent goals, wake triggers, self-control rolls and token burn. Never show this to players." }
            div hx-get=(format!("/observer/entities?token={token}")) hx-trigger="load, every 2s" {}
            h1 { "Live feed" }
            div #feed .feed {}
        },
        "/static/mud/observer.js",
    )
    .into_response()
}

fn budget_class(level: &Value) -> &'static str {
    match level.as_str() {
        Some("hard") => "hard",
        Some("soft") => "soft",
        _ => "",
    }
}

#[get("/observer/entities")]
async fn observer_entities(State(state): State<AppState>, Query(q): Query<Admin>) -> Response {
    let Some(app) = mud(&state).filter(|m| m.admin(q.token.as_deref())) else {
        return forbidden();
    };
    let token = url_escape(&app.admin_token);
    let snap = app.sup.snapshot();
    let tokens = snap["tokens_today"].as_u64().unwrap_or(0);
    let cost = snap["cost_usd_micros_today"].as_u64().unwrap_or(0);
    #[expect(clippy::cast_precision_loss, reason = "display only")]
    let dollars = cost as f64 / 1_000_000.0;
    html! {
        p { "Tokens today: " strong { (tokens) } " · cost: $" (format!("{dollars:.4}")) }
        table {
            tr { th { "entity" } th { "tier / mind" } th { "where" } th { "budget" } th { "last wake" } th { "goal" } th { "macro" } th { "state" } }
            @for e in snap["entities"].as_array().into_iter().flatten() {
                tr {
                    td { strong { (e["name"].as_str().unwrap_or("")) } br; span.muted { (e["id"].as_str().unwrap_or("")) } }
                    td { (e["tier"].as_str().unwrap_or("")) " / " (e["mind"].as_str().unwrap_or("")) }
                    td { (e["room"].as_str().unwrap_or("")) br; span.muted { "hp " (e["hp"]) " · " (e["currency"]) "s" } }
                    td class=(budget_class(&e["budget"]["level"])) {
                        (e["budget"]["level"].as_str().unwrap_or("")) br;
                        span.muted { (e["budget"]["tokens"]) " / " (e["budget"]["limit"]) }
                    }
                    td {
                        @if let Some(w) = e["last_wake"].as_object() {
                            (w.get("priority").and_then(Value::as_str).unwrap_or("")) br;
                            span.muted { (w.get("reason").and_then(Value::as_str).unwrap_or("")) }
                        } @else { span.muted { "never" } }
                    }
                    td { (e["goal"].as_str().unwrap_or("")) }
                    td { (e["macro_ticks"]) br;
                        form hx-post=(format!("/observer/macro-tick/{}?token={token}", url_escape(e["id"].as_str().unwrap_or("")))) hx-swap="none" {
                            button type="submit" { "tick" }
                        }
                    }
                    td {
                        @if e["online"] == true { "online" } @else { span.muted { "offline" } }
                        @if e["typing"] == true { br; "typing…" }
                        @if !e["afk_until_ms"].is_null() { br; span.soft { "afk" } }
                        @if !e["pending"].is_null() { br; span.muted { "wake pending" } }
                    }
                }
            }
        }
    }
    .into_response()
}

#[post("/observer/macro-tick/{id}")]
async fn macro_tick(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<Admin>,
) -> Response {
    let Some(app) = mud(&state).filter(|m| m.admin(q.token.as_deref())) else {
        return forbidden();
    };
    let note = app.sup.fire_macro_tick(&EntityId::new(id));
    Json(note).into_response()
}

fn sse(name: &str, data: &Value) -> SseEvent {
    SseEvent::default().event(name).data(data.to_string())
}

/// The observer feed: supervisor decisions, self-control rolls and every
/// world event, as server-sent events.
fn feed(app: &Mud) -> impl Stream<Item = Result<SseEvent, Infallible>> + Send + 'static {
    use mud_supervisor::runtime::ObserverEvent;
    use mud_world::Audit;
    let sup = BroadcastStream::new(app.sup.subscribe()).filter_map(|r| async move {
        let e = r.ok()?;
        let name = match &*e {
            ObserverEvent::Route(_) => "route",
            ObserverEvent::WakeStarted { .. }
            | ObserverEvent::WakeFinished { .. }
            | ObserverEvent::Interrupted { .. } => "wake",
            ObserverEvent::Session { .. } | ObserverEvent::Afk { .. } => "session",
        };
        Some(sse(name, &serde_json::to_value(&*e).ok()?))
    });
    let audit = BroadcastStream::new(app.game.subscribe_audit()).filter_map(|r| async move {
        match &*r.ok()? {
            a @ Audit::SelfControl { .. } => Some(sse("roll", &serde_json::to_value(a).ok()?)),
            Audit::Command { .. } => None,
        }
    });
    let world = BroadcastStream::new(app.game.subscribe()).filter_map(|r| async move {
        let e = r.ok()?;
        let who = e.actor.as_ref().map_or("world", |a| a.name.as_str()).to_owned();
        let text = e.untrusted_text.clone().map_or_else(|| e.narrative.clone(), |t| format!("{}: {t}", e.narrative));
        Some(sse("world", &json!({ "seq": e.seq, "room": e.room, "kind": e.kind, "who": who, "human": e.human_actor, "text": text })))
    });
    futures::stream::select(sup, futures::stream::select(audit, world)).map(Ok::<_, Infallible>)
}

#[get("/observer/feed")]
async fn observer_feed(State(state): State<AppState>, Query(q): Query<Admin>) -> Response {
    let Some(app) = mud(&state).filter(|m| m.admin(q.token.as_deref())) else {
        return forbidden();
    };
    Sse::new(feed(&app))
        .keep_alive(autumn_web::sse::keep_alive())
        .into_response()
}

#[get("/healthz")]
async fn healthz() -> &'static str {
    "ok"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_comparison() {
        assert!(constant_eq(b"abc", b"abc"));
        assert!(!constant_eq(b"abc", b"abd"));
        assert!(!constant_eq(b"abc", b"abcd"));
    }

    #[test]
    fn url_escape_keeps_unreserved_bytes_only() {
        assert_eq!(url_escape("aZ09-._~"), "aZ09-._~");
        assert_eq!(url_escape("a+b&c=d#e%f/ g"), "a%2Bb%26c%3Dd%23e%25f%2F%20g");
        assert_eq!(url_escape("é"), "%C3%A9");
    }
}
