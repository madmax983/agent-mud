//! The littleguy MUD server.
//!
//! One process owns the world and the lifecycle of every mind. It serves
//! the browser terminal and the observer dashboard on autumn-web, plain
//! telnet, and the MCP endpoint that littleguy agents (or any MCP client)
//! use to play. The supervisor runs alongside and decides when each mind
//! thinks.

pub mod config;
pub mod minds;
pub mod session;
pub mod smoke;
pub mod telnet;
pub mod web;

use std::fmt::Write as _;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use autumn_web::app::AppBuilder;
use autumn_web::config::{AutumnConfig, ConfigError, ConfigLoader, LogConfig, TelemetryConfig};
use autumn_web::telemetry::{TelemetryGuard, TelemetryInitError, TelemetryProvider};
use mud_mcp::{CommandGate, TokenRegistry};
use mud_supervisor::gate::Gate;
use mud_supervisor::littleguy::LittleguyMind;
use mud_supervisor::runtime::{Supervisor, SupervisorConfig};
use mud_supervisor::scripted::FallbackMind;
use mud_world::{CommandResult, EntityId, Game, SystemClock, WorldPack};

use crate::config::MudConfig;
use crate::session::Presence;
use crate::web::Mud;

/// The MCP command gate: every `mud_command` from an agent passes the
/// Turing typing gate, as in-process minds do.
#[derive(Debug, Clone)]
pub struct TypingGate(pub Arc<Gate>);

impl CommandGate for TypingGate {
    fn run(
        &self,
        entity: EntityId,
        line: String,
    ) -> futures::future::BoxFuture<'static, CommandResult> {
        let gate = Arc::clone(&self.0);
        Box::pin(async move { gate.run(&entity, &line).await })
    }
}

/// Everything a running server holds.
#[derive(Debug)]
pub struct Built {
    /// The shared state of the routes.
    pub mud: Arc<Mud>,
    /// MCP bearer tokens.
    pub tokens: Arc<TokenRegistry>,
    /// The littleguy daemons to stop at exit.
    pub littleguy: Vec<Arc<LittleguyMind>>,
    /// The supervisor step interval.
    pub step: Duration,
}

fn random_hex(bytes: usize) -> anyhow::Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    Ok(buf
        .iter()
        .fold(String::with_capacity(bytes * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        }))
}

/// Loads the world and builds the supervisor and its minds.
///
/// # Errors
/// The pack or the store fails to open, or a mind cannot be built.
pub fn build(cfg: &MudConfig) -> anyhow::Result<Built> {
    let pack = Arc::new(match &cfg.world.pack {
        Some(dir) => WorldPack::load(dir)?,
        None => WorldPack::crypt_v01()?,
    });
    let game = Game::open(
        Arc::clone(&pack),
        cfg.world.db.as_deref(),
        cfg.world.seed,
        Arc::new(SystemClock),
    )
    .context("opening the world")?;
    let typing = Gate::new(
        Arc::clone(&game),
        cfg.world.seed,
        cfg.supervisor.typing_scale,
    );
    let tokens = Arc::new(TokenRegistry::new());
    let minds = minds::build(cfg, &pack, &tokens)?;
    let s = &cfg.supervisor;
    let sup_cfg = SupervisorConfig {
        seed: cfg.world.seed,
        daily_token_budget: (s.daily_token_budget > 0).then_some(s.daily_token_budget),
        budgets: s
            .budgets
            .iter()
            .map(|(k, v)| (EntityId::new(k.clone()), *v))
            .collect(),
        macro_tick_scale: s.macro_tick_scale,
        first_macro_tick_secs: s.first_macro_tick_secs,
        ..SupervisorConfig::default()
    };
    let sup = Supervisor::new(
        Arc::clone(&game),
        typing,
        minds.by_entity,
        Arc::new(FallbackMind::from_cast(&pack.cast)),
        sup_cfg,
    );
    let admin_token = match std::env::var(&cfg.server.admin_token_env) {
        Ok(t) if t.len() >= 16 => t,
        _ => random_hex(16)?,
    };
    Ok(Built {
        mud: Arc::new(Mud {
            game,
            sup,
            presence: Arc::new(Presence::default()),
            admin_token,
        }),
        tokens,
        littleguy: minds.littleguy,
        step: Duration::from_millis(s.step_ms.max(10)),
    })
}

/// The autumn-web settings: the bind, no CSRF (no route trusts a cookie;
/// the observer and MCP take tokens), and a fresh signing secret.
///
/// # Errors
/// No randomness for the secret.
pub fn settings(cfg: &MudConfig) -> anyhow::Result<AutumnConfig> {
    let mut c = AutumnConfig::default();
    c.server.host = cfg.server.bind.ip().to_string();
    c.server.port = cfg.server.bind.port();
    c.server.prestop_grace_secs = 0;
    c.aeo.enabled = false;
    c.security.csrf.enabled = false;
    c.security.signing_secret.secret = Some(random_hex(32)?);
    Ok(c)
}

struct Fixed(AutumnConfig);

impl ConfigLoader for Fixed {
    fn load(&self) -> impl Future<Output = Result<AutumnConfig, ConfigError>> + Send {
        let config = self.0.clone();
        async move { Ok(config) }
    }
}

/// The binary installs its own tracing subscriber.
struct Quiet;

impl TelemetryProvider for Quiet {
    fn init(
        &self,
        _log: &LogConfig,
        _t: &TelemetryConfig,
        _p: Option<&str>,
    ) -> Result<TelemetryGuard, TelemetryInitError> {
        Ok(TelemetryGuard::disabled())
    }
}

/// The app: the routes, `/mcp`, and the shared state.
#[must_use]
pub fn app(built: &Built, settings: AutumnConfig) -> AppBuilder {
    let mud = Arc::clone(&built.mud);
    let mcp = mud_mcp::router::<autumn_web::AppState>(
        Arc::clone(&built.mud.game),
        Arc::clone(&built.tokens),
        Arc::new(TypingGate(Arc::clone(built.mud.sup.gate()))),
    );
    autumn_web::app()
        .with_config_loader(Fixed(settings))
        .with_telemetry_provider(Quiet)
        .routes(web::routes())
        .merge(mcp)
        .state_initializer(move |state| state.insert_extension((*mud).clone()))
}

/// Runs the server until a signal stops it.
///
/// # Errors
/// The world, a mind, or a listener fails to start.
pub async fn serve(cfg: MudConfig) -> anyhow::Result<()> {
    let built = build(&cfg)?;
    let settings = settings(&cfg)?;
    tracing::info!(
        terminal = %format!("http://{}/", cfg.server.bind),
        observer = %format!("http://{}/observer?token={}", cfg.server.bind, built.mud.admin_token),
        mcp = %cfg.server.mcp_url(),
        "the MUD is up"
    );
    let mut tasks = built.mud.sup.spawn(built.step);
    if let Some(addr) = cfg.server.telnet {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding telnet on {addr}"))?;
        tracing::info!(%addr, "telnet is up");
        tasks.push(tokio::spawn(telnet::serve(
            listener,
            Arc::clone(&built.mud.game),
            Arc::clone(&built.mud.presence),
        )));
    }
    Box::pin(app(&built, settings).run()).await;
    for t in tasks {
        t.abort();
    }
    for lg in &built.littleguy {
        lg.shutdown().await;
    }
    Ok(())
}

/// Reads `path` if it exists, or the defaults.
///
/// # Errors
/// The file exists but does not parse.
pub fn load_config(path: &Path) -> anyhow::Result<MudConfig> {
    if path.exists() {
        MudConfig::load(path)
    } else {
        Ok(MudConfig::default())
    }
}
