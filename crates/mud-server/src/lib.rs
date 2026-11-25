//! The littleguy MUD server.
//!
//! One process owns the world and the lifecycle of every mind. It serves
//! the browser terminal and the observer dashboard on autumn-web, plain
//! telnet, and the MCP endpoint that littleguy agents (or any MCP client)
//! use to play. The supervisor runs alongside and decides when each mind
//! thinks.

pub mod account_cli;
pub mod config;
pub mod limits;
pub mod logging;
pub mod login;
pub mod minds;
pub mod session;
pub mod smoke;
pub mod telnet;
#[cfg(test)]
pub(crate) mod test_support;
pub mod web;

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use autumn_web::app::AppBuilder;
use autumn_web::config::{AutumnConfig, ConfigError, ConfigLoader, LogConfig, TelemetryConfig};
use autumn_web::telemetry::{TelemetryGuard, TelemetryInitError, TelemetryProvider};
use mud_accounts::Accounts;
use mud_mcp::{CommandGate, TokenRegistry};
use mud_script::{ScriptError, ScriptHost, ScriptSources};
use mud_supervisor::gate::Gate;
use mud_supervisor::littleguy::LittleguyMind;
use mud_supervisor::runtime::{Supervisor, SupervisorConfig};
use mud_supervisor::scripted::FallbackMind;
use mud_world::hooks::WorldHooks;
use mud_world::{Audit, CommandResult, EntityId, Game, HooksChange, SystemClock, WorldPack};

use crate::config::MudConfig;
use crate::limits::ConnLimiter;
use crate::session::Presence;
use crate::web::Mud;

/// The largest WebSocket message a terminal may send, in bytes.
pub const MAX_WS_MESSAGE_BYTES: usize = 4_096;

/// The default cap on open terminal and telnet connections together.
pub const MAX_CONNECTIONS: usize = 512;

/// The default cap on open connections from one client address.
pub const MAX_CONNECTIONS_PER_ADDRESS: usize = 8;

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

/// The MCP wake booker: `mud_wake_me` books in the supervisor's
/// [`mud_supervisor::wakes::WakeBook`], on the game clock, as in-process
/// minds do.
#[derive(Debug, Clone)]
pub struct SupervisorWakes(pub Arc<Supervisor>);

impl mud_mcp::WakeBooker for SupervisorWakes {
    fn wake_me(&self, entity: &EntityId, minutes: u32, note: &str) -> serde_json::Value {
        let now = self.0.game().now_ms();
        mud_supervisor::mind::book_wake(self.0.wakes(), entity, now, minutes, note)
    }

    fn cancel_wake(&self, entity: &EntityId, id: u64) -> serde_json::Value {
        mud_supervisor::mind::cancel_wake(self.0.wakes(), entity, id)
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
    /// Whether the admin token was made at startup (and so may be logged)
    /// rather than given by the operator.
    pub admin_token_generated: bool,
}

/// The observer token: the operator's, if given and at least 16 bytes
/// long, or a fresh random one. Returns it and whether it was generated.
fn admin_token(given: Option<String>) -> anyhow::Result<(String, bool)> {
    match given {
        Some(t) if t.len() >= 16 => Ok((t, false)),
        _ => Ok((random_hex(16)?, true)),
    }
}

/// The observer URL for the startup log. It carries the token only when
/// the server generated it; an operator's token stays out of the logs.
#[must_use]
pub fn observer_log_url(bind: std::net::SocketAddr, token: &str, generated: bool) -> String {
    if generated {
        format!("http://{bind}/observer?token={}", web::url_escape(token))
    } else {
        format!("http://{bind}/observer")
    }
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

/// Loads, checks and dry-runs the pack's scripts.
///
/// # Errors
/// Any problem: a server never starts with a broken script.
pub fn load_scripts(cfg: &MudConfig, pack: &Arc<WorldPack>) -> anyhow::Result<ScriptHost> {
    let host = match &cfg.world.pack {
        Some(dir) => ScriptHost::load(dir, pack)?,
        None => ScriptHost::bundled_crypt_v01(pack)?,
    };
    let problems = host.dry_run(pack);
    if !problems.is_empty() {
        return Err(ScriptError::Invalid(problems).into());
    }
    for w in &host.current().warnings {
        tracing::warn!("{w}");
    }
    Ok(host)
}

/// Opens the accounts beside the world.
///
/// It logs what an operator should know: a low bcrypt cost, the legacy
/// characters, and a new `accounts.db` next to a world that already has
/// humans (a lost `accounts.db` makes every human claimable again).
///
/// # Errors
/// The config is out of bounds, or `accounts.db` cannot be opened.
pub async fn open_accounts(cfg: &MudConfig, game: &Game) -> anyhow::Result<Accounts> {
    let (path, acfg) = cfg.accounts.to_config(cfg.world.db.as_deref())?;
    if acfg.bcrypt_cost < 10 {
        tracing::warn!(
            "[accounts] bcrypt_cost = {} is for tests; use 10 or more on a real server",
            acfg.bcrypt_cost
        );
    }
    let mode = acfg.legacy;
    let accounts = Accounts::open(path.as_deref(), acfg, 1_000, game.now_ms())
        .await
        .context("opening the accounts")?;
    let humans = game.human_ids();
    let legacy = accounts.legacy_count(&humans).await?;
    tracing::info!(
        "{legacy} characters have no password yet; [accounts] legacy = {}",
        mode.as_str()
    );
    if accounts.created_new() && !humans.is_empty() {
        let until = accounts
            .legacy_closes_ms()
            .map_or_else(|| "the window closes (never)".to_owned(), account_cli::utc);
        let until = if accounts.legacy_open(game.now_ms()) {
            until
        } else {
            "nobody: legacy is sealed".to_owned()
        };
        tracing::warn!(
            "accounts.db is new and the world has {} human characters; they are claimable until {until}. If accounts.db was lost, set [accounts] legacy = \"sealed\". Back up world.db and accounts.db together.",
            humans.len()
        );
    }
    Ok(accounts)
}

/// Loads the world and builds the supervisor and its minds.
///
/// # Errors
/// The pack, its scripts, the store or the accounts fail to open, a mind
/// cannot be built, or the proxy setup would merge every player.
pub async fn build(cfg: &MudConfig) -> anyhow::Result<Built> {
    cfg.check_proxy()?;
    cfg.supervisor.check_restart()?;
    cfg.minds.harvest.history()?;
    for w in cfg.plaintext_warnings() {
        tracing::warn!("{w}");
    }
    let pack = Arc::new(match &cfg.world.pack {
        Some(dir) => WorldPack::load(dir)?,
        None => WorldPack::crypt_v01()?,
    });
    let scripts = if cfg.scripts.enabled {
        Some(Arc::new(load_scripts(cfg, &pack)?))
    } else {
        None
    };
    let retention = cfg.world.retention()?;
    // A world.db newer than this server is refused here, unchanged.
    let game = Game::open(
        Arc::clone(&pack),
        cfg.world.db.as_deref(),
        cfg.world.seed,
        Arc::new(SystemClock),
    )
    .context("opening the world")?;
    if let Some(r) = retention {
        game.set_retention(r);
    }
    let hooks = scripts.clone().map(|h| h as Arc<dyn WorldHooks>);
    game.write(|w, now| w.set_hooks(hooks, now, HooksChange::Start));
    if let Some(h) = &scripts {
        tracing::info!(scripts = %h.summary(), "world scripts loaded");
    }
    let typing = Gate::new(
        Arc::clone(&game),
        cfg.world.seed,
        cfg.supervisor.typing_scale,
    );
    let tokens = Arc::new(TokenRegistry::new());
    let minds = minds::build(
        cfg,
        &pack,
        &tokens,
        scripts
            .as_ref()
            .map(|host| minds::MindScripts { host, game: &game }),
    )?;
    let s = &cfg.supervisor;
    let sup_cfg = SupervisorConfig {
        seed: cfg.world.seed,
        daily_token_budget: (s.daily_token_budget > 0).then_some(s.daily_token_budget),
        budgets: budgets(&pack, &s.budgets)?,
        macro_tick_scale: s.macro_tick_scale,
        first_macro_tick_secs: s.first_macro_tick_secs,
        replay_max_age_ms: s.replay_max_age_ms,
        booking_max_late_ms: s.booking_max_late_ms,
        catchup_spread_ms: s.catchup_spread_ms,
        ..SupervisorConfig::default()
    };
    // The open order (ADR 0002, ADR 0011): the world and its side rows
    // first, then the supervisor restores the rows (bookings, the macro
    // schedule, rule counters, plans to resume).
    let restored = game.take_restored().unwrap_or_default();
    let sup = Supervisor::open(
        Arc::clone(&game),
        typing,
        minds.by_entity,
        Arc::new(FallbackMind::from_cast(&pack.cast)),
        sup_cfg,
        restored,
    );
    let (admin_token, admin_token_generated) =
        admin_token(std::env::var(&cfg.server.admin_token_env).ok())?;
    let accounts = Arc::new(open_accounts(cfg, &game).await?);
    Ok(Built {
        mud: Arc::new(Mud {
            game,
            sup,
            presence: Arc::new(Presence::default()),
            admin_token,
            timeouts: cfg.server.timeouts(),
            limiter: ConnLimiter::new(
                cfg.server.max_connections,
                cfg.server.max_connections_per_address,
            ),
            shutdown: tokio_util::sync::CancellationToken::new(),
            littleguy: minds.littleguy.clone(),
            scripts,
            accounts,
            public_url: cfg.server.public_url.clone(),
            trusted_proxies: !cfg.server.trusted_proxies.is_empty(),
        }),
        admin_token_generated,
        tokens,
        littleguy: minds.littleguy,
        step: Duration::from_millis(s.step_ms.max(10)),
    })
}

/// A reload that took.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ReloadOk {
    /// The new version.
    pub version: String,
    /// The version before.
    pub previous: String,
    /// How many files it has.
    pub files: usize,
}

/// A reload that did not take; the old scripts keep running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadError {
    /// `[scripts] enabled = false`.
    Disabled,
    /// The new scripts have problems.
    Invalid {
        /// The version still running.
        version: String,
        /// Every problem.
        problems: Vec<String>,
    },
}

/// Reloads the scripts from the pack folder.
///
/// It compiles, validates and
/// dry-runs them off the async workers, then swaps them in between two
/// commands. Every successful reload reopens every hook and mind breaker
/// and records a marker event, even with unchanged files. The bundled
/// pack has nothing to read again: its reload re-arms the scripts running
/// now, which only reopens the breakers and records the marker.
///
/// A refused reload is logged and published to the observer's feed.
///
/// # Errors
/// Scripts are off, or the new ones have problems.
pub async fn reload_scripts(mud: &Mud) -> Result<ReloadOk, ReloadError> {
    let Some(host) = &mud.scripts else {
        return Err(ReloadError::Disabled);
    };
    let _one_at_a_time = host.reload_lock().lock().await;
    let previous = host.version();
    let hooks = Arc::clone(host) as Arc<dyn WorldHooks>;
    if !host.reloadable() {
        let files = host.current().files;
        mud.game.write(|w, now| {
            host.rearm();
            w.set_hooks(Some(hooks), now, HooksChange::Reload);
        });
        tracing::info!(version = %previous, "bundled scripts re-armed; breakers reset");
        return Ok(ReloadOk {
            version: previous.clone(),
            previous,
            files,
        });
    }
    let pack = mud.game.read(|w| Arc::clone(w.pack()));
    let compiling = Arc::clone(host);
    let compiled = tokio::task::spawn_blocking(move || compiling.compile_from_source(&pack))
        .await
        .map_err(|e| vec![format!("the reload task failed: {e}")])
        .and_then(|r| r.map_err(|e| e.problems()));
    let compiled = match compiled {
        Ok(c) => c,
        Err(problems) => {
            tracing::warn!(version = %previous, ?problems, "scripts reload refused");
            mud.game.publish_audit(Audit::ScriptsReloadRefused {
                version: previous.clone(),
                problems: problems.clone(),
                at_ms: mud.game.now_ms(),
            });
            return Err(ReloadError::Invalid {
                version: previous,
                problems,
            });
        }
    };
    let ok = ReloadOk {
        version: compiled.version.clone(),
        previous,
        files: compiled.files,
    };
    mud.game.write(|w, now| {
        host.install(compiled);
        w.set_hooks(Some(hooks), now, HooksChange::Reload);
    });
    tracing::info!(version = %ok.version, previous = %ok.previous, "scripts reloaded");
    Ok(ok)
}

/// `mud check`: checks a pack and its scripts, and dry-runs every script.
/// Returns the lines to print.
///
/// # Errors
/// The pack does not load, or a script has a problem; the message lists
/// every problem.
pub fn check_pack(dir: &Path) -> anyhow::Result<Vec<String>> {
    let p = Arc::new(WorldPack::load(dir)?);
    let mut out = vec![format!(
        "{}: {} rooms, {} items, {} skills, {} cast, {} shops, {} reward pool{} — ok",
        p.world.name,
        p.rooms.len(),
        p.items.len(),
        p.skills.len(),
        p.cast.len(),
        p.economy.shops.len(),
        p.economy.rewards.len(),
        if p.economy.rewards.len() == 1 {
            ""
        } else {
            "s"
        }
    )];
    let sources = ScriptSources::read_dir(dir)?;
    let compiled = mud_script::load::compile(&sources, &p)?;
    let problems = compiled.dry_run(&p);
    if !problems.is_empty() {
        return Err(ScriptError::Invalid(problems).into());
    }
    if sources.manifest.is_some() {
        out.push(format!("scripts {} — ok", compiled.summary()));
    } else {
        out.push("scripts: none (no scripts.toml)".to_owned());
    }
    out.extend(compiled.warnings.iter().map(|w| format!("warning: {w}")));
    Ok(out)
}

/// The per-entity budgets, keyed by entity id. A key the cast does not
/// name (a typo, or an id a pack edit renamed) fails loudly: ignoring it
/// would silently drop that agent to the default budget.
fn budgets(
    pack: &WorldPack,
    given: &BTreeMap<String, u64>,
) -> anyhow::Result<HashMap<EntityId, Option<u64>>> {
    let unknown: Vec<&str> = given
        .keys()
        .filter(|k| !pack.cast.contains_key(&EntityId::new((*k).clone())))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        anyhow::bail!(
            "[supervisor.budgets] names entities the cast does not have: {}",
            unknown.join(", ")
        );
    }
    // 0 is no limit, as it is for `daily_token_budget`.
    Ok(given
        .iter()
        .map(|(k, v)| (EntityId::new(k.clone()), (*v > 0).then_some(*v)))
        .collect())
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
    // A terminal line is one command; nothing legitimate is near 4 KiB.
    c.realtime.max_message_bytes = Some(MAX_WS_MESSAGE_BYTES);
    // A backstop: the shared limiter in `Mud` admits terminal sockets.
    c.realtime.max_connections = Some(cfg.server.max_connections);
    if !cfg.server.trusted_proxies.is_empty() {
        c.security
            .trusted_proxies
            .ranges
            .clone_from(&cfg.server.trusted_proxies);
        c.security.trusted_proxies.trust_forwarded_headers = true;
    }
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
    let mcp = mud_mcp::router_until::<autumn_web::AppState>(
        Arc::clone(&built.mud.game),
        Arc::clone(&built.tokens),
        Arc::new(TypingGate(Arc::clone(built.mud.sup.gate()))),
        Arc::new(SupervisorWakes(Arc::clone(&built.mud.sup))),
        built.mud.shutdown.child_token(),
    );
    autumn_web::app()
        .with_config_loader(Fixed(settings))
        .with_telemetry_provider(Quiet)
        .routes(web::routes())
        .merge(mcp)
        .state_initializer(move |state| install(state, &mud))
}

/// Puts `mud` in the app state and ties its shutdown token to autumn's:
/// autumn cancels its own token as shutdown begins, before it waits for
/// open responses to finish, and `mud.shutdown` follows at once.
pub fn install(state: &autumn_web::AppState, mud: &Mud) {
    state.insert_extension(mud.clone());
    let begun = state.shutdown_token();
    let ours = mud.shutdown.clone();
    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        rt.spawn(async move {
            begun.cancelled().await;
            ours.cancel();
        });
    }
}

/// Stops the minds once `shutdown` is cancelled.
///
/// The supervisor's tasks stop first (so no wake starts a new daemon),
/// then every littleguy daemon is killed and reaped. This runs as soon as
/// shutdown begins, not after the HTTP server has drained, because each
/// daemon holds an MCP session open and the drain would wait on it.
pub async fn stop_on(
    shutdown: tokio_util::sync::CancellationToken,
    tasks: Vec<tokio::task::AbortHandle>,
    littleguy: Vec<Arc<LittleguyMind>>,
) {
    shutdown.cancelled().await;
    tracing::info!("shutting down: stopping the supervisor and every littleguy daemon");
    for t in tasks {
        t.abort();
    }
    for lg in &littleguy {
        lg.shutdown().await;
    }
}

/// Runs the server until a signal stops it.
///
/// # Errors
/// The world, a mind, or a listener fails to start.
pub async fn serve(cfg: MudConfig) -> anyhow::Result<()> {
    let built = build(&cfg).await?;
    let settings = settings(&cfg)?;
    tracing::info!(
        terminal = %format!("http://{}/", cfg.server.bind),
        observer = %observer_log_url(cfg.server.bind, &built.mud.admin_token, built.admin_token_generated),
        admin_token_from = if built.admin_token_generated { "generated" } else { cfg.server.admin_token_env.as_str() },
        mcp = %cfg.server.mcp_url(),
        "the MUD is up"
    );
    let mut tasks = built.mud.sup.spawn(built.step);
    if let Some(addr) = cfg.server.telnet {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding telnet on {addr}"))?;
        tracing::info!(%addr, "telnet is up");
        tasks.push(tokio::spawn(telnet::serve_limited(
            Arc::clone(&built.mud.limiter),
            listener,
            telnet::Shared {
                game: Arc::clone(&built.mud.game),
                presence: Arc::clone(&built.mud.presence),
                accounts: Arc::clone(&built.mud.accounts),
                timeouts: cfg.server.timeouts(),
            },
        )));
    }
    let stopper = tokio::spawn(stop_on(
        built.mud.shutdown.clone(),
        tasks
            .iter()
            .map(tokio::task::JoinHandle::abort_handle)
            .collect(),
        built.littleguy.clone(),
    ));
    Box::pin(app(&built, settings).run()).await;
    // However the server ended, nothing outlives it.
    built.mud.shutdown.cancel();
    let _ = stopper.await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_operator_token_is_kept_and_a_short_or_missing_one_replaced() {
        let given = "abcdefabcdef0123+xyz".to_owned();
        assert_eq!(admin_token(Some(given.clone())).unwrap(), (given, false));
        for given in [None, Some("short".to_owned())] {
            let (t, generated) = admin_token(given).unwrap();
            assert!(generated);
            assert_eq!(t.len(), 32);
        }
    }

    #[test]
    fn a_budget_for_an_entity_the_cast_lacks_is_refused() {
        let pack = WorldPack::crypt_v01().unwrap();
        let stale = BTreeMap::from([("kestrel".to_owned(), 600_000), ("torin".to_owned(), 1)]);
        let err = budgets(&pack, &stale).unwrap_err().to_string();
        assert!(err.contains("kestrel"), "{err}");
        assert!(!err.contains("torin"), "{err}");
        let good = BTreeMap::from([("pc_kestrel".to_owned(), 600_000), ("torin".to_owned(), 0)]);
        assert_eq!(
            budgets(&pack, &good).unwrap(),
            HashMap::from([
                (EntityId::new("pc_kestrel"), Some(600_000)),
                (EntityId::new("torin"), None),
            ]),
            "0 is no limit, as for daily_token_budget"
        );
    }

    #[test]
    fn trusted_proxies_turn_on_forwarded_client_addresses() {
        let mut cfg = MudConfig::default();
        let plain = settings(&cfg).unwrap();
        assert!(!plain.security.trusted_proxies.trust_forwarded_headers);
        assert_eq!(plain.realtime.max_connections, Some(MAX_CONNECTIONS));
        cfg.server.trusted_proxies = vec!["10.0.0.0/8".into()];
        cfg.server.max_connections = 40;
        let proxied = settings(&cfg).unwrap();
        assert!(proxied.security.trusted_proxies.trust_forwarded_headers);
        assert_eq!(proxied.security.trusted_proxies.ranges, ["10.0.0.0/8"]);
        assert_eq!(proxied.realtime.max_connections, Some(40));
    }

    #[tokio::test]
    async fn mcp_wakes_book_in_the_supervisor_on_the_game_clock() {
        use mud_mcp::WakeBooker as _;
        let mut cfg = MudConfig::default();
        cfg.world.db = None;
        let built = build(&cfg).await.unwrap();
        let torin = EntityId::from("torin");
        let wakes = SupervisorWakes(Arc::clone(&built.mud.sup));
        let before = built.mud.game.now_ms();
        let r = wakes.wake_me(&torin, 30, "is the ore in yet?");
        let after = built.mud.game.now_ms();
        assert_eq!(r["ok"], true, "{r}");
        let pending = built.mud.sup.wakes().pending(&torin);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].note, "is the ore in yet?");
        assert!(
            (before + 30 * 60_000..=after + 30 * 60_000).contains(&pending[0].due_ms),
            "30 minutes on the game clock"
        );
        let id = r["booked"]["id"].as_u64().unwrap();
        assert_eq!(wakes.cancel_wake(&torin, id)["ok"], true);
        assert!(
            built.mud.sup.wakes().pending(&torin).is_empty(),
            "cancelled"
        );
        assert_eq!(wakes.wake_me(&torin, 0, "x")["ok"], false);
    }

    #[tokio::test]
    async fn stopping_aborts_the_supervisor_and_closes_every_littleguy() {
        let dir = tempfile::tempdir().unwrap();
        let lg = Arc::new(LittleguyMind::new(
            mud_supervisor::littleguy::LittleguyConfig {
                bin: dir.path().join("no-such-lg"),
                data_dir: dir.path().to_path_buf(),
                ..Default::default()
            },
            std::collections::HashMap::new(),
        ));
        let forever = tokio::spawn(std::future::pending::<()>());
        let shutdown = tokio_util::sync::CancellationToken::new();
        let stopper = tokio::spawn(stop_on(
            shutdown.clone(),
            vec![forever.abort_handle()],
            vec![Arc::clone(&lg)],
        ));
        tokio::task::yield_now().await;
        assert!(!lg.is_closed(), "nothing stops before the token");
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(3), stopper)
            .await
            .unwrap()
            .unwrap();
        assert!(lg.is_closed());
        assert!(forever.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn a_new_accounts_db_beside_a_world_with_humans_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = MudConfig::default();
        cfg.world.db = Some(dir.path().join("world.db"));
        cfg.accounts.bcrypt_cost = 4;
        let game = Game::open(
            Arc::new(WorldPack::crypt_v01().unwrap()),
            cfg.world.db.as_deref(),
            1,
            Arc::new(mud_world::ManualClock::new(1_800_000_000_000)),
        )
        .unwrap();
        game.login("old").unwrap();
        let a = open_accounts(&cfg, &game).await.unwrap();
        assert!(a.created_new());
        assert!(dir.path().join("accounts.db").exists());
        assert_eq!(a.legacy_count(&game.human_ids()).await.unwrap(), 1);
        drop(a);
        let again = open_accounts(&cfg, &game).await.unwrap();
        assert!(!again.created_new());
        cfg.server.public_url = Some("https://mud.example".into());
        assert!(
            build(&cfg)
                .await
                .unwrap_err()
                .to_string()
                .contains("trusted_proxies is empty")
        );
    }

    #[tokio::test]
    async fn the_world_section_sets_retention_and_a_newer_world_db_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = MudConfig::default();
        let db = dir.path().join("world.db");
        cfg.world.db = Some(db.clone());
        cfg.accounts.bcrypt_cost = 4;
        cfg.world.events_keep_days = 30;
        cfg.world.archive_dir = Some(dir.path().join("arch"));
        let built = build(&cfg).await.unwrap();
        let r = built.mud.game.retention().unwrap();
        assert_eq!(r.keep_days, 30);
        assert_eq!(r.archive_dir, dir.path().join("arch"));
        assert_eq!(built.mud.game.boot(), 1);
        drop(built);
        // A bad bound is refused before the world opens.
        cfg.world.retention_batch = 0;
        let err = build(&cfg).await.unwrap_err().to_string();
        assert!(err.contains("retention_batch"), "{err}");
        cfg.world.retention_batch = 5_000;
        // A file a newer server wrote is refused, and left as it was.
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
        }
        let err = format!("{:#}", build(&cfg).await.unwrap_err());
        assert!(err.contains("opening the world"), "{err}");
        assert!(err.contains("schema version 2"), "{err}");
        let conn = rusqlite::Connection::open(&db).unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 2);
    }

    #[test]
    fn only_a_generated_token_is_logged_and_it_is_encoded() {
        let bind: std::net::SocketAddr = "127.0.0.1:4000".parse().unwrap();
        assert_eq!(
            observer_log_url(bind, "abc+def&x", true),
            "http://127.0.0.1:4000/observer?token=abc%2Bdef%26x"
        );
        let url = observer_log_url(bind, "operator-secret-123", false);
        assert_eq!(url, "http://127.0.0.1:4000/observer");
        assert!(!url.contains("operator-secret"));
    }
}
