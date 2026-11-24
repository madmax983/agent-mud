//! Persistent minds on littleguy: one `lg serve` daemon per entity.
//!
//! The supervisor owns the lifecycle (the Mercer pattern). For each
//! persistent entity it writes an `lg.toml`, starts `lg serve` with its own
//! SQLite database and socket, and hands it a bearer token for the MUD's
//! MCP server. From inside littleguy the world is just one MCP server with
//! four tools; the agent never learns that it is supervised.
//!
//! littleguy refuses non-internal MCP calls on unattended heartbeat ticks,
//! so the supervisor never relies on the heartbeat to act. It delivers each
//! wake over the socket as a `chat` message, an attended op, and keeps the
//! heartbeat for quiet reflection and memory consolidation.

use std::collections::HashMap;
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
    BoxFuture, Mind, MindError, MindReply, MudTools, Usage, UsageSink, WakeContext, system_prompt,
    wake_message,
};

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
    /// Minutes between reflection heartbeats; 0 turns them off.
    pub heartbeat_minutes: u32,
    /// How long `lg serve` may take to open its socket.
    pub start_timeout_secs: u64,
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
            start_timeout_secs: 30,
        }
    }
}

/// What one entity's daemon needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Persona {
    /// The display name.
    pub name: String,
    /// The persona text.
    pub persona: String,
    /// The private goal.
    pub goal: String,
    /// Its MCP bearer token.
    pub token: String,
}

/// Renders the `lg.toml` of one entity.
#[must_use]
pub fn render_config(cfg: &LittleguyConfig, p: &Persona) -> String {
    let mut root = toml::Table::new();
    root.insert("owner".into(), "mud".into());
    root.insert(
        "system".into(),
        system_prompt(&p.name, &p.persona, &p.goal).into(),
    );
    root.insert("window".into(), 30.into());
    if cfg.max_steps > 0 {
        root.insert("max_steps".into(), i64::from(cfg.max_steps).into());
    }
    let mut tools = toml::Table::new();
    for (k, v) in [
        ("web_fetch", false),
        ("web_search", false),
        ("files", false),
        ("runs", false),
        ("plans", true),
    ] {
        tools.insert(k.into(), v.into());
    }
    root.insert("tools".into(), tools.into());
    let mut memory = toml::Table::new();
    memory.insert("enabled".into(), true.into());
    memory.insert("consolidate".into(), "0 4 * * *".into());
    memory.insert("timezone".into(), "UTC".into());
    memory.insert("reflect".into(), true.into());
    root.insert("memory".into(), memory.into());
    let mut ui = toml::Table::new();
    // Port 0: each daemon's private pages take a free port.
    ui.insert("bind".into(), "127.0.0.1:0".into());
    root.insert("ui".into(), ui.into());
    if cfg.heartbeat_minutes > 0 {
        let mut hb = toml::Table::new();
        hb.insert(
            "every_minutes".into(),
            i64::from(cfg.heartbeat_minutes).into(),
        );
        hb.insert("research".into(), false.into());
        root.insert("heartbeat".into(), hb.into());
    }
    let mut server = toml::Table::new();
    server.insert("id".into(), "mud".into());
    server.insert("url".into(), cfg.mcp_url.clone().into());
    server.insert("bearer_env".into(), TOKEN_ENV.into());
    // Players write what the tools return; nothing returned is private.
    server.insert(
        "labels".into(),
        toml::Value::Array(vec!["untrusted_input".into()]),
    );
    server.insert("effect".into(), "write".into());
    let mut mcp = toml::Table::new();
    mcp.insert("server".into(), toml::Value::Array(vec![server.into()]));
    root.insert("mcp".into(), mcp.into());
    toml::to_string(&root).unwrap_or_default()
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
/// The socket does not answer, or the answer is not a reply.
pub async fn chat(socket: &Path, message_id: &str, text: &str) -> Result<MindReply, MindError> {
    let unavailable =
        |e: std::io::Error| MindError::Unavailable(format!("{}: {e}", socket.display()));
    let mut stream = UnixStream::connect(socket).await.map_err(unavailable)?;
    let mut line =
        serde_json::to_string(&json!({ "kind": "chat", "message_id": message_id, "text": text }))
            .map_err(|e| MindError::Failed(e.to_string()))?;
    line.push('\n');
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
                })
            }
            Response::Duplicate { last_answer } => Ok(MindReply {
                text: last_answer,
                ..MindReply::default()
            }),
            Response::NoAnswer { reason } => Ok(MindReply {
                text: Some(format!("no answer: {reason}")),
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
/// makes after that is charged by the next wake.
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
                    sink.add(&entity, r.usage);
                }
            }
            Ok(Err(e)) => {
                sink.add(&entity, charge.settle(&entity, Usage::default()).await);
                let _ = tx.send(Err(e));
            }
            Err(_) => sink.add(&entity, charge.settle(&entity, Usage::default()).await),
        }
    });
    rx.await
        .map_err(|_| MindError::Unavailable("the daemon did not answer in time".into()))?
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

#[derive(Debug)]
struct Daemon {
    child: Child,
    socket: PathBuf,
}

/// A mind that runs each entity as its own `lg serve`.
#[derive(Debug)]
pub struct LittleguyMind {
    cfg: LittleguyConfig,
    personas: HashMap<EntityId, Persona>,
    daemons: Mutex<HashMap<EntityId, Daemon>>,
    meters: std::sync::Mutex<HashMap<EntityId, Arc<LedgerMeter>>>,
    /// Set by [`LittleguyMind::shutdown`]: no daemon starts after it.
    closed: std::sync::atomic::AtomicBool,
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
            closed: std::sync::atomic::AtomicBool::new(false),
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
    /// # Errors
    /// The files cannot be written, or the daemon does not start in time.
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
        std::fs::write(&config, render_config(&self.cfg, persona)).map_err(io)?;
        let socket = dir.join("lg.sock");
        reap_orphan(&dir, &socket).await;
        let _ = std::fs::remove_file(&socket);
        let mut cmd = Command::new(&self.cfg.bin);
        cmd.arg("serve")
            .arg("--config")
            .arg(&config)
            .arg("--db")
            .arg(dir.join("lg.db"))
            .arg("--socket")
            .arg(&socket)
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
                return Err(MindError::Unavailable(format!(
                    "lg serve for {entity} exited ({status}); see {}",
                    dir.join("lg.log").display()
                )));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(MindError::Unavailable(format!(
                    "lg serve for {entity} did not open its socket"
                )));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        daemons.insert(
            entity.clone(),
            Daemon {
                child,
                socket: socket.clone(),
            },
        );
        Ok(socket)
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

    /// Whether [`LittleguyMind::shutdown`] has run.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Mind for LittleguyMind {
    fn kind(&self) -> &'static str {
        "littleguy"
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
            };
            // Spend since the last wake (a heartbeat, an op that outlived
            // its wake) is owed before this wake adds its own.
            let between = charge.settle(&ctx.entity, Usage::default()).await;
            tools.usage_sink().add(&ctx.entity, between);
            let socket = self.ensure(&ctx.entity).await?;
            chat_charged(
                socket,
                ctx.wake_id.clone(),
                wake_message(&ctx),
                ctx.entity.clone(),
                Arc::clone(tools.usage_sink()),
                charge,
            )
            .await
        })
    }
}

#[cfg(test)]
mod tests;
