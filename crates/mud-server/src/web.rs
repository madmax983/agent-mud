//! The autumn-web app: the terminal, the observer dashboard, and `/mcp`.
//!
//! - `/` is a browser terminal. It speaks JSON over the `/ws` WebSocket:
//!   the client sends `{"resume": token}` or `{"line": text, "remember":
//!   bool}`; the server sends [`ServerMsg`]s (`{"kind", ...}`). The login
//!   dialogue ([`crate::login`]) runs first; a resume token is an explicit
//!   message, never a cookie.
//! - `/observer` is the private dashboard: every agent's goal, last wake,
//!   budget and spend, and a live feed of routing decisions, wakes,
//!   self-control rolls and world events. It needs the admin token.
//! - `/mcp` is the MCP endpoint of `mud-mcp`, behind bearer tokens.

use std::convert::Infallible;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;

use autumn_web::AppState;
use autumn_web::htmx::HTMX_JS_PATH;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::Extension;
use autumn_web::reexports::axum::extract::ConnectInfo;
use autumn_web::reexports::axum::http::{HeaderMap, StatusCode, header};
use autumn_web::reexports::axum::response::{IntoResponse, Response};
use autumn_web::security::ResolvedClientIdentity;
use autumn_web::sse::{Event as SseEvent, Sse};
use autumn_web::ws::{CloseFrame, Message, WebSocket, WsHandler};
use futures::{Stream, StreamExt};
use maud::{DOCTYPE, Markup, PreEscaped};
use mud_accounts::{Accounts, Secret};
use mud_supervisor::runtime::Supervisor;
use mud_world::{EntityId, Game};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_stream::wrappers::BroadcastStream;

use crate::config::Timeouts;
use crate::limits::{ConnLimiter, account_peer, client_addr};
use crate::login::{Dialogue, Next};
use crate::session::{CloseKind, Control, Line, LineKind, Presence, Session, Transport};

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
    /// Connection slots, shared with telnet.
    pub limiter: Arc<ConnLimiter>,
    /// Cancelled when the server starts shutting down: long-lived
    /// responses (the observer feed, MCP sessions) end on it so the server
    /// can drain.
    pub shutdown: tokio_util::sync::CancellationToken,
    /// The littleguy minds, for the observer's links to their pages.
    pub littleguy: Vec<Arc<mud_supervisor::littleguy::LittleguyMind>>,
    /// The Steward, when it runs (ADR 0015).
    pub steward: Option<Arc<mud_supervisor::steward::Steward>>,
    /// The world's scripts, unless `[scripts] enabled = false`.
    pub scripts: Option<Arc<mud_script::ScriptHost>>,
    /// Player accounts. Only the transports hold it; the script engine
    /// never does.
    pub accounts: Arc<Accounts>,
    /// `[server] public_url`, for the terminal's Origin check.
    pub public_url: Option<String>,
    /// Whether `[server] trusted_proxies` is set.
    pub trusted_proxies: bool,
    /// Every terminal session, web and telnet. Each ends when `shutdown`
    /// is cancelled, and the stop waits for them ([`crate::shut_down`]):
    /// a session's last command and its logout reach the store before
    /// the store closes.
    pub sessions: tokio_util::task::TaskTracker,
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
        observer_littleguy,
        observer_scripts,
        scripts_reload,
        scripts_memory_reset,
        macro_tick,
        healthz,
        terminal_js,
        observer_js
    ]
}

const STYLE: &str = r"
:root{--bg:#fbfaf7;--fg:#1d1d1b;--dim:#6b6a65;--acc:#7a4b12;--err:#a1261b;--ev:#26507a;--tell:#8a2f8f;--line:#e3e0d8}
@media (prefers-color-scheme:dark){:root{--bg:#141412;--fg:#e6e3da;--dim:#9a978d;--acc:#e0a458;--err:#ef7d6f;--ev:#8ab4e0;--tell:#d79ad9;--line:#2c2b27}}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.45 ui-monospace,SFMono-Regular,Menlo,monospace}
main{max-width:960px;margin:0 auto;padding:16px}h1{font-size:18px;margin:0 0 12px}
#out{white-space:pre-wrap;border:1px solid var(--line);border-radius:6px;padding:12px;height:70vh;overflow-y:auto}
.narrative{color:var(--fg)}.event{color:var(--ev)}.tell{color:var(--tell)}.error{color:var(--err)}.system{color:var(--acc)}
form{display:flex;gap:8px;margin-top:8px}input{flex:1;font:inherit;padding:8px;background:transparent;color:var(--fg);border:1px solid var(--line);border-radius:6px}
button{font:inherit;padding:8px 14px;border-radius:6px;border:1px solid var(--line);background:transparent;color:var(--fg);cursor:pointer}
table{border-collapse:collapse;width:100%;font-size:13px}td,th{border-bottom:1px solid var(--line);padding:6px;text-align:left;vertical-align:top}
.feed{height:45vh;overflow-y:auto;font-size:12px;border:1px solid var(--line);border-radius:6px;padding:8px;white-space:pre-wrap}
.hard{color:var(--err)}.soft{color:var(--acc)}.muted{color:var(--dim)}
@media (max-width:640px){td:nth-child(n+6),th:nth-child(n+6){display:none}}
";

const TERMINAL_JS: &str = r"
const out=document.getElementById('out'),form=document.getElementById('f'),input=document.getElementById('i'),keep=document.getElementById('keep'),keepBox=document.getElementById('keepbox');
let ws=null,authed=false,secret=false,tries=0;
function get(s,k){try{return s.getItem(k)||'';}catch(e){return '';}}
function put(s,k,v){try{s.setItem(k,v);}catch(e){}}
function drop(s,k){try{s.removeItem(k);}catch(e){}}
if(keep){keep.checked=get(localStorage,'mud.keep')==='1';keep.addEventListener('change',()=>put(localStorage,'mud.keep',keep.checked?'1':'0'));}
function showKeep(on){if(keepBox)keepBox.style.display=on?'':'none';}
showKeep(false);
function add(kind,text){const d=document.createElement('div');d.className=kind;d.textContent=text;out.appendChild(d);out.scrollTop=out.scrollHeight;}
function onMsg(m){
  if(m.kind==='prompt'){add('system',m.text);secret=!!m.secret;input.type=secret?'password':'text';input.autocomplete=m.secret||(authed?'off':'username');showKeep(secret&&!authed);input.focus();return;}
  if(m.kind==='authed'){authed=true;tries=0;showKeep(false);input.type='text';input.autocomplete='off';return;}
  if(m.kind==='remember'){if(m.persist)put(localStorage,'mud.device',m.token);else put(sessionStorage,'mud.tab',m.token);return;}
  if(m.kind==='forget'){drop(localStorage,'mud.device');drop(sessionStorage,'mud.tab');return;}
  add(m.kind,m.text);
}
function connect(){
  const proto=location.protocol==='https:'?'wss':'ws';
  ws=new WebSocket(proto+'://'+location.host+'/ws');
  ws.onopen=()=>ws.send(JSON.stringify({resume:get(localStorage,'mud.device')||get(sessionStorage,'mud.tab')||''}));
  ws.onmessage=e=>onMsg(JSON.parse(e.data));
  ws.onclose=e=>{
    secret=false;input.type='text';
    if(e.code===1000){add('system','[disconnected \u2014 reload to play again]');return;}
    if(e.code===1008){add('system','[disconnected]');return;}
    if(tries>=10){add('system','[disconnected \u2014 reload to play again]');return;}
    add('system','[connection lost \u2014 reconnecting\u2026]');
    const wait=[1,2,4,8,16][tries]||30;tries++;authed=false;
    setTimeout(connect,wait*1000);
  };
}
form.addEventListener('submit',e=>{e.preventDefault();const v=input.value;input.value='';if(!ws||ws.readyState!==1)return;
  if(!secret)add('muted','> '+v);
  ws.send(JSON.stringify({line:v,remember:!!(keep&&keep.checked)}));
  secret=false;input.type='text';});
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
    if(m.kind==='late_spend'){const u=m.usage||{};return `late    ${m.entity}  +${(u.input_tokens||0)+(u.output_tokens||0)} tok after its wake · budget ${m.budget}`;}
    if(m.kind==='booking_dropped')return `booking ${m.entity}  #${m.id} dropped at boot: due ${new Date(m.due_ms).toLocaleString()}, too late`;
    if(m.kind==='plan_abandoned')return `plan    ${m.entity}  abandoned at boot after ${m.cursor}/${m.len} lines: too old to resume`;
    if(m.kind==='interrupted')return `typing  ${m.entity}  line dropped: combat broke out`;
    return `wake    ${m.entity}  ${m.kind}`;
  }
  if(k==='roll')return `roll    ${m.entity}  ${m.trait_name}(${m.roll.target_number}) rolled ${m.roll.total} → ${m.nudged?'FAILED, nudged':'resisted'}`;
  if(k==='script')return `script  ${m.text}`;
  if(k==='booking')return `booking ${m.entity}  #${m.id} ${m.action} · due ${new Date(m.due_ms).toLocaleString()}`;
  if(k==='steward'){const v=m.votes?` · votes ${m.votes[0]} yes, ${m.votes[1]} no`:'';const r=(m.reasons||[]).length?` · ${m.reasons.join(' | ')}`:'';return `steward ${m.entity}  ${m.item}${m.id!=null?' #'+m.id:''} ${m.action} (${m.code})${v}${m.text?' · '+m.text:''}${r}`;}
  if(k==='notice'){const n=m.notice||{};return `NOTICE  ${n.entity}  ${m.mind} ${n.code} · ${n.text}`;}
  if(k==='session')return m.kind==='afk'?`afk     ${m.entity}  until ${new Date(m.until_ms).toLocaleTimeString()}`:`session ${m.entity}  ${m.online?'logged in':'logged out'}`;
  return `world   [${m.room||'-'}] ${m.who}${m.human?' (human)':''}: ${m.text}`;
}
function cls(k,m){if(k==='roll')return 'soft';if(k==='notice')return 'error';if(k==='world')return m.kind==='tell'?'tell':'muted';return '';}
['route','wake','roll','world','session','script','notice','booking','steward'].forEach(k=>es.addEventListener(k,e=>{const m=JSON.parse(e.data);row(cls(k,m),fmt(k,m));}));
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
async fn index(State(state): State<AppState>) -> Markup {
    let remember = mud(&state).is_some_and(|m| m.accounts.config().remember_days > 0);
    page(
        "Harrow's Rest",
        &html! {
            h1 { "Harrow's Rest" }
            div #out {}
            form #f autocomplete="off" {
                input #i autofocus placeholder="type a command — help for the list";
                button type="submit" { "Send" }
            }
            @if remember {
                p #keepbox { label { input #keep type="checkbox"; " stay signed in on this browser" } }
            }
            p.muted { "Same commands as everyone else in the world. Some of the people you meet may not be people. Type logout to make this browser forget you." }
        },
        "/static/mud/terminal.js",
    )
}

/// What a terminal sends. It has no `Debug`: it may hold a password or a
/// token.
#[derive(Deserialize, Default)]
struct ClientMsg {
    /// A typed line: a name, a password, or a command. `login` and `cmd`
    /// are the v0.1 names.
    #[serde(default, alias = "login", alias = "cmd")]
    line: Option<String>,
    /// "Stay signed in" was ticked.
    #[serde(default)]
    remember: bool,
    /// A resume token, on the first message only.
    #[serde(default)]
    resume: Option<String>,
}

/// What the server sends a terminal.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerMsg<'a> {
    /// The answer to the player's own command.
    Narrative {
        /// The text.
        text: &'a str,
    },
    /// Something that happened around them.
    Event {
        /// The text.
        text: &'a str,
    },
    /// A private message to or from them.
    Tell {
        /// The text.
        text: &'a str,
    },
    /// A refusal.
    Error {
        /// The text.
        text: &'a str,
    },
    /// A server notice.
    System {
        /// The text.
        text: &'a str,
    },
    /// A question; `secret` marks one whose answer must not echo.
    Prompt {
        /// The text.
        text: &'a str,
        /// `current-password` or `new-password`.
        #[serde(skip_serializing_if = "Option::is_none")]
        secret: Option<&'static str>,
    },
    /// The login is done.
    Authed {
        /// The character's name.
        name: &'a str,
    },
    /// Keep this resume token.
    Remember {
        /// `series:token`.
        token: &'a str,
        /// `localStorage` ("stay signed in") or `sessionStorage` (a tab).
        persist: bool,
    },
    /// Forget any resume token.
    Forget,
}

impl<'a> ServerMsg<'a> {
    /// The message of a line.
    #[must_use]
    pub const fn from_line(l: &'a Line) -> Self {
        let text = l.text.as_str();
        match l.kind {
            LineKind::Narrative => Self::Narrative { text },
            LineKind::Event => Self::Event { text },
            LineKind::Tell => Self::Tell { text },
            LineKind::Error => Self::Error { text },
            LineKind::System => Self::System { text },
            LineKind::Prompt => Self::Prompt { text, secret: None },
            LineKind::Secret(s) => Self::Prompt {
                text,
                secret: Some(s.as_str()),
            },
        }
    }

    /// The message of a control.
    #[must_use]
    pub fn from_control(c: &'a Control) -> Self {
        match c {
            Control::Authed { name } => Self::Authed { name },
            Control::Remember(t) => Self::Remember {
                token: t.value.expose(),
                persist: t.persist,
            },
            Control::Forget => Self::Forget,
        }
    }
}

fn encode(m: &ServerMsg<'_>) -> Message {
    Message::Text(serde_json::to_string(m).unwrap_or_default().into())
}

fn error_msg(text: &str) -> Message {
    encode(&ServerMsg::Error { text })
}

async fn send_lines(socket: &mut WebSocket, lines: &[Line]) -> bool {
    for l in lines {
        if socket.send(encode(&ServerMsg::from_line(l))).await.is_err() {
            return false;
        }
    }
    true
}

async fn send_all(socket: &mut WebSocket, lines: &[Line], controls: &[Control]) -> bool {
    if !send_lines(socket, lines).await {
        return false;
    }
    for c in controls {
        if socket
            .send(encode(&ServerMsg::from_control(c)))
            .await
            .is_err()
        {
            return false;
        }
    }
    true
}

async fn close(socket: &mut WebSocket, kind: CloseKind) {
    let code = match kind {
        CloseKind::Normal => 1000,
        CloseKind::Policy => 1008,
    };
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: "".into(),
        })))
        .await;
}

/// Whether a terminal connection may come from `origin`: no `Origin`
/// header (a non-browser client), or this server's own page, by its
/// `Host` or its `public_url`.
#[must_use]
pub fn origin_allowed(origin: Option<&str>, host: Option<&str>, public_url: Option<&str>) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    if let Some(h) = host
        && (origin == format!("http://{h}") || origin == format!("https://{h}"))
    {
        return true;
    }
    public_url.is_some_and(|u| {
        let u = u.trim_end_matches('/');
        let base = u
            .find("://")
            .and_then(|i| u[i + 3..].find('/').map(|j| &u[..i + 3 + j]))
            .unwrap_or(u);
        origin == base
    })
}

/// The first message: a resume token, a name, or nothing yet.
async fn first_reply(d: &mut Dialogue, m: ClientMsg) -> crate::login::Reply {
    match (m.resume.filter(|r| !r.is_empty()), m.line) {
        (Some(token), _) => d.resume(Secret::new(token)).await,
        (None, Some(line)) => d.answer(Secret::new(line), m.remember).await,
        (None, None) => d.greeting(),
    }
}

#[ws("/ws")]
async fn terminal(
    state: AppState,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    resolved: Option<Extension<ResolvedClientIdentity>>,
    headers: HeaderMap,
) -> impl WsHandler {
    let app = mud(&state);
    let resolved = resolved.and_then(|Extension(r)| r.addr);
    let peer = peer.map(|Extension(ConnectInfo(p))| p);
    let addr = client_addr(resolved, peer);
    let text = |h| {
        headers
            .get(h)
            .and_then(|v: &header::HeaderValue| v.to_str().ok())
    };
    let origin_ok = origin_allowed(
        text(header::ORIGIN),
        text(header::HOST),
        app.as_ref().and_then(|a| a.public_url.as_deref()),
    );
    let forwarded = ["x-forwarded-for", "forwarded", "x-real-ip"]
        .iter()
        .any(|h| headers.contains_key(*h));
    let who = account_peer(
        resolved,
        peer,
        forwarded,
        app.as_ref().is_some_and(|a| a.trusted_proxies),
    );
    // Taken at upgrade time; the socket closure owns it until it ends.
    let slot = app.as_ref().map(|a| a.limiter.try_acquire(addr));
    move |mut socket: WebSocket| async move {
        let Some(app) = app else { return };
        // Counted until the socket ends, for the stop to wait on.
        let _live = app.sessions.token();
        let stop = app.shutdown.clone();
        let stopped = tokio::select! {
            () = connect(&app, &mut socket, slot, origin_ok, who) => false,
            () = stop.cancelled() => true,
        };
        if stopped {
            let _ = socket
                .send(error_msg("The MUD is shutting down. Farewell."))
                .await;
            close(&mut socket, CloseKind::Normal).await;
        }
    }
}

/// One terminal socket: the slot and origin checks, the login, then play.
async fn connect(
    app: &Mud,
    socket: &mut WebSocket,
    slot: Option<Result<crate::limits::ConnSlot, crate::limits::Refusal>>,
    origin_ok: bool,
    who: mud_accounts::Peer,
) {
    let _slot = match slot {
        Some(Ok(s)) => s,
        Some(Err(refusal)) => {
            let _ = socket.send(error_msg(&refusal.to_string())).await;
            close(socket, CloseKind::Policy).await;
            return;
        }
        None => return,
    };
    if !origin_ok {
        let _ = socket
            .send(error_msg(
                "This terminal only accepts connections from its own page.",
            ))
            .await;
        close(socket, CloseKind::Policy).await;
        return;
    }
    let Some(auth) = login(app, socket, who).await else {
        return;
    };
    let (session, greet) = match Session::login(
        Arc::clone(&app.game),
        Arc::clone(&app.presence),
        Arc::clone(&app.accounts),
        auth,
        who,
        Transport::Web,
    ) {
        Ok(s) => s,
        Err(e) => {
            let _ = socket.send(error_msg(&format!("{e}. Farewell."))).await;
            close(socket, CloseKind::Policy).await;
            return;
        }
    };
    if send_lines(socket, &greet).await {
        play(app, socket, session).await;
    }
}

/// The login dialogue over a socket, within the total login deadline.
async fn login(
    app: &Mud,
    socket: &mut WebSocket,
    who: mud_accounts::Peer,
) -> Option<mud_accounts::Authenticated> {
    let deadline = app.timeouts.login_deadline();
    let mut d = Dialogue::new(
        Arc::clone(&app.game),
        Arc::clone(&app.accounts),
        Transport::Web,
        who,
    );
    let mut first = true;
    loop {
        let incoming = Timeouts::until(deadline, socket.recv()).await;
        let Some(incoming) = incoming else {
            let _ = socket.send(error_msg("Too slow. Farewell.")).await;
            close(socket, CloseKind::Normal).await;
            return None;
        };
        let m = match incoming {
            Some(Ok(Message::Text(t))) => serde_json::from_str::<ClientMsg>(&t).unwrap_or_default(),
            Some(Ok(Message::Close(_)) | Err(_)) | None => return None,
            Some(Ok(_)) => continue,
        };
        let reply = if std::mem::take(&mut first) {
            Timeouts::until(deadline, first_reply(&mut d, m)).await
        } else if let Some(line) = m.line {
            Timeouts::until(deadline, d.answer(Secret::new(line), m.remember)).await
        } else {
            continue;
        };
        let Some(reply) = reply else {
            let _ = socket.send(error_msg("Too slow. Farewell.")).await;
            close(socket, CloseKind::Normal).await;
            return None;
        };
        if !send_all(socket, &reply.lines, &reply.controls).await {
            return None;
        }
        match reply.next {
            Next::Ask => {}
            Next::Close(kind) => {
                close(socket, kind).await;
                return None;
            }
            Next::Done(auth) => return Some(*auth),
        }
    }
}

/// A logged-in terminal: lines in, events out, and a revalidation tick.
async fn play(app: &Mud, socket: &mut WebSocket, mut session: Session) {
    let mut events = app.game.subscribe();
    let every =
        std::time::Duration::from_secs(u64::from(app.accounts.config().revalidate_secs.max(1)));
    let mut recheck = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
    loop {
        tokio::select! {
            incoming = Timeouts::within(app.timeouts.idle, socket.recv()) => {
                let Some(incoming) = incoming else {
                    let _ = socket.send(error_msg("Idle too long. Farewell.")).await;
                    close(socket, CloseKind::Normal).await;
                    break;
                };
                let line = match incoming {
                    Some(Ok(Message::Text(t))) => serde_json::from_str::<ClientMsg>(&t).ok().and_then(|m| m.line).unwrap_or_default(),
                    Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                    Some(Ok(_)) => continue,
                };
                let out = session.input(Secret::new(line)).await;
                if !send_all(socket, &out.lines, &out.controls).await {
                    break;
                }
                if let Some(kind) = out.close {
                    close(socket, kind).await;
                    break;
                }
            }
            ev = events.recv() => {
                match ev {
                    Ok(ev) => {
                        if let Some(l) = session.render(&ev)
                            && socket.send(encode(&ServerMsg::from_line(&l))).await.is_err()
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = recheck.tick() => match session.revalidate().await {
                Ok(true) => {}
                Ok(false) => {
                    let msg = ServerMsg::System { text: "Your account changed elsewhere. Farewell." };
                    let _ = socket.send(encode(&msg)).await;
                    close(socket, CloseKind::Policy).await;
                    break;
                }
                Err(e) => tracing::warn!(target: "mud::auth", error = %e, "a session could not be revalidated"),
            },
        }
    }
    drop(session);
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
            h1 { "Scripts" }
            div hx-get=(format!("/observer/scripts?token={token}")) hx-trigger="load, every 5s" {}
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
    let logins = app.accounts.stats();
    html! {
        p { "Logins: " (logins.ok) " ok · " (logins.failed) " failed · " (logins.held) " held · " (logins.created) " new · " (logins.resumed) " resumed" }
        p { "Tokens today: " strong { (tokens) } " · cost: $" (format!("{dollars:.4}")) }
        table {
            tr { th { "entity" } th { "tier / mind" } th { "where" } th { "budget" } th { "last wake" } th { "goal" } th { "autonomy" } th { "macro" } th { "state" } }
            @for e in snap["entities"].as_array().into_iter().flatten() {
                tr {
                    td { strong { (e["name"].as_str().unwrap_or("")) } br; span.muted { (e["id"].as_str().unwrap_or("")) } }
                    td { (e["tier"].as_str().unwrap_or("")) " / " (e["mind"].as_str().unwrap_or(""))
                        @if e["mind"] == "littleguy" {
                            br; a href=(format!("/observer/agents/{}/littleguy?token={token}", url_escape(e["id"].as_str().unwrap_or("")))) target="_blank" rel="noopener" { "littleguy ↗" }
                        }
                    }
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
                    td { (autonomy_cell(&e["autonomy"])) }
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
                        @for b in e["booked"].as_array().into_iter().flatten() {
                            br; span.soft title=(b["note"].as_str().unwrap_or("")) {
                                "booked: " (b["note"].as_str().unwrap_or("").chars().take(40).collect::<String>())
                            }
                        }
                    }
                }
            }
        }
    }
    .into_response()
}

/// The Steward's view of one agent (ADR 0015): its goals as lg lists
/// them, and what the Steward holds open. Goal titles are the agent's own
/// text; maud escapes them. The littleguy link beside it opens lg's goals
/// page, the veto.
fn autonomy_cell(a: &Value) -> Markup {
    let goals = |key: &str| -> Vec<String> {
        a[key]
            .as_array()
            .into_iter()
            .flatten()
            .map(|g| {
                format!(
                    "#{} {}",
                    g["id"],
                    g["title"]
                        .as_str()
                        .unwrap_or("")
                        .chars()
                        .take(60)
                        .collect::<String>()
                )
            })
            .collect()
    };
    html! {
        @if a.is_object() {
            @for g in goals("active") { span { "active " (g) } br; }
            @for g in goals("proposed") { span.muted { "proposed " (g) } br; }
            span.muted {
                (a["ideas"].as_u64().unwrap_or(0)) " ideas"
                @if let Some(n) = a["notes"]["count"].as_u64() { " · " (n) " notes" }
            }
            @if a["quiet"] == true { br; span.hard { "quiet (hard budget)" } }
            @if let Some(i) = a["intent"].as_str() { br; span.soft { "sending: " (i) } }
            @for off in a["off"].as_array().into_iter().flatten() { br; span.muted { "off: " (off.as_str().unwrap_or("")) } }
            @if a["overlay"].is_object() { (overlay_lines(&a["overlay"])) }
        } @else {
            span.muted { "—" }
        }
    }
}

/// The Steward's overlays of one agent (ADR 0015): lg's active version,
/// the pause, the active free-text lines (ADR 0016), what lg's Roll back
/// would do, and the Steward's own versions with the text each holds,
/// newest first. The text is escaped by maud.
/// lg's own Roll back button, on the page the littleguy link opens, is
/// the veto.
fn overlay_lines(o: &Value) -> Markup {
    let version = |v: &Value| {
        v.as_u64()
            .map_or_else(|| "none".to_owned(), |n| format!("v{n}"))
    };
    html! {
        br; span { "refine (" (o["mode"].as_str().unwrap_or("menu")) "): lg runs " (version(&o["pointer"])) }
        @if let Some(src) = o["source"].as_str() { span.muted { " · " (src) } }
        @if let Some(code) = o["paused"].as_str() { br; span.hard { "refine paused: " (code) } }
        // Free text (ADR 0016): model lines a unanimous council approved,
        // in the agent's instructions now. Escaped by maud.
        @let lines = o["lines"].as_array().map(Vec::as_slice).unwrap_or_default();
        @if !lines.is_empty() {
            br; span.soft { "free-text habits in its instructions (ADR 0016):" }
            @for l in lines { br; span { "- " (l.as_str().unwrap_or("")) } }
        }
        @if let Some(b) = o["baseline"].as_u64() {
            br;
            // lg's Roll back reverses lg's last move, which is not always
            // a move off the baseline.
            @match o["rollback_to"].as_u64() {
                Some(to) if to == b => {
                    span.muted { "Roll back on the littleguy page returns to the empty baseline (version " (b) ")." }
                }
                Some(to) => {
                    span.soft { "Roll back on the littleguy page brings back version " (to) ", not the empty baseline (version " (b) ")." }
                }
                None => {
                    span.muted { "The empty baseline is version " (b) " on the littleguy refine tab." }
                }
            }
        }
        @for c in o["chain"].as_array().into_iter().flatten() {
            br;
            @let line = format!("v{} {}: {}", c["version"], c["label"].as_str().unwrap_or(""), c["text"].as_str().unwrap_or(""));
            @if c["active"] == true { span { (line) " (active)" } } @else { span.muted { (line) } }
        }
    }
}

/// Opens an agent's littleguy pages (goals, notes, skills, memory,
/// history): a fresh one-time sign-in link from its daemon. The pages
/// listen on this machine's loopback, so the browser must run here too.
#[get("/observer/agents/{id}/littleguy")]
async fn observer_littleguy(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<Admin>,
) -> Response {
    let Some(app) = mud(&state).filter(|m| m.admin(q.token.as_deref())) else {
        return forbidden();
    };
    let entity = EntityId::new(id);
    let Some(mind) = app.littleguy.iter().find(|m| m.serves(&entity)) else {
        return (StatusCode::NOT_FOUND, "no littleguy plays that entity").into_response();
    };
    match mind.ui_link(&entity).await {
        Ok(link) => (
            StatusCode::SEE_OTHER,
            [(autumn_web::reexports::axum::http::header::LOCATION, link)],
        )
            .into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    }
}

/// The Scripts panel: the version, each hook's breaker, the mind scripts
/// with their memory, Reload and Reset memory. Everything is rendered by
/// maud, which escapes it.
#[get("/observer/scripts")]
async fn observer_scripts(State(state): State<AppState>, Query(q): Query<Admin>) -> Response {
    let Some(app) = mud(&state).filter(|m| m.admin(q.token.as_deref())) else {
        return forbidden();
    };
    let token = url_escape(&app.admin_token);
    let Some(host) = &app.scripts else {
        return html! { p.muted { "Scripts are off ([scripts] enabled = false)." } }
            .into_response();
    };
    let status = app.game.read(mud_world::World::hook_status);
    let now = app.game.now_ms();
    // The running totals in the state, not the ledger's rows: a read that
    // costs the same however long the world has run.
    let (pools, ledger) = app
        .game
        .read(|w| (w.pool_facts(now), w.state().minted.clone()));
    let compiled = host.current();
    let max = compiled.limits.max_consecutive_faults;
    html! {
        p { "Version " strong { (compiled.version) } " · " (compiled.summary())
            " "
            form style="display:inline" hx-post=(format!("/observer/scripts/reload?token={token}")) hx-swap="none" {
                button type="submit" { "Reload" }
            }
            @if !host.reloadable() {
                span.muted { " (bundled: Reload re-arms these scripts and reopens their breakers)" }
            }
            span.muted { " A refused reload shows its problems in the feed." }
        }
        table {
            tr { th { "hook" } th { "faults" } th { "state" } }
            @for key in host.hook_keys() {
                @let s = status.iter().find(|s| s.key == key);
                tr {
                    td { (key) }
                    td { (s.map_or(0, |s| s.faults)) "/" (max) }
                    td {
                        @if s.is_some_and(|s| s.tripped) { span.hard { "TRIPPED" } } @else { "OK" }
                    }
                }
            }
            @for id in host.minds() {
                @let faults = host.mind_faults(&id);
                tr {
                    td { "minds." (id) ".on_wake" }
                    td { (faults) "/" (max) }
                    td {
                        @if mud_world::hooks::hook_allowed(faults, max) { "OK " } @else { span.hard { "TRIPPED" } " (runs its TOML plan) " }
                        form style="display:inline" hx-post=(format!("/observer/scripts/memory/{}/reset?token={token}", url_escape(id.as_str()))) hx-swap="none" {
                            button type="submit" { "Reset memory" }
                        }
                    }
                }
            }
        }
        @if !pools.is_empty() {
            table {
                tr { th { "reward pool" } th { "left this period" } th { "ledger" } }
                @for (id, left) in &pools {
                    @let total = ledger.get(id).copied().unwrap_or_default();
                    tr {
                        td { (id) " (per " (left.per.name()) ")" }
                        td {
                            "currency " (left.currency)
                            @for (proto, n) in &left.items { ", " (proto) " " (n) }
                        }
                        td { (total.mints) " mints: currency " (total.currency) ", " (total.items) " items" }
                    }
                }
            }
        }
    }
    .into_response()
}

/// Reloads the scripts from the pack folder, or re-arms the bundled ones.
/// 200 with the new version, 422 with every problem (the old scripts keep
/// running, and the feed shows the problems), 409 when scripts are off.
#[post("/observer/scripts/reload")]
async fn scripts_reload(State(state): State<AppState>, Query(q): Query<Admin>) -> Response {
    let Some(app) = mud(&state).filter(|m| m.admin(q.token.as_deref())) else {
        return forbidden();
    };
    match crate::reload_scripts(&app).await {
        Ok(ok) => Json(json!({
            "ok": true,
            "version": ok.version,
            "previous": ok.previous,
            "files": ok.files,
        }))
        .into_response(),
        Err(crate::ReloadError::Invalid { version, problems }) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "ok": false, "version": version, "problems": problems })),
        )
            .into_response(),
        Err(crate::ReloadError::Disabled) => (
            StatusCode::CONFLICT,
            "scripts are disabled ([scripts] enabled = false)",
        )
            .into_response(),
    }
}

/// Clears one mind's script memory.
#[post("/observer/scripts/memory/{id}/reset")]
async fn scripts_memory_reset(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<Admin>,
) -> Response {
    let Some(app) = mud(&state).filter(|m| m.admin(q.token.as_deref())) else {
        return forbidden();
    };
    let entity = EntityId::new(id);
    if !app.scripts.as_ref().is_some_and(|h| h.has_mind(&entity)) {
        return (StatusCode::NOT_FOUND, "no mind script for that entity").into_response();
    }
    app.game.reset_script_memory(&entity);
    Json(json!({ "ok": true, "entity": entity })).into_response()
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

/// One feed line. The JSON carries agent and player text (goal titles,
/// speech) as strings; `<`, `>` and `&` go as JSON escapes, so no markup
/// is ever on the wire, and the page renders each row as text.
fn sse(name: &str, data: &Value) -> SseEvent {
    SseEvent::default().event(name).data(json_text(data))
}

/// `data` as JSON with `<`, `>` and `&` escaped (`\u003c`, …): the same
/// value to a JSON reader.
#[must_use]
pub fn json_text(data: &Value) -> String {
    data.to_string()
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

/// The observer feed: supervisor decisions, self-control rolls and every
/// world event, as server-sent events. It opens with the boot report (the
/// bookings dropped and plans abandoned at boot), which happened before
/// anyone could subscribe.
fn feed(app: &Mud) -> impl Stream<Item = Result<SseEvent, Infallible>> + Send + 'static {
    use mud_supervisor::runtime::ObserverEvent;
    use mud_world::Audit;
    let live = BroadcastStream::new(app.sup.subscribe()).filter_map(|r| async move { r.ok() });
    let sup = futures::stream::iter(app.sup.boot_report())
        .chain(live)
        .filter_map(|e| async move {
            let name = match &*e {
                ObserverEvent::Route(_) => "route",
                ObserverEvent::WakeStarted { .. }
                | ObserverEvent::WakeFinished { .. }
                | ObserverEvent::Interrupted { .. }
                | ObserverEvent::LateSpend { .. }
                | ObserverEvent::BookingDropped { .. }
                | ObserverEvent::PlanAbandoned { .. } => "wake",
                ObserverEvent::Session { .. } | ObserverEvent::Afk { .. } => "session",
                ObserverEvent::Booking { .. } => "booking",
                ObserverEvent::Steward(_) => "steward",
                ObserverEvent::MindNotice { .. } => "notice",
            };
            Some(sse(name, &serde_json::to_value(&*e).ok()?))
        });
    let audit = BroadcastStream::new(app.game.subscribe_audit()).filter_map(|r| async move {
        match &*r.ok()? {
            a @ Audit::SelfControl { .. } => Some(sse("roll", &serde_json::to_value(a).ok()?)),
            Audit::Command { .. } => None,
            a => Some(sse("script", &json!({ "text": script_line(a)? }))),
        }
    });
    let world = BroadcastStream::new(app.game.subscribe()).filter_map(|r| async move {
        let e = r.ok()?;
        let text = observer_text(&e)?;
        let who = e.actor.as_ref().map_or("world", |a| a.name.as_str()).to_owned();
        Some(sse("world", &json!({ "seq": e.seq, "room": e.room, "kind": e.kind, "who": who, "human": e.human_actor, "text": text })))
    });
    futures::stream::select(sup, futures::stream::select(audit, world))
        .map(Ok::<_, Infallible>)
        .take_until(app.shutdown.clone().cancelled_owned())
}

/// One script entry as the observer's feed line. Script logs and fault
/// messages are server or pack text; the observer still renders them with
/// `textContent`.
#[must_use]
pub fn script_line(a: &mud_world::Audit) -> Option<String> {
    use mud_world::Audit;
    Some(match a {
        Audit::ScriptFault {
            hook,
            entity,
            message,
            faults,
            tripped,
            ..
        } => format!(
            "{hook} FAULT {faults}{} ({entity}) · {message}",
            if *tripped { " TRIPPED" } else { "" }
        ),
        Audit::ScriptLog {
            hook, entity, line, ..
        } => format!("{hook} log ({entity}) · {line}"),
        Audit::ScriptEffects {
            hook,
            entity,
            effects,
            ..
        } => format!("{hook} ran ({entity}) · {}", effects.join(", ")),
        Audit::ScriptsLoaded {
            version,
            previous,
            why,
            ..
        } => format!(
            "scripts loaded {version} ({why}; was {})",
            previous.as_deref().unwrap_or("nothing")
        ),
        Audit::ScriptsReloadRefused {
            version, problems, ..
        } => format!(
            "scripts reload refused (still {version}): {} problem{} · {}",
            problems.len(),
            if problems.len() == 1 { "" } else { "s" },
            problems.join(" · ")
        ),
        Audit::Minted {
            hook,
            entity,
            pool,
            currency,
            items,
            ..
        } => {
            let mut what = vec![format!("currency {currency}")];
            what.extend(items.iter().map(ToString::to_string));
            format!("{hook} minted from {pool} ({entity}) · {}", what.join(", "))
        }
        Audit::MindMemoryReset { entity, .. } => format!("memory reset for {entity}"),
        Audit::ComeToRoll {
            entity,
            attr,
            roll,
            hp,
            came_to,
            ..
        } => format!(
            "come-to roll ({entity}) · {}({}) rolled {} at {hp} HP → {}",
            attr.to_ascii_uppercase(),
            roll.target,
            roll.total,
            if *came_to { "comes to" } else { "stays out" }
        ),
        Audit::StoreBacklog { dropped, .. } => format!(
            "STORE FAILING: {dropped} unsaved event{} dropped from the backlog",
            if *dropped == 1 { "" } else { "s" }
        ),
        Audit::EventsArchived {
            trimmed,
            archived,
            through,
            ..
        } => {
            format!("events archived: {archived} written, {trimmed} trimmed, through seq {through}")
        }
        Audit::SelfControl { .. } | Audit::Command { .. } => return None,
    })
}

/// One world event as the observer's feed line, or `None` for an event
/// the feed shows another way: a self-control nudge is a private cue whose
/// roll already appears as a `roll` line, and has no narrative.
///
/// Player text stays a quoted, labelled value after the server's own words
/// (the observer renders it with `textContent`); it is never spliced into
/// the narrative.
fn observer_text(e: &mud_world::Event) -> Option<String> {
    use mud_world::EventKind;
    if e.kind == EventKind::SelfControlNudge {
        return None;
    }
    let who = e.actor.as_ref().map_or("someone", |a| a.name.as_str());
    let Some(t) = e.untrusted_text.as_deref() else {
        return Some(e.narrative.clone());
    };
    Some(match e.kind {
        EventKind::Speech => format!("{who} says: \"{t}\""),
        EventKind::Tell => {
            let to = e.target.as_ref().map_or("someone", |a| a.name.as_str());
            format!("{who} tells {to}: \"{t}\"")
        }
        EventKind::Emote => format!("{who} (emote): {t}"),
        _ => format!("{} \"{t}\"", e.narrative),
    })
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

    fn event(kind: mud_world::EventKind, text: Option<&str>) -> mud_world::Event {
        serde_json::from_value(json!({
            "seq": 1, "at_ms": 0, "room": "town_square", "kind": kind, "tags": [],
            "actor": { "id": "guard_01", "name": "Hale" },
            "target": { "id": "vich", "name": "Vich" },
            "untrusted_text": text, "narrative": "Hale says something.",
            "data": null, "recipients": [], "human_actor": false
        }))
        .unwrap()
    }

    #[test]
    fn the_observer_quotes_speech_and_drops_empty_nudges() {
        use mud_world::EventKind;
        assert_eq!(
            observer_text(&event(EventKind::Speech, Some("Off the crate, now."))).as_deref(),
            Some("Hale says: \"Off the crate, now.\"")
        );
        assert_eq!(
            observer_text(&event(EventKind::Tell, Some("Six silver."))).as_deref(),
            Some("Hale tells Vich: \"Six silver.\"")
        );
        assert_eq!(
            observer_text(&event(EventKind::Enter, None)).as_deref(),
            Some("Hale says something.")
        );
        assert_eq!(
            observer_text(&event(EventKind::SelfControlNudge, None)),
            None
        );
    }

    #[test]
    fn server_messages_serialize_as_documented() {
        use crate::session::Secretness;
        let line = |kind, text: &str| Line::new(kind, text);
        let cases = [
            (
                line(LineKind::Narrative, "a"),
                r#"{"kind":"narrative","text":"a"}"#,
            ),
            (line(LineKind::Event, "b"), r#"{"kind":"event","text":"b"}"#),
            (line(LineKind::Error, "c"), r#"{"kind":"error","text":"c"}"#),
            (line(LineKind::Tell, "t"), r#"{"kind":"tell","text":"t"}"#),
            (
                line(LineKind::System, "d"),
                r#"{"kind":"system","text":"d"}"#,
            ),
            (
                line(LineKind::Prompt, "What is your name?"),
                r#"{"kind":"prompt","text":"What is your name?"}"#,
            ),
            (
                line(LineKind::Secret(Secretness::Current), "Password:"),
                r#"{"kind":"prompt","text":"Password:","secret":"current-password"}"#,
            ),
            (
                line(LineKind::Secret(Secretness::New), "Type it again:"),
                r#"{"kind":"prompt","text":"Type it again:","secret":"new-password"}"#,
            ),
        ];
        for (l, want) in cases {
            assert_eq!(
                serde_json::to_string(&ServerMsg::from_line(&l)).unwrap(),
                want
            );
        }
        let authed = Control::Authed { name: "ada".into() };
        assert_eq!(
            serde_json::to_string(&ServerMsg::from_control(&authed)).unwrap(),
            r#"{"kind":"authed","name":"ada"}"#
        );
        assert_eq!(
            serde_json::to_string(&ServerMsg::from_control(&Control::Forget)).unwrap(),
            r#"{"kind":"forget"}"#
        );
        let r = ServerMsg::Remember {
            token: "s:t",
            persist: true,
        };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"kind":"remember","token":"s:t","persist":true}"#
        );
    }

    #[test]
    fn client_messages_read_every_spelling() {
        let m: ClientMsg = serde_json::from_str(r#"{"login":"ada"}"#).unwrap();
        assert_eq!(m.line.as_deref(), Some("ada"));
        let m: ClientMsg = serde_json::from_str(r#"{"cmd":"look","remember":true}"#).unwrap();
        assert_eq!((m.line.as_deref(), m.remember), (Some("look"), true));
        let m: ClientMsg = serde_json::from_str(r#"{"resume":null}"#).unwrap();
        assert!(m.resume.is_none() && m.line.is_none());
    }

    #[test]
    fn the_terminal_script_never_uses_inner_html() {
        for js in [TERMINAL_JS, OBSERVER_JS] {
            assert!(!js.contains("innerHTML"));
            assert!(!js.contains("outerHTML"));
            assert!(!js.contains("insertAdjacentHTML"));
        }
        assert!(TERMINAL_JS.contains("textContent"));
        assert!(!TERMINAL_JS.contains("cookie"), "no route trusts a cookie");
    }

    #[test]
    fn tells_have_their_own_colour_in_both_themes_and_the_feed() {
        let (light, dark) = STYLE
            .split_once("@media (prefers-color-scheme:dark)")
            .unwrap();
        assert!(light.contains("--tell:") && dark.contains("--tell:"));
        assert!(STYLE.contains(".tell{color:var(--tell)}"));
        assert!(OBSERVER_JS.contains("m.kind==='tell'"));
    }

    #[test]
    fn origin_allowed_cases() {
        assert!(origin_allowed(None, Some("mud.example"), None), "absent");
        assert!(origin_allowed(
            Some("http://127.0.0.1:3000"),
            Some("127.0.0.1:3000"),
            None
        ));
        assert!(origin_allowed(
            Some("https://mud.example"),
            Some("mud.example"),
            None
        ));
        assert!(origin_allowed(
            Some("https://play.example:8443"),
            Some("10.0.0.5:3000"),
            Some("https://play.example:8443/mud/")
        ));
        assert!(origin_allowed(
            Some("https://play.example"),
            None,
            Some("https://play.example")
        ));
        assert!(!origin_allowed(
            Some("https://evil.example"),
            Some("mud.example"),
            None
        ));
        assert!(!origin_allowed(
            Some("https://evil.example"),
            Some("mud.example"),
            Some("https://play.example")
        ));
        assert!(!origin_allowed(Some("null"), None, None));
    }

    #[test]
    fn token_comparison() {
        assert!(constant_eq(b"abc", b"abc"));
        assert!(!constant_eq(b"abc", b"abd"));
        assert!(!constant_eq(b"abc", b"abcd"));
    }

    #[test]
    fn the_store_lines_tell_the_operator_what_happened() {
        let backlog = script_line(&mud_world::Audit::StoreBacklog {
            dropped: 3,
            at_ms: 0,
        })
        .unwrap();
        assert_eq!(
            backlog,
            "STORE FAILING: 3 unsaved events dropped from the backlog"
        );
        let one = script_line(&mud_world::Audit::StoreBacklog {
            dropped: 1,
            at_ms: 0,
        })
        .unwrap();
        assert!(one.contains("1 unsaved event dropped"), "{one}");
        let archived = script_line(&mud_world::Audit::EventsArchived {
            trimmed: 40,
            archived: 38,
            through: mud_world::Seq(40),
            at_ms: 0,
        })
        .unwrap();
        assert_eq!(
            archived,
            "events archived: 38 written, 40 trimmed, through seq 40"
        );
    }

    #[test]
    fn feed_json_carries_no_markup_and_reads_the_same() {
        let v = json!({ "text": "<script>a && b</script>", "n": 1 });
        let text = json_text(&v);
        assert!(!text.contains('<') && !text.contains('>') && !text.contains('&'));
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), v);
        assert!(OBSERVER_JS.contains("k==='steward'"));
        assert!(
            OBSERVER_JS.contains("k==='notice'")
                && OBSERVER_JS.contains("'script','notice','booking'")
        );
    }

    #[test]
    fn url_escape_keeps_unreserved_bytes_only() {
        assert_eq!(url_escape("aZ09-._~"), "aZ09-._~");
        assert_eq!(url_escape("a+b&c=d#e%f/ g"), "a%2Bb%26c%3Dd%23e%25f%2F%20g");
        assert_eq!(url_escape("é"), "%C3%A9");
    }
}
