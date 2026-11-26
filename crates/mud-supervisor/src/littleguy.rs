//! Persistent minds on littleguy: one `lg serve` daemon per entity.
//!
//! The supervisor owns the lifecycle (the Mercer pattern). For each
//! persistent entity it writes an `lg.toml`, starts `lg serve` with its own
//! SQLite database and socket, and hands it a bearer token for the MUD's
//! MCP server. From inside littleguy the world is six MUD tools on two
//! servers, plus lg's own plan, note and workspace tools; the agent never
//! learns that it is supervised.
//!
//! The two servers split the tools by effect. `world` (effect `internal`)
//! serves `mud_look`, `mud_sheet`, `mud_wake_me` and `mud_cancel_wake`, so
//! an unattended heartbeat tick may read and book a wake. `mud` (effect
//! `write`) serves `mud_events` and `mud_command`: littleguy refuses effect
//! write on ticks, so a tick never moves the world or the event cursor. The
//! supervisor delivers each wake over the socket as a `chat` message, an
//! attended op, in which every tool works.
//!
//! The supervisor is also each daemon's owner on its socket ([`owner`]):
//! lg takes the caller with its own uid for the owner.

use std::collections::{BTreeSet, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use mud_world::EntityId;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

use tokio::sync::{Mutex, oneshot};

use crate::ledger::LedgerMeter;
use crate::mind::{
    BoxFuture, InFlight, InFlightGuard, Mind, MindError, MindNotice, MindReply, MudTools,
    SpendSource, ToolNames, Usage, UsageSink, WakeContext, heartbeat_prompt, littleguy_habits,
    system_prompt, wake_message,
};

pub mod compat;
pub mod owner;

use compat::{LG_MIN_COMMIT, Optional, README_INSTALL};

use owner::{OwnerRequest, OwnerResponse};

/// The longest request line this client sends, newline included: under
/// lg's cap (`MAX_REQUEST_BYTES`, 64 KiB), which refuses a longer line
/// unread.
pub const LG_MAX_LINE_BYTES: usize = 64 * 1024 - 1;

/// The tools of the `world` server: reads and wakes, which a quiet tick
/// may use.
pub const WORLD_TOOLS: [&str; 4] = ["mud_look", "mud_sheet", "mud_wake_me", "mud_cancel_wake"];

/// The tools of the `mud` server: effect write, attended chats only.
pub const MUD_TOOLS: [&str; 2] = ["mud_events", "mud_command"];

/// How long a daemon's reply is still read after its wake was given up.
const LATE_REPLY_GRACE: Duration = Duration::from_secs(3_600);

/// The variable that carries the MCP bearer token into `lg serve`.
pub const TOKEN_ENV: &str = "MUD_MCP_TOKEN";

/// The model keys passed through to `lg serve`, if set.
const MODEL_KEYS: [&str; 3] = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "OPENROUTER_API_KEY"];

/// How to run littleguy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LittleguyConfig {
    /// The `lg` binary.
    pub bin: PathBuf,
    /// Where each entity's folder goes (`<dir>/<entity>/`).
    pub data_dir: PathBuf,
    /// The MUD's MCP endpoint, such as `http://127.0.0.1:3000/mcp`.
    pub mcp_url: String,
    /// `--model`: `offline`, `claude`, `openai` or `openrouter`.
    pub provider: String,
    /// `--model-name`, if the provider needs one.
    pub model_name: Option<String>,
    /// `--base-url`, for an OpenAI-compatible server.
    pub base_url: Option<String>,
    /// Tool rounds per wake; `0` sets no bound, so a wake runs until the
    /// model answers (the daily budget still caps spend). A wake walks,
    /// looks, reads and speaks, each a round, and one that runs out ends
    /// with no answer, so leave room for a whole errand.
    pub max_steps: u32,
    /// Minutes between heartbeat ticks; 0 writes no `[heartbeat]`.
    pub heartbeat_minutes: u32,
    /// `heartbeat.research`: a tick researches the agent's open ideas.
    pub research: bool,
    /// `tools.files`: the note tools (with memory on). It also turns on
    /// lg's workspace file tools in the daemon's own folder; the ones that
    /// change it are effect write, so a tick cannot use them.
    pub notes: bool,
    /// `[compaction]`, or none.
    pub compaction: Option<CompactionRender>,
    /// The top-level `max_request_bytes` (below the provider's window); 0
    /// writes none (lg's default).
    pub max_request_bytes: u64,
    /// The `window`: the turns of transcript the agent sees.
    pub window: u32,
    /// Refine: `off` writes no `[refine]`; `menu` and `free` write it, and
    /// the Steward owns the overlays (ADR 0015).
    #[serde(default)]
    pub refine: RefineMode,
    /// How long `lg serve` may take to open its socket.
    pub start_timeout_secs: u64,
}

/// How an agent refines its own standing text (`[littleguy] refine`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefineMode {
    /// No `[refine]`: lg's proposer never runs, and the Steward leaves
    /// overlays alone.
    Off,
    /// The Steward runs lg's proposer and lets the council pick operator
    /// menu items from it as evidence (the code default).
    #[default]
    Menu,
    /// Council-approved free text (ADR 0016): the guardrails lines lg's
    /// proposer wrote, after the deterministic guard and a unanimous
    /// council of at least three. A Turing-tier entity still uses the
    /// menu.
    Free,
}

impl RefineMode {
    /// Whether refine runs at all.
    #[must_use]
    pub const fn is_on(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// The messages of transcript lg's proposer reads.
pub const REFINE_MESSAGES: u32 = 30;

/// The `[compaction]` section of an `lg.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionRender {
    /// Compact at this percent of the request cap (1 to 100).
    pub at_percent: u8,
    /// Resets of one op at most (1 or more).
    pub max_resets: u32,
}

impl Default for CompactionRender {
    fn default() -> Self {
        Self {
            at_percent: 80,
            max_resets: 3,
        }
    }
}

impl Default for LittleguyConfig {
    fn default() -> Self {
        Self {
            bin: PathBuf::from("lg"),
            data_dir: PathBuf::from("data/agents"),
            mcp_url: "http://127.0.0.1:3000/mcp".into(),
            provider: "offline".into(),
            model_name: None,
            base_url: None,
            max_steps: 64,
            heartbeat_minutes: 240,
            research: true,
            notes: true,
            compaction: Some(CompactionRender::default()),
            max_request_bytes: 240_000,
            window: 30,
            refine: RefineMode::Menu,
            start_timeout_secs: 30,
        }
    }
}

/// What one entity's daemon needs. Its token never leaves this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Persona {
    /// The display name.
    pub name: String,
    /// The persona text.
    pub persona: String,
    /// The private goal.
    pub goal: String,
    /// The pack's rule for a body ([`mud_world::WorldPack::body_rules`]).
    pub body: String,
    /// Its MCP bearer token.
    pub token: String,
}

impl Persona {
    /// The author's text of this persona, with no token.
    #[must_use]
    pub fn text(&self) -> PersonaText {
        PersonaText {
            name: self.name.clone(),
            persona: self.persona.clone(),
            goal: self.goal.clone(),
            body: self.body.clone(),
        }
    }
}

/// The author's text of a persona: trusted pack text, and no token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonaText {
    /// The display name.
    pub name: String,
    /// The persona text.
    pub persona: String,
    /// The private goal.
    pub goal: String,
    /// The pack's rule for a body.
    pub body: String,
}

/// One table of a rendered config.
fn table<const N: usize>(entries: [(&str, toml::Value); N]) -> toml::Table {
    entries
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect()
}

/// One `[[mcp.server]]` of the MUD: both servers reach the same endpoint
/// with the same token, and differ by effect and the tools they keep.
fn mcp_server(cfg: &LittleguyConfig, id: &str, effect: &str, tools: &[&str]) -> toml::Value {
    table([
        ("id", id.into()),
        ("url", cfg.mcp_url.clone().into()),
        ("bearer_env", TOKEN_ENV.into()),
        // Players write what the tools return; nothing returned is private.
        ("labels", toml::Value::Array(vec!["untrusted_input".into()])),
        ("effect", effect.into()),
        (
            "tools",
            toml::Value::Array(tools.iter().map(|t| (*t).into()).collect()),
        ),
    ])
    .into()
}

/// Renders the `lg.toml` of one entity, its pages served on loopback
/// `ui_port`.
///
/// `quiet` renders the same file with `[memory] enabled = false` and no
/// `[heartbeat]`: lg's reload applies both live, so no tick, reflection or
/// nightly consolidation spends a token. The memory files stay on disk.
/// Its system text then names no note tool, as lg serves none without
/// memory (the same holds for `notes = false`).
/// The config never asks for an owner-gated feature: no `[council]`,
/// `[skills]`, `[sandbox]`, `[approvals]`, `[scripts]`, `[patches]`,
/// `[research]`, `[hooks]`, `[followups]` or `allow_actions`.
#[must_use]
pub fn render_config(cfg: &LittleguyConfig, p: &PersonaText, ui_port: u16, quiet: bool) -> String {
    render_config_without(cfg, p, ui_port, quiet, &BTreeSet::new())
}

/// [`render_config`] with the optional parts in `lacks` left out, for an
/// `lg` too old to read them ([`compat`]).
#[must_use]
pub fn render_config_without(
    cfg: &LittleguyConfig,
    p: &PersonaText,
    ui_port: u16,
    quiet: bool,
    lacks: &BTreeSet<Optional>,
) -> String {
    let names = ToolNames::LITTLEGUY;
    // lg serves the note tools only with `tools.files` and `[memory]
    // enabled`, so the prompts name them only then.
    let notes = cfg.notes && !quiet;
    let mut root = toml::Table::new();
    root.insert("owner".into(), "mud".into());
    root.insert(
        "system".into(),
        // A littleguy agent is a Persistent or Turing entity: it books its
        // own wakes (an ambient one would be answered `no_wakes`).
        format!(
            "{}\n\n{}",
            system_prompt(names, &p.name, &p.persona, &p.goal, &p.body),
            littleguy_habits(names, notes)
        )
        .into(),
    );
    root.insert("window".into(), i64::from(cfg.window).into());
    if cfg.max_steps > 0 {
        root.insert("max_steps".into(), i64::from(cfg.max_steps).into());
    }
    if cfg.max_request_bytes > 0 && !lacks.contains(&Optional::MaxRequestBytes) {
        root.insert(
            "max_request_bytes".into(),
            i64::try_from(cfg.max_request_bytes)
                .unwrap_or(i64::MAX)
                .into(),
        );
    }
    root.insert(
        "tools".into(),
        table([
            ("web_fetch", false.into()),
            ("web_search", false.into()),
            ("files", cfg.notes.into()),
            ("runs", false.into()),
            ("plans", true.into()),
        ])
        .into(),
    );
    root.insert(
        "memory".into(),
        table([
            ("enabled", (!quiet).into()),
            ("consolidate", "0 4 * * *".into()),
            ("timezone", "UTC".into()),
            ("reflect", true.into()),
        ])
        .into(),
    );
    if cfg.heartbeat_minutes > 0 && !quiet {
        let mut heartbeat = table([
            ("every_minutes", i64::from(cfg.heartbeat_minutes).into()),
            ("prompt", heartbeat_prompt(names, notes).into()),
        ]);
        if !lacks.contains(&Optional::HeartbeatResearch) {
            // Researches open ideas; not lg's [research] sweep tool.
            heartbeat.insert("research".into(), cfg.research.into());
        }
        root.insert("heartbeat".into(), heartbeat.into());
    }
    if let Some(c) = cfg
        .compaction
        .filter(|_| !lacks.contains(&Optional::Compaction))
    {
        root.insert(
            "compaction".into(),
            table([
                ("at_percent", i64::from(c.at_percent).into()),
                ("max_resets", i64::from(c.max_resets).into()),
            ])
            .into(),
        );
    }
    if cfg.refine.is_on() {
        // lg's proposer; the Steward is the only caller (ADR 0015).
        root.insert(
            "refine".into(),
            table([
                ("messages", i64::from(REFINE_MESSAGES).into()),
                ("dimension", "inv".into()),
            ])
            .into(),
        );
    }
    root.insert("flow".into(), table([("rule_of_two", true.into())]).into());
    // A concrete port: littleguy builds its sign-in link from this address,
    // so port 0 would hand out links to port 0.
    root.insert(
        "ui".into(),
        table([("bind", format!("127.0.0.1:{ui_port}").into())]).into(),
    );
    root.insert(
        "mcp".into(),
        table([(
            "server",
            toml::Value::Array(vec![
                // May run on a quiet tick: no cursor moves, no world change.
                mcp_server(cfg, "world", "internal", &WORLD_TOOLS),
                // Attended chats only.
                mcp_server(cfg, "mud", "write", &MUD_TOOLS),
            ]),
        )])
        .into(),
    );
    toml::to_string(&root).unwrap_or_default()
}

/// Writes `text` to `path` so a reader never sees half a file: a temp file
/// in the same folder, flushed to disk, then renamed over `path`.
///
/// # Errors
/// The file cannot be written or renamed.
pub fn write_config(path: &Path, text: &str) -> std::io::Result<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path
        .file_name()
        .map_or_else(|| "lg.toml".into(), |n| n.to_string_lossy().into_owned());
    let tmp = dir.join(format!(".{name}.{}.{n}.tmp", std::process::id()));
    let written = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written?;
    // The rename itself survives a crash once the folder is flushed.
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// The socket reply of `lg serve`, as far as the supervisor reads it.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Response {
    Reply {
        text: String,
        #[serde(default)]
        usage: Option<LgUsage>,
    },
    Duplicate {
        last_answer: Option<String>,
    },
    NoAnswer {
        reason: String,
    },
    Waiting {
        approval: i64,
    },
    Busy {
        approval: i64,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Default, Deserialize)]
struct LgUsage {
    calls: u64,
    input_tokens: u64,
    output_tokens: u64,
    cost_usd_micros: u64,
}

/// Sends one `chat` request over a littleguy socket and reads the answer.
///
/// # Errors
/// The line is longer than lg reads (nothing is sent then), the socket
/// does not answer, or the answer is not a reply.
pub async fn chat(socket: &Path, message_id: &str, text: &str) -> Result<MindReply, MindError> {
    let unavailable =
        |e: std::io::Error| MindError::Unavailable(format!("{}: {e}", socket.display()));
    let mut line =
        serde_json::to_string(&json!({ "kind": "chat", "message_id": message_id, "text": text }))
            .map_err(|e| MindError::Failed(e.to_string()))?;
    line.push('\n');
    // lg refuses a longer line unread: never send half a request.
    if line.len() > LG_MAX_LINE_BYTES {
        return Err(MindError::Failed(
            "the wake is longer than lg accepts".into(),
        ));
    }
    let mut stream = UnixStream::connect(socket).await.map_err(unavailable)?;
    stream
        .write_all(line.as_bytes())
        .await
        .map_err(unavailable)?;
    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader.read_line(&mut buf).await.map_err(unavailable)?;
        if n == 0 {
            return Err(MindError::Unavailable(
                "the daemon closed the socket".into(),
            ));
        }
        let v: Value =
            serde_json::from_str(buf.trim()).map_err(|e| MindError::Failed(e.to_string()))?;
        // A streamed piece is a preview; the response line comes after.
        if matches!(v["kind"].as_str(), Some("delta" | "restart")) {
            continue;
        }
        let r: Response =
            serde_json::from_value(v).map_err(|e| MindError::Failed(e.to_string()))?;
        return match r {
            Response::Reply { text, usage } => {
                let u = usage.unwrap_or_default();
                Ok(MindReply {
                    text: Some(text),
                    usage: Usage {
                        calls: u.calls,
                        input_tokens: u.input_tokens,
                        output_tokens: u.output_tokens,
                        cost_usd_micros: u.cost_usd_micros,
                    },
                    commands: 0,
                    unanswered: false,
                })
            }
            Response::Duplicate { last_answer } => Ok(MindReply {
                text: last_answer,
                ..MindReply::default()
            }),
            Response::NoAnswer { reason } => Ok(MindReply {
                text: Some(format!("no answer: {reason}")),
                unanswered: true,
                ..MindReply::default()
            }),
            Response::Waiting { approval } | Response::Busy { approval } => {
                Err(MindError::Failed(format!(
                    "a tool call waits for approval {approval}; check the [flow] labels of the mud MCP server"
                )))
            }
            Response::Other => Err(MindError::Failed("unexpected answer from lg serve".into())),
        };
    }
}

/// Where a wake's spend is read from: the daemon's cost ledger, as of the
/// wake's start.
#[derive(Debug, Clone)]
pub(crate) struct Charge {
    /// The daemon's ledger.
    pub meter: Arc<LedgerMeter>,
    /// The wake's start, which fixes the budget day.
    pub now_ms: u64,
    /// Held until the op is read and charged: the entity's wake settles
    /// only then ([`Mind::settled`]).
    pub until_settled: Option<Arc<InFlightGuard>>,
}

impl Charge {
    /// The spend to charge now: what the ledger has that was not charged
    /// yet, or `reported` when there is no ledger to read.
    pub async fn settle(&self, entity: &EntityId, reported: Usage) -> Usage {
        let (meter, now) = (Arc::clone(&self.meter), self.now_ms);
        match tokio::task::spawn_blocking(move || meter.charge(now)).await {
            Ok(Ok(Some(spent))) => spent,
            Ok(Ok(None)) => reported,
            Ok(Err(e)) => {
                tracing::warn!(%entity, error = %e, "littleguy ledger unreadable; charging the reported usage");
                reported
            }
            Err(e) => {
                tracing::warn!(%entity, error = %e, "littleguy ledger read failed; charging the reported usage");
                reported
            }
        }
    }
}

/// [`chat`] on a task of its own. `lg serve` finishes an op after its
/// wake is given up (a timeout), so the reply is read whatever happens to
/// the wake.
///
/// Whatever the answer (a reply, no answer, a wait for approval, an error,
/// or nothing within [`LATE_REPLY_GRACE`]), the spend is read from the
/// daemon's ledger (see [`crate::ledger`]) and goes in exactly one place:
/// the reply, if the wake still waits for it, else `sink`. Spend the daemon
/// makes after that is charged by the next wake. A reply that comes after
/// the wake gave up also marks the sink that the op answered
/// ([`UsageSink::answered_late`]); a late `no_answer` or error does not.
pub(crate) async fn chat_charged(
    socket: PathBuf,
    message_id: String,
    text: String,
    entity: EntityId,
    sink: Arc<UsageSink>,
    charge: Charge,
) -> Result<MindReply, MindError> {
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let out = tokio::time::timeout(LATE_REPLY_GRACE, chat(&socket, &message_id, &text)).await;
        match out {
            Ok(Ok(mut r)) => {
                r.usage = charge.settle(&entity, r.usage).await;
                if let Err(Ok(r)) = tx.send(Ok(r)) {
                    // The wake gave up first: the answer, like the spend,
                    // goes to the sink.
                    if !r.unanswered {
                        sink.answered_late(&entity);
                    }
                    sink.add(&entity, r.usage);
                }
            }
            Ok(Err(e)) => {
                sink.add(&entity, charge.settle(&entity, Usage::default()).await);
                let _ = tx.send(Err(e));
            }
            Err(_) => sink.add(&entity, charge.settle(&entity, Usage::default()).await),
        }
        // The op is read and charged: the wake may settle.
        drop(charge.until_settled);
    });
    rx.await
        .map_err(|_| MindError::Unavailable("the daemon did not answer in time".into()))?
}

/// A loopback port that was free a moment ago, for a daemon's pages.
fn free_loopback_port() -> std::io::Result<u16> {
    Ok(std::net::TcpListener::bind(("127.0.0.1", 0))?
        .local_addr()?
        .port())
}

/// Asks the daemon at `socket` for a one-time sign-in link to its pages,
/// as its owner ([`owner`]). lg makes the link in its job loop, so this
/// waits behind a wake or a tick that runs ([`owner::LOOP_JOB_TIMEOUT`]).
///
/// # Errors
/// The socket does not answer, or the daemon refuses.
pub async fn ui_link(socket: &Path) -> Result<String, MindError> {
    match owner::request(
        socket,
        &OwnerRequest::UiLink,
        OwnerRequest::UiLink.timeout(),
    )
    .await
    {
        Ok(OwnerResponse::ConnectLink { link }) if is_loopback_link(&link) => Ok(link),
        Ok(OwnerResponse::ConnectLink { .. }) => Err(MindError::Failed(
            "littleguy gave a sign-in link that is not on loopback".into(),
        )),
        Ok(OwnerResponse::Error { message } | OwnerResponse::Off { message }) => {
            Err(MindError::Failed(message))
        }
        Ok(_) => Err(MindError::Failed("unexpected answer from lg serve".into())),
        Err(e @ (owner::OwnerError::Io(_) | owner::OwnerError::Timeout)) => {
            Err(MindError::Unavailable(e.to_string()))
        }
        Err(e) => Err(MindError::Failed(e.to_string())),
    }
}

/// Whether `link` points at this machine's loopback, so the observer never
/// redirects anywhere else.
fn is_loopback_link(link: &str) -> bool {
    ["http://127.0.0.1:", "http://[::1]:"]
        .iter()
        .any(|p| link.starts_with(p))
}

/// The error of a start that failed because the `lg` at `bin` is older
/// than the MUD needs: it refused `path`, which the MUD cannot leave out,
/// or it still exited after the MUD left out what it refused.
fn older_lg(
    entity: &EntityId,
    status: std::process::ExitStatus,
    bin: &Path,
    path: Option<&str>,
    log: &Path,
) -> MindError {
    let why = path.map_or_else(
        || "it still exits after the MUD left out what it does not read".to_owned(),
        |p| format!("it does not read `{p}` in lg.toml, which the MUD needs"),
    );
    MindError::Unavailable(format!(
        "lg serve for {entity} exited ({status}): the lg at {} is older than the MUD needs, as {why}. \
         Build lg at littleguy {LG_MIN_COMMIT} or later ({README_INSTALL}); see {}",
        bin.display(),
        log.display()
    ))
}

/// The file in an entity's folder that holds its daemon's process id.
const PID_FILE: &str = "lg.pid";

/// Stops a daemon a previous run of the MUD left behind (it crashed or was
/// killed before it could stop its children), so two daemons never share
/// one database. The process is stopped only if its command line names
/// this entity's `socket`, so a recycled process id is never touched.
/// Where `/proc` is missing (not Linux) nothing is checked or stopped.
pub(crate) async fn reap_orphan(dir: &Path, socket: &Path) {
    let pid_file = dir.join(PID_FILE);
    let Some(pid) = std::fs::read_to_string(&pid_file)
        .ok()
        .and_then(|p| p.trim().parse::<u32>().ok())
    else {
        return;
    };
    let ours = std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmd| {
        let want = socket.as_os_str().as_encoded_bytes();
        cmd.split(|b| *b == 0).any(|arg| arg == want)
    });
    if ours {
        tracing::warn!(pid, dir = %dir.display(), "stopping an lg serve left by an earlier run");
        for signal in ["-TERM", "-KILL"] {
            let _ = Command::new("kill")
                .arg(signal)
                .arg(pid.to_string())
                .status()
                .await;
            for _ in 0..20 {
                if !Path::new(&format!("/proc/{pid}")).exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            if !Path::new(&format!("/proc/{pid}")).exists() {
                break;
            }
        }
    }
    let _ = std::fs::remove_file(pid_file);
}

/// What a daemon answered when it was asked `approvals` after its start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalCheck {
    /// `off`: no tool call ever waits for an owner. The MUD expects this.
    Off,
    /// Approvals are on: a wake may stop at an approval the MUD never
    /// gives.
    On {
        /// The calls that wait now.
        waiting: usize,
    },
    /// No answer, or one this check does not expect.
    Unknown(String),
}

/// Asks a daemon that just started whether any tool call waits for an
/// owner's approval. The MUD config has no `[sandbox]` and no tool carries
/// `private_data`, so lg answers `off`; anything else is logged, because
/// then a wake could stop at an approval the MUD never gives.
async fn check_approvals(entity: &EntityId, socket: &Path) -> ApprovalCheck {
    let req = OwnerRequest::Approvals;
    let check = match owner::request(socket, &req, req.timeout()).await {
        Ok(OwnerResponse::Off { .. }) => return ApprovalCheck::Off,
        Ok(OwnerResponse::Approvals { entries }) => ApprovalCheck::On {
            waiting: entries.len(),
        },
        Ok(other) => ApprovalCheck::Unknown(format!("{other:?}")),
        Err(e) => ApprovalCheck::Unknown(e.to_string()),
    };
    if let ApprovalCheck::On { waiting } = check {
        tracing::warn!(%entity, waiting, "lg serve has approvals on; a wake may wait for one");
    } else {
        tracing::warn!(%entity, answer = ?check, "lg serve did not answer the approvals check with off");
    }
    check
}

#[derive(Debug)]
struct Daemon {
    child: Child,
    socket: PathBuf,
    /// The loopback port of its pages, kept so a config rewrite (quiet or
    /// loud) never moves them.
    ui_port: u16,
    /// What it answered to the approvals check after its start.
    approvals: ApprovalCheck,
}

/// A mind that runs each entity as its own `lg serve`.
#[derive(Debug)]
pub struct LittleguyMind {
    cfg: LittleguyConfig,
    personas: HashMap<EntityId, Persona>,
    daemons: Mutex<HashMap<EntityId, Daemon>>,
    meters: std::sync::Mutex<HashMap<EntityId, Arc<LedgerMeter>>>,
    /// The entities whose config is rendered quiet ([`render_config`]).
    quiet: std::sync::Mutex<BTreeSet<EntityId>>,
    /// Set by [`LittleguyMind::shutdown`]: no daemon starts after it.
    closed: std::sync::atomic::AtomicBool,
    /// Ops still read after their wake (see [`chat_charged`]).
    in_flight: InFlight,
    /// What to tell the operator after the next wake ([`Mind::take_notices`]).
    notices: std::sync::Mutex<Vec<MindNotice>>,
}

/// How one `lg serve` start ended.
#[derive(Debug)]
enum Started {
    /// The daemon runs and its socket is open.
    Running(Child),
    /// The daemon exited before it opened its socket.
    Exited(std::process::ExitStatus),
}

impl LittleguyMind {
    /// A mind for these entities.
    #[must_use]
    pub fn new(cfg: LittleguyConfig, personas: HashMap<EntityId, Persona>) -> Self {
        Self {
            cfg,
            personas,
            daemons: Mutex::new(HashMap::new()),
            meters: std::sync::Mutex::new(HashMap::new()),
            quiet: std::sync::Mutex::new(BTreeSet::new()),
            closed: std::sync::atomic::AtomicBool::new(false),
            in_flight: InFlight::default(),
            notices: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// The ledger meter of `entity`'s daemon.
    fn meter(&self, entity: &EntityId) -> Arc<LedgerMeter> {
        let mut meters = self
            .meters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(
            meters.entry(entity.clone()).or_insert_with(|| {
                Arc::new(LedgerMeter::new(self.entity_dir(entity).join("lg.db")))
            }),
        )
    }

    /// The folder of one entity.
    #[must_use]
    pub fn entity_dir(&self, entity: &EntityId) -> PathBuf {
        self.cfg.data_dir.join(entity.as_str())
    }

    /// Writes the files and starts the daemon of `entity`, if it is not
    /// running. Returns its socket.
    ///
    /// A daemon that exits before its socket opens because its `lg` does
    /// not read an optional part of the config ([`compat`]) starts again
    /// without that part, once per part, with a warning.
    ///
    /// # Errors
    /// The files cannot be written, the daemon does not start in time, or
    /// it exits: then the error names an `lg` that is older than the MUD
    /// needs, if its log says so.
    #[expect(
        clippy::significant_drop_tightening,
        reason = "the lock is held through start-up so one entity never gets two daemons"
    )]
    pub async fn ensure(&self, entity: &EntityId) -> Result<PathBuf, MindError> {
        let mut daemons = self.daemons.lock().await;
        // Checked under the lock `shutdown` takes, so a wake racing the
        // shutdown cannot start a daemon after it has stopped the rest.
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(MindError::Unavailable("the MUD is shutting down".into()));
        }
        if let Some(d) = daemons.get_mut(entity) {
            if matches!(d.child.try_wait(), Ok(None)) {
                return Ok(d.socket.clone());
            }
            daemons.remove(entity);
        }
        let persona = self
            .personas
            .get(entity)
            .ok_or_else(|| MindError::Unavailable(format!("no persona for {entity}")))?;
        let dir = self.entity_dir(entity);
        let io = |e: std::io::Error| MindError::Unavailable(format!("{}: {e}", dir.display()));
        std::fs::create_dir_all(&dir).map_err(io)?;
        let config = dir.join("lg.toml");
        let log = dir.join("lg.log");
        let ui_port = free_loopback_port().map_err(io)?;
        let bin = compat::resolve_bin(&self.cfg.bin);
        let socket = dir.join("lg.sock");
        // An older lg refuses a part it does not read, and the next start
        // leaves that part out. Each optional part is dropped once at most,
        // so a crashing lg never loops.
        let mut dropped: Vec<Optional> = Vec::new();
        loop {
            let text = render_config_without(
                &self.cfg,
                &persona.text(),
                ui_port,
                self.is_quiet(entity),
                &compat::lacks(&bin),
            );
            write_config(&config, &text).map_err(io)?;
            let status = match self.start(entity, persona, &dir, &config, &socket).await? {
                Started::Running(child) => {
                    let approvals = check_approvals(entity, &socket).await;
                    daemons.insert(
                        entity.clone(),
                        Daemon {
                            child,
                            socket: socket.clone(),
                            ui_port,
                            approvals,
                        },
                    );
                    return Ok(socket);
                }
                Started::Exited(status) => status,
            };
            let refused = compat::refused_key(&compat::log_tail(&log), &text);
            match refused
                .as_deref()
                .map(|path| (path, Optional::from_path(path)))
            {
                Some((_, Some(part))) if !dropped.contains(&part) => {
                    dropped.push(part);
                    self.lacks_part(entity, &bin, part);
                }
                Some((path, _)) => {
                    return Err(older_lg(entity, status, &bin, Some(path), &log));
                }
                None if !dropped.is_empty() => {
                    return Err(older_lg(entity, status, &bin, None, &log));
                }
                None => {
                    return Err(MindError::Unavailable(format!(
                        "lg serve for {entity} exited ({status}); see {}",
                        log.display()
                    )));
                }
            }
        }
    }

    /// Spawns `lg serve` for `entity` on the config at `config` and waits
    /// for its socket.
    ///
    /// # Errors
    /// The process does not spawn, or opens no socket in time.
    async fn start(
        &self,
        entity: &EntityId,
        persona: &Persona,
        dir: &Path,
        config: &Path,
        socket: &Path,
    ) -> Result<Started, MindError> {
        let io = |e: std::io::Error| MindError::Unavailable(format!("{}: {e}", dir.display()));
        reap_orphan(dir, socket).await;
        let _ = std::fs::remove_file(socket);
        let mut cmd = Command::new(&self.cfg.bin);
        cmd.arg("serve")
            .arg("--config")
            .arg(config)
            .arg("--db")
            .arg(dir.join("lg.db"))
            .arg("--socket")
            .arg(socket)
            .arg("--model")
            .arg(&self.cfg.provider);
        if let Some(name) = &self.cfg.model_name {
            cmd.arg("--model-name").arg(name);
        }
        if let Some(url) = &self.cfg.base_url {
            cmd.arg("--base-url").arg(url);
        }
        cmd.env_clear();
        for var in ["PATH", "HOME", "RUST_LOG", "LG_LOG"]
            .iter()
            .chain(MODEL_KEYS.iter())
        {
            if let Ok(v) = std::env::var(var) {
                cmd.env(var, v);
            }
        }
        if std::env::var_os("LG_LOG").is_none() {
            cmd.env("LG_LOG", "info");
        }
        cmd.env(TOKEN_ENV, &persona.token)
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("XDG_DATA_HOME", dir.join("share"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(dir.join("lg.log")).map_err(io)?)
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            MindError::Unavailable(format!("starting {}: {e}", self.cfg.bin.display()))
        })?;
        if let Some(pid) = child.id() {
            std::fs::write(dir.join(PID_FILE), pid.to_string()).map_err(io)?;
        }
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(self.cfg.start_timeout_secs);
        while !socket.exists() {
            if let Ok(Some(status)) = child.try_wait() {
                return Ok(Started::Exited(status));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(MindError::Unavailable(format!(
                    "lg serve for {entity} did not open its socket"
                )));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(Started::Running(child))
    }

    /// Records that the `lg` at `bin` lacks `part`, and warns once per
    /// binary and part: in the log and, after the wake, on the observer.
    fn lacks_part(&self, entity: &EntityId, bin: &Path, part: Optional) {
        if !compat::record(bin, part) {
            return;
        }
        let text = format!(
            "the lg at {} does not read `{}` in lg.toml ({} added it), so its agents run without it. \
             Build lg at littleguy {LG_MIN_COMMIT} or later ({README_INSTALL}), or set [littleguy] {} to leave it out",
            bin.display(),
            part.path(),
            part.since(),
            part.off_key(),
        );
        tracing::warn!(
            %entity,
            bin = %bin.display(),
            section = part.path(),
            min_commit = LG_MIN_COMMIT,
            readme = README_INSTALL,
            "{text}"
        );
        self.notices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(MindNotice {
                entity: entity.clone(),
                code: "lg_lacks_section".into(),
                text,
            });
    }

    /// The config file of `entity`'s daemon.
    fn config_path(&self, entity: &EntityId) -> PathBuf {
        self.entity_dir(entity).join("lg.toml")
    }

    /// The socket of `entity`'s daemon while it runs. It never starts one.
    pub async fn running_socket(&self, entity: &EntityId) -> Option<PathBuf> {
        self.daemons
            .lock()
            .await
            .get_mut(entity)
            .and_then(|d| matches!(d.child.try_wait(), Ok(None)).then(|| d.socket.clone()))
    }

    /// What `entity`'s running daemon answered to the approvals check
    /// after its start ([`ApprovalCheck::Off`] is expected). `None` while
    /// no daemon runs.
    pub async fn approval_check(&self, entity: &EntityId) -> Option<ApprovalCheck> {
        self.daemons
            .lock()
            .await
            .get_mut(entity)
            .and_then(|d| matches!(d.child.try_wait(), Ok(None)).then(|| d.approvals.clone()))
    }

    /// The author's text of `entity`'s persona, with no token.
    #[must_use]
    pub fn brief(&self, entity: &EntityId) -> Option<PersonaText> {
        self.personas.get(entity).map(Persona::text)
    }

    /// Whether `entity`'s config is rendered quiet.
    #[must_use]
    pub fn is_quiet(&self, entity: &EntityId) -> bool {
        self.quiet
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(entity)
    }

    /// Renders `entity`'s config quiet (no heartbeat, memory off) or loud,
    /// and rewrites it now when its daemon runs, on the same pages port.
    /// lg looks at its config file every 2 s and reloads a changed one by
    /// itself; the caller's `reload` applies it at once if the watcher has
    /// not yet. So any `reloaded` answer is success, and its `applied` list
    /// may be empty: the watcher's reload may have applied the change
    /// first. A daemon that starts later starts with it.
    ///
    /// # Errors
    /// The entity is not this mind's, or the file cannot be written.
    pub async fn set_quiet(&self, entity: &EntityId, quiet: bool) -> Result<(), MindError> {
        let persona = self
            .personas
            .get(entity)
            .ok_or_else(|| MindError::Unavailable(format!("no persona for {entity}")))?;
        // Under the daemons lock, through the write: a start renders the
        // same flag, and two calls never write out of order.
        let daemons = self.daemons.lock().await;
        {
            let mut set = self
                .quiet
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if quiet {
                set.insert(entity.clone());
            } else {
                set.remove(entity);
            }
        }
        let Some(ui_port) = daemons.get(entity).map(|d| d.ui_port) else {
            return Ok(());
        };
        let path = self.config_path(entity);
        let lacks = compat::lacks(&compat::resolve_bin(&self.cfg.bin));
        let written = write_config(
            &path,
            &render_config_without(&self.cfg, &persona.text(), ui_port, quiet, &lacks),
        );
        drop(daemons);
        written.map_err(|e| MindError::Unavailable(format!("{}: {e}", path.display())))
    }

    /// Stops every daemon and waits for each to exit; no daemon starts
    /// after this. Idempotent.
    pub async fn shutdown(&self) {
        let mut daemons = self.daemons.lock().await;
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        for (entity, mut d) in daemons.drain() {
            if let Err(e) = d.child.kill().await {
                tracing::warn!(%entity, error = %e, "stopping lg serve");
            }
            let _ = std::fs::remove_file(self.entity_dir(&entity).join(PID_FILE));
            let _ = std::fs::remove_file(&d.socket);
        }
    }

    /// Whether `entity` is one of this mind's.
    #[must_use]
    pub fn serves(&self, entity: &EntityId) -> bool {
        self.personas.contains_key(entity)
    }

    /// A one-time sign-in link to the pages of `entity`'s daemon (its
    /// goals, notes, skills, memory and history), for the observer. The
    /// pages listen on loopback only. The daemon is not started for this.
    ///
    /// # Errors
    /// The daemon is not running yet, or does not answer.
    pub async fn ui_link(&self, entity: &EntityId) -> Result<String, MindError> {
        let Some(socket) = self.running_socket(entity).await else {
            return Err(MindError::Unavailable(format!(
                "{entity}'s littleguy is not running yet; it starts on its first wake"
            )));
        };
        ui_link(&socket).await
    }

    /// Whether [`LittleguyMind::shutdown`] has run.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// The Steward's seam (ADR 0015): the Steward owns these daemons through
/// it, and never starts one.
impl crate::steward::OwnedDaemons for LittleguyMind {
    fn entities(&self) -> Vec<EntityId> {
        let mut ids: Vec<EntityId> = self.personas.keys().cloned().collect();
        ids.sort();
        ids
    }

    fn running_socket<'a>(&'a self, entity: &'a EntityId) -> BoxFuture<'a, Option<PathBuf>> {
        Box::pin(Self::running_socket(self, entity))
    }

    fn start_key<'a>(&'a self, entity: &'a EntityId) -> BoxFuture<'a, Option<u32>> {
        Box::pin(async move {
            self.daemons.lock().await.get_mut(entity).and_then(|d| {
                if matches!(d.child.try_wait(), Ok(None)) {
                    d.child.id()
                } else {
                    None
                }
            })
        })
    }

    fn brief(&self, entity: &EntityId) -> Option<PersonaText> {
        Self::brief(self, entity)
    }

    /// The ledger rows no wake charged yet (a heartbeat tick's), now
    /// marked charged on the shared meter.
    fn settle<'a>(&'a self, entity: &'a EntityId, now_ms: u64) -> BoxFuture<'a, Usage> {
        Box::pin(async move {
            Charge {
                meter: self.meter(entity),
                now_ms,
                until_settled: None,
            }
            .settle(entity, Usage::default())
            .await
        })
    }

    fn set_quiet<'a>(
        &'a self,
        entity: &'a EntityId,
        quiet: bool,
    ) -> BoxFuture<'a, Result<(), MindError>> {
        Box::pin(Self::set_quiet(self, entity, quiet))
    }

    fn is_quiet(&self, entity: &EntityId) -> bool {
        Self::is_quiet(self, entity)
    }
}

impl Mind for LittleguyMind {
    fn kind(&self) -> &'static str {
        "littleguy"
    }

    fn take_notices(&self) -> Vec<MindNotice> {
        std::mem::take(
            &mut *self
                .notices
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// littleguy's ledger is the one complete record of its spend.
    fn spend_source(&self) -> SpendSource {
        SpendSource::Ledger
    }

    /// Reads the daemon's ledger at boot: today's rows, now marked seen.
    /// An unreadable ledger primes nothing (logged): its rows are then
    /// charged by the first wake on top of the stored tally, which
    /// over-counts, the safe direction for a budget.
    fn prime(&self, entity: &EntityId, now_ms: u64) -> Option<Usage> {
        match self.meter(entity).prime(now_ms) {
            Ok(primed) => primed,
            Err(e) => {
                tracing::warn!(%entity, error = %e, "littleguy ledger unreadable at boot; not primed");
                None
            }
        }
    }

    /// Resolves once every op of the entity was read and charged, or its
    /// grace ([`LATE_REPLY_GRACE`]) ran out.
    fn settled(&self, entity: &EntityId) -> BoxFuture<'_, ()> {
        self.in_flight.settled(entity)
    }

    fn wake(
        &self,
        ctx: WakeContext,
        tools: MudTools,
    ) -> BoxFuture<'_, Result<MindReply, MindError>> {
        Box::pin(async move {
            let charge = Charge {
                meter: self.meter(&ctx.entity),
                now_ms: ctx.now_ms,
                until_settled: Some(Arc::new(self.in_flight.start(&ctx.entity))),
            };
            // Spend since the last wake (a heartbeat, an op that outlived
            // its wake) is owed before this wake adds its own.
            let between = charge.settle(&ctx.entity, Usage::default()).await;
            tools.usage_sink().add(&ctx.entity, between);
            let socket = self.ensure(&ctx.entity).await?;
            chat_charged(
                socket,
                ctx.wake_id.clone(),
                wake_message(ToolNames::LITTLEGUY, &ctx),
                ctx.entity.clone(),
                Arc::clone(tools.usage_sink()),
                charge,
            )
            .await
        })
    }
}

#[cfg(test)]
mod old_lg_tests;
#[cfg(test)]
mod tests;
