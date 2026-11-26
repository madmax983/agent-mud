//! `mud.toml`: what to serve, which world, and which minds.
//!
//! Every section has a default, so `mud serve` runs with no file at all:
//! the bundled world, scripted minds, no model keys.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use mud_accounts::{AccountsConfig, Legacy, ThrottlePolicy};
use serde::{Deserialize, Serialize};

/// The whole config.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MudConfig {
    /// Network.
    pub server: ServerConfig,
    /// The world.
    pub world: WorldConfig,
    /// Which runtime each tier uses.
    pub minds: MindsConfig,
    /// Model aliases (the `model` of a cast entry), for harvest and
    /// plugin-agent minds.
    pub models: BTreeMap<String, ModelConfig>,
    /// littleguy, when a tier uses it.
    pub littleguy: LittleguySection,
    /// Budgets and timings.
    pub supervisor: SupervisorSection,
    /// World scripts.
    pub scripts: ScriptsSection,
    /// Player accounts.
    pub accounts: AccountsSection,
}

/// One throttle's knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThrottleKnobs {
    /// Failures that never delay.
    pub free: u32,
    /// The first delay, in ms.
    pub base_ms: u64,
    /// The longest delay, in ms (at most a day).
    pub max_ms: u64,
    /// A quiet spell this long forgets every failure, in ms.
    pub forget_ms: u64,
}

impl From<ThrottlePolicy> for ThrottleKnobs {
    fn from(p: ThrottlePolicy) -> Self {
        Self {
            free: p.free,
            base_ms: p.base_ms,
            max_ms: p.max_ms,
            forget_ms: p.forget_ms,
        }
    }
}

impl ThrottleKnobs {
    fn policy(self, which: &str) -> anyhow::Result<ThrottlePolicy> {
        anyhow::ensure!(
            self.base_ms >= 1 && self.base_ms <= self.max_ms,
            "[accounts.throttle.{which}] base_ms must be at least 1 and at most max_ms"
        );
        anyhow::ensure!(
            self.max_ms <= 86_400_000,
            "[accounts.throttle.{which}] max_ms must be at most 86400000"
        );
        anyhow::ensure!(
            self.forget_ms > self.max_ms,
            "[accounts.throttle.{which}] forget_ms must exceed max_ms"
        );
        Ok(ThrottlePolicy {
            free: self.free,
            base_ms: self.base_ms,
            max_ms: self.max_ms,
            forget_ms: self.forget_ms,
        })
    }
}

/// `[accounts.throttle]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThrottleSection {
    /// One name from one address: always applies.
    pub name_address: ThrottleKnobs,
    /// One name from addresses it never logged in from.
    pub name: ThrottleKnobs,
    /// One address over names it never logged in as.
    pub address: ThrottleKnobs,
    /// bcrypt checks from one address, pass or fail.
    pub work: ThrottleKnobs,
}

impl Default for ThrottleSection {
    fn default() -> Self {
        let d = mud_accounts::throttle::ThrottlePolicies::default();
        Self {
            name_address: d.name_address.into(),
            name: d.name.into(),
            address: d.address.into(),
            work: d.work.into(),
        }
    }
}

/// `[accounts]`: passwords, sessions and the login limits (ADR 0009).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccountsSection {
    /// `accounts.db`. Unset: beside `[world] db`, in memory when the world
    /// is.
    pub db: Option<PathBuf>,
    /// bcrypt cost, 4 to 16; below 10 is for tests.
    pub bcrypt_cost: u32,
    /// The shortest new password, 8 to 64 characters.
    pub min_password_chars: usize,
    /// Refuse autumn-web's 10k weak passwords.
    pub reject_common_passwords: bool,
    /// Whether new characters may be made.
    pub allow_new: bool,
    /// New characters per address per UTC day (an IPv6 `/48` counts as
    /// one); 0: no limit.
    pub new_characters_per_address_per_day: u32,
    /// New characters per UTC day across addresses; 0: no limit.
    pub new_characters_per_day: u32,
    /// Characters saved before accounts: `claim` or `sealed`.
    pub legacy: Legacy,
    /// Days after which `claim` acts as `sealed`; 0: never closes.
    pub legacy_claim_days: u32,
    /// "Stay signed in" days; 0 turns it off (0 to 365).
    pub remember_days: u32,
    /// A tab token's life after its last activity, in minutes; 0: off
    /// (0 to 1440).
    pub reconnect_minutes: u32,
    /// Remembered browsers per character (1 to 50).
    pub max_remembered: u32,
    /// Recent login addresses per character that skip the shared limits
    /// (0 to 20).
    pub known_addresses: u32,
    /// Wrong passwords per connection before it closes (1 to 10).
    pub max_password_attempts: u32,
    /// bcrypt checks at once (1 to 64).
    pub hash_concurrency: u32,
    /// How often live sessions re-check locks and password changes, in
    /// seconds (5 to 3600).
    pub revalidate_secs: u32,
    /// The throttles.
    pub throttle: ThrottleSection,
}

impl Default for AccountsSection {
    fn default() -> Self {
        let d = AccountsConfig::default();
        Self {
            db: None,
            bcrypt_cost: d.bcrypt_cost,
            min_password_chars: d.min_password_chars,
            reject_common_passwords: d.reject_common_passwords,
            allow_new: d.allow_new,
            new_characters_per_address_per_day: d.new_characters_per_address_per_day,
            new_characters_per_day: d.new_characters_per_day,
            legacy: d.legacy,
            legacy_claim_days: d.legacy_claim_days,
            remember_days: d.remember_days,
            reconnect_minutes: d.reconnect_minutes,
            max_remembered: d.max_remembered,
            known_addresses: d.known_addresses,
            max_password_attempts: d.max_password_attempts,
            hash_concurrency: d.hash_concurrency,
            revalidate_secs: d.revalidate_secs,
            throttle: ThrottleSection::default(),
        }
    }
}

impl AccountsSection {
    /// The checked account config, and where `accounts.db` lives: `db`,
    /// else beside `world_db`, else in memory.
    ///
    /// # Errors
    /// A value is out of bounds; the message names it.
    pub fn to_config(
        &self,
        world_db: Option<&Path>,
    ) -> anyhow::Result<(Option<PathBuf>, AccountsConfig)> {
        use anyhow::ensure;
        ensure!(
            (4..=16).contains(&self.bcrypt_cost),
            "[accounts] bcrypt_cost must be 4 to 16, not {}",
            self.bcrypt_cost
        );
        ensure!(
            (8..=64).contains(&self.min_password_chars),
            "[accounts] min_password_chars must be 8 to 64, not {}",
            self.min_password_chars
        );
        ensure!(
            self.remember_days <= 365,
            "[accounts] remember_days must be at most 365"
        );
        ensure!(
            self.reconnect_minutes <= 1_440,
            "[accounts] reconnect_minutes must be at most 1440"
        );
        ensure!(
            (5..=3_600).contains(&self.revalidate_secs),
            "[accounts] revalidate_secs must be 5 to 3600"
        );
        ensure!(
            self.reconnect_minutes == 0
                || u64::from(self.reconnect_minutes) * 60 >= 2 * u64::from(self.revalidate_secs),
            "[accounts] reconnect_minutes must be 0 or at least twice revalidate_secs"
        );
        ensure!(
            (1..=50).contains(&self.max_remembered),
            "[accounts] max_remembered must be 1 to 50"
        );
        ensure!(
            self.known_addresses <= 20,
            "[accounts] known_addresses must be at most 20"
        );
        ensure!(
            (1..=10).contains(&self.max_password_attempts),
            "[accounts] max_password_attempts must be 1 to 10"
        );
        ensure!(
            (1..=64).contains(&self.hash_concurrency),
            "[accounts] hash_concurrency must be 1 to 64"
        );
        let t = &self.throttle;
        let throttle = mud_accounts::ThrottleConfig {
            name_address: t.name_address.policy("name_address")?,
            name: t.name.policy("name")?,
            address: t.address.policy("address")?,
            work: t.work.policy("work")?,
        };
        let path = self
            .db
            .clone()
            .or_else(|| world_db.map(mud_accounts::store::beside));
        Ok((
            path,
            AccountsConfig {
                bcrypt_cost: self.bcrypt_cost,
                min_password_chars: self.min_password_chars,
                reject_common_passwords: self.reject_common_passwords,
                allow_new: self.allow_new,
                new_characters_per_address_per_day: self.new_characters_per_address_per_day,
                new_characters_per_day: self.new_characters_per_day,
                legacy: self.legacy,
                legacy_claim_days: self.legacy_claim_days,
                remember_days: self.remember_days,
                reconnect_minutes: self.reconnect_minutes,
                max_remembered: self.max_remembered,
                known_addresses: self.known_addresses,
                max_password_attempts: self.max_password_attempts,
                hash_concurrency: self.hash_concurrency,
                revalidate_secs: self.revalidate_secs,
                throttle,
            },
        ))
    }
}

/// `[scripts]`. The limits that change outcomes are pack data, in the
/// pack's `scripts.toml`; this says only whether scripts run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScriptsSection {
    /// Whether the pack's scripts run. Off: no hook, no custom verb, no
    /// Rhai mind; the version `none` is recorded.
    pub enabled: bool,
    /// Whether a mind binding's `runtime = "rhai"` may put its entity on
    /// its Rhai script whatever `[minds]` maps its tier to.
    pub allow_runtime_override: bool,
}

impl Default for ScriptsSection {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_runtime_override: true,
        }
    }
}

/// `[server]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// The HTTP bind: terminal, observer, MCP.
    pub bind: SocketAddr,
    /// A telnet bind, if any.
    pub telnet: Option<SocketAddr>,
    /// The URL agents use to reach `/mcp`. Defaults to the bind.
    pub public_url: Option<String>,
    /// The variable that holds the observer token. Unset: one is made at
    /// start and logged.
    pub admin_token_env: String,
    /// How long a terminal or telnet client may take over the whole
    /// login, name to last password, in ms. `0`: no limit.
    pub login_timeout_ms: u64,
    /// How long a logged-in player may send nothing before the connection
    /// closes, in ms. `0`: no limit.
    pub idle_timeout_ms: u64,
    /// The most open terminal and telnet connections at once. `0`: no
    /// limit.
    pub max_connections: usize,
    /// The most open connections from one client address (an IPv6 `/64`
    /// counts as one). `0`: no limit.
    pub max_connections_per_address: usize,
    /// Reverse proxies (IPs or CIDR ranges) whose `X-Forwarded-For` names
    /// the real client of a web connection. Empty: the TCP peer is the
    /// client. Behind a proxy, set this, or every player shares the
    /// proxy's per-address share.
    pub trusted_proxies: Vec<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from(([127, 0, 0, 1], 3000)),
            telnet: None,
            public_url: None,
            admin_token_env: "MUD_ADMIN_TOKEN".into(),
            login_timeout_ms: 300_000,
            idle_timeout_ms: 3_600_000,
            max_connections: crate::MAX_CONNECTIONS,
            max_connections_per_address: crate::MAX_CONNECTIONS_PER_ADDRESS,
            trusted_proxies: Vec::new(),
        }
    }
}

impl ServerConfig {
    /// The connection timeouts.
    #[must_use]
    pub const fn timeouts(&self) -> Timeouts {
        Timeouts {
            login: ms(self.login_timeout_ms),
            idle: ms(self.idle_timeout_ms),
        }
    }

    /// Whether the http bind is on loopback.
    #[must_use]
    pub const fn http_is_loopback(&self) -> bool {
        self.bind.ip().is_loopback()
    }

    /// The MCP endpoint agents should use.
    #[must_use]
    pub fn mcp_url(&self) -> String {
        let base = self
            .public_url
            .clone()
            .unwrap_or_else(|| format!("http://{}", self.bind));
        format!("{}{}", base.trim_end_matches('/'), mud_mcp::MCP_PATH)
    }
}

/// How long a connection may wait. Each connection slot is scarce (see
/// [`crate::MAX_CONNECTIONS`]), so a silent client must not hold one
/// forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// For the whole login, from connect to the last password. `None`: no
    /// limit.
    pub login: Option<std::time::Duration>,
    /// Between two lines, after login. `None`: no limit.
    pub idle: Option<std::time::Duration>,
}

impl Default for Timeouts {
    fn default() -> Self {
        ServerConfig::default().timeouts()
    }
}

impl Timeouts {
    /// When a login that starts now must be done.
    #[must_use]
    pub fn login_deadline(&self) -> Option<tokio::time::Instant> {
        self.login.map(|d| tokio::time::Instant::now() + d)
    }

    /// Runs `fut`, or gives `None` once `deadline` passes first.
    pub async fn until<T>(
        deadline: Option<tokio::time::Instant>,
        fut: impl std::future::Future<Output = T>,
    ) -> Option<T> {
        match deadline {
            Some(at) => tokio::time::timeout_at(at, fut).await.ok(),
            None => Some(fut.await),
        }
    }

    /// Runs `fut`, or gives `None` once `limit` passes first.
    pub async fn within<T>(
        limit: Option<std::time::Duration>,
        fut: impl std::future::Future<Output = T>,
    ) -> Option<T> {
        match limit {
            Some(d) => tokio::time::timeout(d, fut).await.ok(),
            None => Some(fut.await),
        }
    }
}

const fn ms(n: u64) -> Option<std::time::Duration> {
    if n == 0 {
        None
    } else {
        Some(std::time::Duration::from_millis(n))
    }
}

/// `[world]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorldConfig {
    /// The pack folder. Unset: the bundled crypt-v01.
    pub pack: Option<PathBuf>,
    /// The SQLite world store. Unset: in memory.
    pub db: Option<PathBuf>,
    /// The seed of a new world's dice.
    pub seed: u64,
    /// Events older than this many days are archived, then trimmed from
    /// the store; 0 keeps every event in the store (0 to 36500).
    pub events_keep_days: u64,
    /// The newest events never trimmed, whatever their age; below 10000
    /// (the in-memory log) it acts as 10000 (at most 1000000000).
    pub events_keep_min: u64,
    /// How often retention runs, in ms (1000 to 604800000).
    pub retention_every_ms: u64,
    /// Events per retention transaction (1 to 100000).
    pub retention_batch: u32,
    /// Where `events-YYYY-MM-<epoch>.jsonl` go. Unset: `archive/` beside
    /// `db`.
    pub archive_dir: Option<PathBuf>,
}

impl Default for WorldConfig {
    fn default() -> Self {
        let r = mud_world::Retention::default();
        Self {
            pack: None,
            db: Some(PathBuf::from("data/world.db")),
            seed: 1,
            events_keep_days: r.keep_days,
            events_keep_min: r.keep_min,
            retention_every_ms: r.every_ms,
            retention_batch: r.batch,
            archive_dir: None,
        }
    }
}

impl WorldConfig {
    /// The checked events retention, `None` for an in-memory world. The
    /// archive goes in `archive_dir`, else in `archive/` beside `db`.
    ///
    /// # Errors
    /// A value is out of bounds; the message names it.
    pub fn retention(&self) -> anyhow::Result<Option<mud_world::Retention>> {
        use anyhow::ensure;
        ensure!(
            self.events_keep_days <= mud_world::durability::MAX_KEEP_DAYS,
            "[world] events_keep_days must be 0 to 36500, not {}",
            self.events_keep_days
        );
        ensure!(
            self.events_keep_min <= 1_000_000_000,
            "[world] events_keep_min must be at most 1000000000, not {}",
            self.events_keep_min
        );
        ensure!(
            (1_000..=604_800_000).contains(&self.retention_every_ms),
            "[world] retention_every_ms must be 1000 to 604800000, not {}",
            self.retention_every_ms
        );
        ensure!(
            (1..=100_000).contains(&self.retention_batch),
            "[world] retention_batch must be 1 to 100000, not {}",
            self.retention_batch
        );
        let Some(db) = &self.db else {
            return Ok(None);
        };
        let archive_dir = self
            .archive_dir
            .clone()
            .unwrap_or_else(|| db.parent().unwrap_or_else(|| Path::new("")).join("archive"));
        Ok(Some(
            mud_world::Retention {
                keep_days: self.events_keep_days,
                keep_min: self.events_keep_min,
                every_ms: self.retention_every_ms,
                batch: self.retention_batch,
                archive_dir,
            }
            .clamped(),
        ))
    }
}

/// A mind runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Runtime {
    /// The cast scripts. No model.
    #[default]
    Scripted,
    /// A durable autumn-harvest agent in process.
    Harvest,
    /// One `lg serve` per entity.
    Littleguy,
    /// An autumn-plugin-agent loop (ambient NPCs).
    PluginAgent,
}

/// `[minds]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MindsConfig {
    /// Persistent inhabitants.
    pub persistent: Runtime,
    /// Hidden agent PCs.
    pub turing: Runtime,
    /// Ambient NPCs.
    pub ambient: Runtime,
    /// How a harvest agent's own history stays bounded.
    pub harvest: HarvestSection,
}

impl Default for MindsConfig {
    fn default() -> Self {
        Self {
            persistent: Runtime::Scripted,
            turing: Runtime::Scripted,
            ambient: Runtime::Scripted,
            harvest: HarvestSection::default(),
        }
    }
}

/// `[minds.harvest]`: continue-as-new and retention of each harvest
/// agent's `harvest.db` (ADR 0011).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HarvestSection {
    /// Wakes per run before the agent continues as new (1 to 10000).
    pub ops_per_run: u64,
    /// Sealed runs older than this many days are purged, by harvest's own
    /// clock; 0 keeps them all (0 to 36500).
    pub keep_days: u64,
    /// The least game time between two purges, in ms (1000 to 604800000).
    pub purge_every_ms: u64,
}

impl Default for HarvestSection {
    fn default() -> Self {
        let d = mud_supervisor::harvest::HarvestHistory::default();
        Self {
            ops_per_run: d.ops_per_run,
            keep_days: d.keep_days,
            purge_every_ms: d.purge_every_ms,
        }
    }
}

impl HarvestSection {
    /// The checked history bounds.
    ///
    /// # Errors
    /// A value is out of bounds; the message names it.
    pub fn history(&self) -> anyhow::Result<mud_supervisor::harvest::HarvestHistory> {
        use anyhow::ensure;
        ensure!(
            (1..=10_000).contains(&self.ops_per_run),
            "[minds.harvest] ops_per_run must be 1 to 10000, not {}",
            self.ops_per_run
        );
        ensure!(
            self.keep_days <= mud_world::durability::MAX_KEEP_DAYS,
            "[minds.harvest] keep_days must be 0 to 36500, not {}",
            self.keep_days
        );
        ensure!(
            (1_000..=604_800_000).contains(&self.purge_every_ms),
            "[minds.harvest] purge_every_ms must be 1000 to 604800000, not {}",
            self.purge_every_ms
        );
        Ok(mud_supervisor::harvest::HarvestHistory {
            ops_per_run: self.ops_per_run,
            keep_days: self.keep_days,
            purge_every_ms: self.purge_every_ms,
        })
    }
}

/// One model alias for in-process minds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    /// An OpenAI-compatible base URL, such as `https://openrouter.ai/api/v1`.
    pub base_url: String,
    /// The model id at that server.
    pub model: String,
    /// The variable that holds the key.
    pub key_env: String,
}

/// `[littleguy]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LittleguySection {
    /// The `lg` binary.
    pub bin: PathBuf,
    /// Where each entity's daemon keeps its files.
    pub data_dir: PathBuf,
    /// `--model` of `lg serve`.
    pub provider: String,
    /// `--model-name`, by cast model alias. `default` covers the rest.
    pub model_names: BTreeMap<String, String>,
    /// `--base-url`.
    pub base_url: Option<String>,
    /// Tool rounds one wake may use; `0` is no bound. A wake that needs
    /// more ends with no answer (and is still charged).
    pub max_steps: u32,
}

impl Default for LittleguySection {
    fn default() -> Self {
        Self {
            bin: PathBuf::from("lg"),
            data_dir: PathBuf::from("data/agents"),
            provider: "offline".into(),
            model_names: BTreeMap::new(),
            base_url: None,
            max_steps: mud_supervisor::littleguy::LittleguyConfig::default().max_steps,
        }
    }
}

/// `[supervisor]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SupervisorSection {
    /// Tokens per agent per UTC day. 0 means no limit.
    pub daily_token_budget: u64,
    /// Per-entity budgets, by entity id. 0 means no limit.
    pub budgets: BTreeMap<String, u64>,
    /// Scales the Turing typing holds (1.0 is a real typist).
    pub typing_scale: f64,
    /// Scales macro-tick intervals.
    pub macro_tick_scale: f64,
    /// The first macro-tick after start, in seconds.
    pub first_macro_tick_secs: [u64; 2],
    /// How often the supervisor steps, in ms.
    pub step_ms: u64,
    /// The bound on one wake, in ms (1 s to 1 h). The time a hidden PC
    /// spends typing (a line held at the gate) does not count.
    pub wake_timeout_ms: u64,
    /// The bound on one wake of a tier, in ms, where it differs:
    /// `ambient`, `persistent` or `turing` (1 s to 1 h each).
    pub wake_timeout_ms_by_tier: mud_supervisor::runtime::TierTimeouts,
    /// A reactive plan older than this at boot is abandoned, and an
    /// addressed event (a tell, an attack, a mention by name) older than
    /// this is not replayed (0 to 1 day). An errand (a plan of a macro
    /// tick, a booking or a login) resumes however long the server was
    /// down.
    pub replay_max_age_ms: u64,
    /// A booking overdue by more than this at boot is dropped (0 to 30
    /// days).
    pub booking_max_late_ms: u64,
    /// A macro tick missed while the server was down fires once, within
    /// this many ms of boot (0 to 1 hour).
    pub catchup_spread_ms: u64,
}

impl Default for SupervisorSection {
    fn default() -> Self {
        let d = mud_supervisor::runtime::SupervisorConfig::default();
        Self {
            daily_token_budget: 400_000,
            budgets: BTreeMap::new(),
            typing_scale: 1.0,
            macro_tick_scale: 1.0,
            first_macro_tick_secs: [30, 120],
            step_ms: 250,
            wake_timeout_ms: d.wake_timeout_ms,
            wake_timeout_ms_by_tier: d.wake_timeout_ms_by_tier,
            replay_max_age_ms: d.replay_max_age_ms,
            booking_max_late_ms: d.booking_max_late_ms,
            catchup_spread_ms: d.catchup_spread_ms,
        }
    }
}

impl SupervisorSection {
    /// Checks the restart settings and the wake bounds.
    ///
    /// # Errors
    /// A value is out of bounds; the message names it.
    pub fn check_restart(&self) -> anyhow::Result<()> {
        use anyhow::ensure;
        ensure!(
            self.replay_max_age_ms <= 86_400_000,
            "[supervisor] replay_max_age_ms must be 0 to 86400000, not {}",
            self.replay_max_age_ms
        );
        ensure!(
            self.booking_max_late_ms <= 30 * 86_400_000,
            "[supervisor] booking_max_late_ms must be 0 to 2592000000, not {}",
            self.booking_max_late_ms
        );
        ensure!(
            self.catchup_spread_ms <= 3_600_000,
            "[supervisor] catchup_spread_ms must be 0 to 3600000, not {}",
            self.catchup_spread_ms
        );
        let t = &self.wake_timeout_ms_by_tier;
        for (key, ms) in [
            ("wake_timeout_ms", Some(self.wake_timeout_ms)),
            ("wake_timeout_ms_by_tier.ambient", t.ambient),
            ("wake_timeout_ms_by_tier.persistent", t.persistent),
            ("wake_timeout_ms_by_tier.turing", t.turing),
        ] {
            if let Some(ms) = ms {
                ensure!(
                    (1_000..=3_600_000).contains(&ms),
                    "[supervisor] {key} must be 1000 to 3600000, not {ms}"
                );
            }
        }
        Ok(())
    }
}

impl MudConfig {
    /// Refuses a proxy setup that would merge every player into one
    /// address: an https `public_url` (so a TLS proxy is in front) with no
    /// `trusted_proxies`.
    ///
    /// # Errors
    /// That setup.
    pub fn check_proxy(&self) -> anyhow::Result<()> {
        let https = self
            .server
            .public_url
            .as_deref()
            .is_some_and(|u| u.starts_with("https://"));
        anyhow::ensure!(
            !https || !self.server.trusted_proxies.is_empty(),
            "[server] public_url is https, so a proxy is in front, but [server] trusted_proxies is empty: every player would share the proxy's address, and one guesser could hold every login. Set trusted_proxies to the proxy's address."
        );
        Ok(())
    }

    /// Warnings for passwords that would cross the network in clear text.
    #[must_use]
    pub fn plaintext_warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.server.telnet.is_some_and(|t| !t.ip().is_loopback()) {
            out.push(
                "telnet carries passwords in clear text; bind it to loopback or tunnel it"
                    .to_owned(),
            );
        }
        let https = self
            .server
            .public_url
            .as_deref()
            .is_some_and(|u| u.starts_with("https://"));
        if !self.server.http_is_loopback() && !https {
            out.push("the terminal is served over plain http on a public address; put a TLS proxy in front and set public_url to https".to_owned());
        }
        out
    }

    /// Reads a config file.
    ///
    /// # Errors
    /// The file cannot be read or does not parse.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        toml::from_str(&text).map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_example_parses_and_defaults_are_offline() {
        let ex: MudConfig = toml::from_str(include_str!("../../../mud.toml.example")).unwrap();
        assert_eq!(ex.minds.persistent, Runtime::Littleguy);
        assert!(ex.models.contains_key("flash"));
        let d = MudConfig::default();
        assert_eq!(d.minds.persistent, Runtime::Scripted);
        assert_eq!(d.server.mcp_url(), "http://127.0.0.1:3000/mcp");
        assert!(toml::from_str::<MudConfig>("[server]\nbogus = 1").is_err());
    }

    #[test]
    fn the_example_sets_every_account_default() {
        let ex: MudConfig = toml::from_str(include_str!("../../../mud.toml.example")).unwrap();
        assert_eq!(ex.accounts, AccountsSection::default());
        assert_eq!(ex.server.login_timeout_ms, 300_000);
        let (path, c) = ex
            .accounts
            .to_config(Some(Path::new("data/world.db")))
            .unwrap();
        assert_eq!(path.as_deref(), Some(Path::new("data/accounts.db")));
        assert_eq!(c, AccountsConfig::default());
        assert_eq!(c.legacy, Legacy::Claim);
        let (none, _) = AccountsSection::default().to_config(None).unwrap();
        assert_eq!(none, None, "in memory when the world is");
        let own: MudConfig =
            toml::from_str("[accounts]\ndb = \"x/a.db\"\nlegacy = \"sealed\"").unwrap();
        let (p, c) = own.accounts.to_config(None).unwrap();
        assert_eq!(p.as_deref(), Some(Path::new("x/a.db")));
        assert_eq!(c.legacy, Legacy::Sealed);
        assert!(toml::from_str::<MudConfig>("[accounts]\nlegacy = \"open\"").is_err());
        assert!(toml::from_str::<MudConfig>("[accounts.throttle.name]\nfree = 1").is_err());
    }

    type Edit<'a> = &'a dyn Fn(&mut AccountsSection);

    #[test]
    fn accounts_bounds_are_checked() {
        let bad = |edit: &dyn Fn(&mut AccountsSection)| {
            let mut a = AccountsSection::default();
            edit(&mut a);
            a.to_config(None).unwrap_err().to_string()
        };
        let cases: [(Edit<'_>, &str); 13] = [
            (
                &|a| a.bcrypt_cost = 3,
                "[accounts] bcrypt_cost must be 4 to 16, not 3",
            ),
            (
                &|a| a.min_password_chars = 7,
                "[accounts] min_password_chars must be 8 to 64, not 7",
            ),
            (
                &|a| a.remember_days = 366,
                "[accounts] remember_days must be at most 365",
            ),
            (
                &|a| a.reconnect_minutes = 1_441,
                "[accounts] reconnect_minutes must be at most 1440",
            ),
            (
                &|a| {
                    a.reconnect_minutes = 1;
                    a.revalidate_secs = 31;
                },
                "[accounts] reconnect_minutes must be 0 or at least twice revalidate_secs",
            ),
            (
                &|a| a.max_remembered = 0,
                "[accounts] max_remembered must be 1 to 50",
            ),
            (
                &|a| a.known_addresses = 21,
                "[accounts] known_addresses must be at most 20",
            ),
            (
                &|a| a.max_password_attempts = 11,
                "[accounts] max_password_attempts must be 1 to 10",
            ),
            (
                &|a| a.hash_concurrency = 0,
                "[accounts] hash_concurrency must be 1 to 64",
            ),
            (
                &|a| a.revalidate_secs = 4,
                "[accounts] revalidate_secs must be 5 to 3600",
            ),
            (
                &|a| a.throttle.name.base_ms = 0,
                "[accounts.throttle.name] base_ms must be at least 1 and at most max_ms",
            ),
            (
                &|a| {
                    a.throttle.work.max_ms = 86_400_001;
                    a.throttle.work.forget_ms = 90_000_000;
                },
                "[accounts.throttle.work] max_ms must be at most 86400000",
            ),
            (
                &|a| a.throttle.address.forget_ms = 900_000,
                "[accounts.throttle.address] forget_ms must exceed max_ms",
            ),
        ];
        for (edit, want) in cases {
            assert_eq!(bad(edit), want);
        }
        let ok = AccountsSection {
            reconnect_minutes: 0,
            revalidate_secs: 3_600,
            ..AccountsSection::default()
        };
        assert!(ok.to_config(None).is_ok());
    }

    #[test]
    fn a_proxy_without_trusted_proxies_is_refused() {
        let mut c = MudConfig::default();
        assert!(c.check_proxy().is_ok());
        c.server.public_url = Some("http://mud.example".into());
        assert!(c.check_proxy().is_ok(), "http needs no proxy");
        c.server.public_url = Some("https://mud.example".into());
        assert_eq!(
            c.check_proxy().unwrap_err().to_string(),
            "[server] public_url is https, so a proxy is in front, but [server] trusted_proxies is empty: every player would share the proxy's address, and one guesser could hold every login. Set trusted_proxies to the proxy's address."
        );
        c.server.trusted_proxies = vec!["10.0.0.0/8".into()];
        assert!(c.check_proxy().is_ok());
    }

    #[test]
    fn plaintext_warnings_fire_only_off_loopback() {
        let mut c = MudConfig::default();
        c.server.telnet = Some("127.0.0.1:4000".parse().unwrap());
        assert_eq!(c.plaintext_warnings(), Vec::<String>::new());
        c.server.telnet = Some("0.0.0.0:4000".parse().unwrap());
        c.server.bind = "0.0.0.0:3000".parse().unwrap();
        let w = c.plaintext_warnings();
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w[0].starts_with("telnet carries passwords in clear text"));
        assert!(w[1].starts_with("the terminal is served over plain http"));
        c.server.public_url = Some("https://mud.example".into());
        assert_eq!(c.plaintext_warnings().len(), 1);
    }

    #[tokio::test]
    async fn a_login_deadline_is_total() {
        let t = Timeouts {
            login: Some(std::time::Duration::from_millis(30)),
            idle: None,
        };
        let at = t.login_deadline();
        assert_eq!(Timeouts::until(at, async { 1 }).await, Some(1));
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        assert_eq!(
            Timeouts::until(at, std::future::pending::<u8>()).await,
            None
        );
        assert_eq!(Timeouts::until(None, async { 3 }).await, Some(3));
        assert!(
            Timeouts {
                login: None,
                idle: None
            }
            .login_deadline()
            .is_none()
        );
    }

    #[test]
    fn world_retention_defaults_and_bounds() {
        let ex: MudConfig = toml::from_str(include_str!("../../../mud.toml.example")).unwrap();
        assert_eq!(ex.world, WorldConfig::default());
        let r = ex.world.retention().unwrap().unwrap();
        assert_eq!(r.keep_days, 7);
        assert_eq!(r.keep_min, 10_000);
        assert_eq!(r.every_ms, 3_600_000);
        assert_eq!(r.batch, 5_000);
        assert_eq!(r.archive_dir, Path::new("data/archive"));
        let own: MudConfig = toml::from_str(
            "[world]\ndb = \"w.db\"\nevents_keep_days = 0\nevents_keep_min = 5\narchive_dir = \"elsewhere/arch\"",
        )
        .unwrap();
        let r = own.world.retention().unwrap().unwrap();
        assert_eq!(r.keep_days, 0);
        assert_eq!(r.keep_min, 10_000, "raised to the in-memory log");
        assert_eq!(r.archive_dir, Path::new("elsewhere/arch"));
        let bare: MudConfig = toml::from_str("[world]\ndb = \"w.db\"").unwrap();
        assert_eq!(
            bare.world.retention().unwrap().unwrap().archive_dir,
            Path::new("archive")
        );
        let mem = WorldConfig {
            db: None,
            ..WorldConfig::default()
        };
        assert!(mem.retention().unwrap().is_none());
        let bad = |edit: &dyn Fn(&mut WorldConfig)| {
            let mut w = WorldConfig::default();
            edit(&mut w);
            w.retention().unwrap_err().to_string()
        };
        assert!(bad(&|w| w.events_keep_days = 36_501).contains("events_keep_days"));
        assert!(bad(&|w| w.events_keep_min = 1_000_000_001).contains("events_keep_min"));
        assert!(bad(&|w| w.retention_every_ms = 999).contains("retention_every_ms"));
        assert!(bad(&|w| w.retention_every_ms = 604_800_001).contains("retention_every_ms"));
        assert!(bad(&|w| w.retention_batch = 0).contains("retention_batch"));
        assert!(bad(&|w| w.retention_batch = 100_001).contains("retention_batch"));
        assert!(toml::from_str::<MudConfig>("[world]\nevents_keep = 1").is_err());
    }

    #[test]
    fn supervisor_restart_defaults_and_bounds() {
        let ex: MudConfig = toml::from_str(include_str!("../../../mud.toml.example")).unwrap();
        let d = SupervisorSection::default();
        assert_eq!(ex.supervisor.replay_max_age_ms, d.replay_max_age_ms);
        assert_eq!(ex.supervisor.booking_max_late_ms, d.booking_max_late_ms);
        assert_eq!(ex.supervisor.catchup_spread_ms, d.catchup_spread_ms);
        assert_eq!(
            (
                d.replay_max_age_ms,
                d.booking_max_late_ms,
                d.catchup_spread_ms
            ),
            (120_000, 86_400_000, 120_000)
        );
        d.check_restart().unwrap();
        let bad = |edit: &dyn Fn(&mut SupervisorSection)| {
            let mut s = SupervisorSection::default();
            edit(&mut s);
            s.check_restart().unwrap_err().to_string()
        };
        assert!(bad(&|s| s.replay_max_age_ms = 86_400_001).contains("replay_max_age_ms"));
        assert!(
            bad(&|s| s.booking_max_late_ms = 30 * 86_400_000 + 1).contains("booking_max_late_ms")
        );
        assert!(bad(&|s| s.catchup_spread_ms = 3_600_001).contains("catchup_spread_ms"));
    }

    #[test]
    fn wake_bounds_by_tier_parse_and_are_checked() {
        let ex: MudConfig = toml::from_str(include_str!("../../../mud.toml.example")).unwrap();
        assert_eq!(ex.supervisor.wake_timeout_ms, 120_000);
        assert_eq!(ex.supervisor.wake_timeout_ms_by_tier.turing, Some(180_000));
        assert_eq!(ex.supervisor.wake_timeout_ms_by_tier.persistent, None);
        ex.supervisor.check_restart().unwrap();
        let own: MudConfig = toml::from_str(
            "[supervisor]\nwake_timeout_ms = 90000\n[supervisor.wake_timeout_ms_by_tier]\nambient = 30000",
        )
        .unwrap();
        let t = own.supervisor.wake_timeout_ms_by_tier;
        assert_eq!(t.for_tier(mud_world::pack::Tier::Ambient, 90_000), 30_000);
        assert_eq!(
            t.for_tier(mud_world::pack::Tier::Turing, 90_000),
            90_000,
            "a tier left out falls back"
        );
        assert!(
            toml::from_str::<MudConfig>("[supervisor.wake_timeout_ms_by_tier]\nmob = 1000")
                .is_err(),
            "only the agent tiers have a bound"
        );
        let bad = |edit: &dyn Fn(&mut SupervisorSection)| {
            let mut s = SupervisorSection::default();
            edit(&mut s);
            s.check_restart().unwrap_err().to_string()
        };
        assert!(bad(&|s| s.wake_timeout_ms = 999).contains("wake_timeout_ms"));
        assert!(
            bad(&|s| s.wake_timeout_ms_by_tier.turing = Some(3_600_001))
                .contains("wake_timeout_ms_by_tier.turing")
        );
    }

    #[test]
    fn harvest_history_defaults_and_bounds() {
        let ex: MudConfig = toml::from_str(include_str!("../../../mud.toml.example")).unwrap();
        assert_eq!(ex.minds.harvest, HarvestSection::default());
        let h = ex.minds.harvest.history().unwrap();
        assert_eq!(
            (h.ops_per_run, h.keep_days, h.purge_every_ms),
            (32, 3, 3_600_000)
        );
        let own: MudConfig = toml::from_str(
            "[minds.harvest]
ops_per_run = 5
keep_days = 0",
        )
        .unwrap();
        let h = own.minds.harvest.history().unwrap();
        assert_eq!((h.ops_per_run, h.keep_days), (5, 0));
        let bad = |edit: &dyn Fn(&mut HarvestSection)| {
            let mut s = HarvestSection::default();
            edit(&mut s);
            s.history().unwrap_err().to_string()
        };
        assert!(bad(&|s| s.ops_per_run = 0).contains("ops_per_run"));
        assert!(bad(&|s| s.ops_per_run = 10_001).contains("ops_per_run"));
        assert!(bad(&|s| s.keep_days = 36_501).contains("keep_days"));
        assert!(bad(&|s| s.purge_every_ms = 999).contains("purge_every_ms"));
        assert!(
            toml::from_str::<MudConfig>(
                "[minds.harvest]
keep = 1"
            )
            .is_err()
        );
    }

    #[test]
    fn scripts_section_defaults_and_rejects_unknown_keys() {
        let d = MudConfig::default();
        assert!(d.scripts.enabled && d.scripts.allow_runtime_override);
        let off: MudConfig =
            toml::from_str("[scripts]\nenabled = false\nallow_runtime_override = false").unwrap();
        assert!(!off.scripts.enabled && !off.scripts.allow_runtime_override);
        assert!(toml::from_str::<MudConfig>("[scripts]\nmax_operations = 5").is_err());
    }
}
