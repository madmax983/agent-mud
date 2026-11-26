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
///
/// Every key has a code default, so a `[littleguy]` written before a key
/// existed keeps working: notes, research and compaction are on unless
/// turned off.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// Minutes between heartbeat ticks (lg's minimum is 1); 0 turns them
    /// off.
    pub heartbeat_minutes: u32,
    /// A tick researches the agent's open ideas (`heartbeat.research`).
    pub research: bool,
    /// The agent keeps notes (`tools.files`). This also turns on lg's
    /// workspace file tools in the daemon's own folder; the ones that
    /// change it are refused on ticks.
    pub notes: bool,
    /// `[compaction]`: a long op saves notes and starts over below the
    /// request cap, instead of failing.
    pub compaction: bool,
    /// Compact at this percent of `max_request_bytes` (1 to 100).
    pub compaction_at_percent: u8,
    /// Resets of one op at most (at least 1).
    pub compaction_max_resets: u32,
    /// The largest model request lg sends, in bytes; keep it under the
    /// provider's window. 0 is lg's default (2 MiB).
    pub max_request_bytes: u64,
    /// Turns of transcript the agent sees (at least 1).
    pub window: u32,
    /// Refine: `off`, `menu` (the default: the council picks operator menu
    /// items, using lg's proposer as evidence) or `free` (ADR 0016: the
    /// guardrails lines lg's proposer wrote, after the deterministic guard
    /// and a unanimous council of at least three; a Turing-tier agent still
    /// uses the menu). Needs an `lg` built with the `refine` feature
    /// unless `off`.
    pub refine: mud_supervisor::littleguy::RefineMode,
    /// `[littleguy.steward]`: the MUD as each daemon's owner (ADR 0015).
    pub steward: mud_supervisor::steward::StewardConfig,
}

impl Default for LittleguySection {
    fn default() -> Self {
        let d = mud_supervisor::littleguy::LittleguyConfig::default();
        let c = d.compaction.unwrap_or_default();
        Self {
            bin: PathBuf::from("lg"),
            data_dir: PathBuf::from("data/agents"),
            provider: "offline".into(),
            model_names: BTreeMap::new(),
            base_url: None,
            max_steps: d.max_steps,
            heartbeat_minutes: d.heartbeat_minutes,
            research: d.research,
            notes: d.notes,
            compaction: d.compaction.is_some(),
            compaction_at_percent: c.at_percent,
            compaction_max_resets: c.max_resets,
            max_request_bytes: d.max_request_bytes,
            window: d.window,
            refine: d.refine,
            steward: mud_supervisor::steward::StewardConfig::default(),
        }
    }
}

impl LittleguySection {
    /// Checks the bounds lg would refuse at start.
    ///
    /// # Errors
    /// A value is out of bounds; the message names it.
    pub fn check(&self) -> anyhow::Result<()> {
        use anyhow::ensure;
        ensure!(
            (1..=100).contains(&self.compaction_at_percent),
            "[littleguy] compaction_at_percent must be 1 to 100, not {}",
            self.compaction_at_percent
        );
        ensure!(
            self.compaction_max_resets >= 1,
            "[littleguy] compaction_max_resets must be at least 1"
        );
        ensure!(self.window >= 1, "[littleguy] window must be at least 1");
        Ok(())
    }

    /// Checks `[littleguy.steward]` against the model aliases in `models`.
    ///
    /// # Errors
    /// A council alias is not in `[models]`, or a value is out of bounds;
    /// the message names it.
    pub fn check_steward(&self, models: &BTreeMap<String, ModelConfig>) -> anyhow::Result<()> {
        use anyhow::ensure;
        let s = &self.steward;
        for alias in s.council_model.iter().chain(&s.council_models) {
            ensure!(
                models.contains_key(alias),
                "[littleguy.steward] names the council model {alias:?}, but there is no [models.{alias}]"
            );
        }
        ensure!(
            s.council_members % 2 == 1 && (1..=7).contains(&s.council_members),
            "[littleguy.steward] council_members must be odd, 1 to 7, not {}",
            s.council_members
        );
        ensure!(
            s.council_members == 1 || (s.council_temperature > 0.0 && s.council_temperature <= 2.0),
            "[littleguy.steward] council_temperature must be above 0 and at most 2 with more than one member, not {}",
            s.council_temperature
        );
        ensure!(
            (1..=100).contains(&s.max_council_votes_per_day),
            "[littleguy.steward] max_council_votes_per_day must be 1 to 100, not {}",
            s.max_council_votes_per_day
        );
        ensure!(
            s.every_secs >= 1 && s.owner_timeout_secs >= 1 && s.max_intent_tries >= 1,
            "[littleguy.steward] every_secs, owner_timeout_secs and max_intent_tries must be at least 1"
        );
        ensure!(
            s.title_chars >= 1 && s.why_chars >= 1,
            "[littleguy.steward] title_chars and why_chars must be at least 1"
        );
        for p in &s.deny_phrases {
            ensure!(
                !p.trim().is_empty() && p.is_ascii(),
                "[littleguy.steward] deny_phrases entries must be non-empty ASCII, not {p:?}"
            );
        }
        self.check_refine()
    }

    /// Checks the refine keys of `[littleguy.steward]`: the floors on the
    /// rate, and the operator menu.
    ///
    /// # Errors
    /// A value is out of bounds; the message names it.
    pub fn check_refine(&self) -> anyhow::Result<()> {
        use anyhow::ensure;
        use mud_supervisor::steward::vet::{menu_id_ok, menu_text_ok};
        let s = &self.steward;
        if self.refine.is_on() {
            ensure!(
                s.refine_every_hours >= 24,
                "[littleguy.steward] refine_every_hours must be at least 24, not {}",
                s.refine_every_hours
            );
            ensure!(
                s.refine_min_wakes >= 1,
                "[littleguy.steward] refine_min_wakes must be at least 1"
            );
        }
        ensure!(
            s.activate_wait_secs >= 1 && s.refine_timeout_secs >= 1,
            "[littleguy.steward] activate_wait_secs and refine_timeout_secs must be at least 1"
        );
        for (key, secs) in [
            ("activate_wait_secs", s.activate_wait_secs),
            ("refine_timeout_secs", s.refine_timeout_secs),
            ("refine_drain_secs", s.refine_drain_secs),
        ] {
            ensure!(
                secs <= 86_400,
                "[littleguy.steward] {key} must be at most 86400 (a day), not {secs}"
            );
        }
        ensure!(
            (1..=16).contains(&s.max_guardrails),
            "[littleguy.steward] max_guardrails must be 1 to 16, not {}",
            s.max_guardrails
        );
        ensure!(
            s.guardrail.len() <= 16,
            "[littleguy.steward] at most 16 guardrail menu items, not {}",
            s.guardrail.len()
        );
        let mut ids = std::collections::BTreeSet::new();
        for g in &s.guardrail {
            ensure!(
                menu_id_ok(&g.id),
                "[[littleguy.steward.guardrail]] id {:?} must be 1 to 32 of a-z, 0-9 and _",
                g.id
            );
            ensure!(
                ids.insert(g.id.as_str()),
                "[[littleguy.steward.guardrail]] id {:?} is used twice",
                g.id
            );
            ensure!(
                menu_text_ok(&g.text),
                "[[littleguy.steward.guardrail]] {:?}: the text must be 1 to 200 characters of ASCII letters, digits, space and . , ; : ' ! ? ( ) -",
                g.id
            );
        }
        ensure!(
            (100..=2000).contains(&s.refine_max_chars),
            "[littleguy.steward] refine_max_chars must be 100 to 2000, not {}",
            s.refine_max_chars
        );
        ensure!(
            (1..=12).contains(&s.refine_max_lines),
            "[littleguy.steward] refine_max_lines must be 1 to 12, not {}",
            s.refine_max_lines
        );
        ensure!(
            (20..=300).contains(&s.refine_line_chars),
            "[littleguy.steward] refine_line_chars must be 20 to 300, not {}",
            s.refine_line_chars
        );
        for (key, list) in [
            ("refine_deny_words", &s.refine_deny_words),
            ("refine_deny_stems", &s.refine_deny_stems),
        ] {
            for w in list {
                // A deny entry matches a token, which holds only ASCII
                // letters and digits: anything else would match nothing.
                ensure!(
                    !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric()),
                    "[littleguy.steward] {key} entries must be non-empty ASCII letters and digits, not {w:?}"
                );
            }
        }
        if self.refine == mud_supervisor::littleguy::RefineMode::Free {
            ensure!(
                s.council_members >= mud_supervisor::steward::vet::FREE_MIN_MEMBERS,
                "[littleguy] refine = \"free\" needs [littleguy.steward] council_members of at least {} (ADR 0016), not {}",
                mud_supervisor::steward::vet::FREE_MIN_MEMBERS,
                s.council_members
            );
        }
        for e in &s.resume_refine {
            ensure!(
                !e.trim().is_empty(),
                "[littleguy.steward] resume_refine names an empty entity id"
            );
        }
        Ok(())
    }

    /// The `[models]` alias of the council: `council_model`, or `council`
    /// when that alias exists. `None`: no judge.
    #[must_use]
    pub fn council_alias(&self, models: &BTreeMap<String, ModelConfig>) -> Option<String> {
        self.steward
            .council_model
            .clone()
            .or_else(|| models.contains_key("council").then(|| "council".to_owned()))
    }

    /// The `[compaction]` to render, if on.
    #[must_use]
    pub const fn compaction(&self) -> Option<mud_supervisor::littleguy::CompactionRender> {
        if self.compaction {
            Some(mud_supervisor::littleguy::CompactionRender {
                at_percent: self.compaction_at_percent,
                max_resets: self.compaction_max_resets,
            })
        } else {
            None
        }
    }

    /// What to warn about at start when littleguy runs. `models` are the
    /// `[models]` aliases the council may name.
    #[must_use]
    pub fn warnings(&self, models: &BTreeMap<String, ModelConfig>) -> Vec<String> {
        let mut out = Vec::new();
        if self.compaction && self.max_request_bytes == 0 {
            out.push(
                "[littleguy] compaction is on but max_request_bytes is 0: lg compacts below its 2 MiB default, which may be past the provider's window; set max_request_bytes under it"
                    .to_owned(),
            );
        }
        if self.refine == mud_supervisor::littleguy::RefineMode::Free
            && self.council_is_one_model(models)
        {
            out.push(
                "[littleguy] refine = \"free\": council members are samples of one model, so their votes on a habit text are correlated (ADR 0016); name more aliases in [littleguy.steward] council_models to make them differ"
                    .to_owned(),
            );
        }
        out
    }

    /// Whether every council member asks one model: the aliases members
    /// cycle (`council_model`, then `council_models`) name one model at
    /// one server. With no alias there is no council at all.
    #[must_use]
    pub fn council_is_one_model(&self, models: &BTreeMap<String, ModelConfig>) -> bool {
        let Some(first) = self.council_alias(models) else {
            return false;
        };
        let distinct: std::collections::BTreeSet<(&str, &str)> = std::iter::once(&first)
            .chain(&self.steward.council_models)
            .take(usize::try_from(self.steward.council_members).unwrap_or(usize::MAX))
            .filter_map(|a| models.get(a))
            .map(|m| (m.base_url.as_str(), m.model.as_str()))
            .collect();
        distinct.len() <= 1
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
    fn the_example_sets_the_littleguy_autonomy_keys() {
        let ex: MudConfig = toml::from_str(include_str!("../../../mud.toml.example")).unwrap();
        let lg = &ex.littleguy;
        assert_eq!(lg.heartbeat_minutes, 240);
        assert!(lg.research && lg.notes && lg.compaction);
        assert_eq!(
            (lg.compaction_at_percent, lg.compaction_max_resets),
            (80, 3)
        );
        assert_eq!(lg.max_request_bytes, 240_000);
        assert_eq!(lg.window, 30);
        assert_eq!(
            lg.refine,
            mud_supervisor::littleguy::RefineMode::Free,
            "the example opts in to ADR 0016"
        );
        assert!(lg.check().is_ok());
        let w = lg.warnings(&ex.models);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("samples of one model"), "{w:?}");
        assert!(
            toml::from_str::<MudConfig>("[littleguy]\nnotez = true").is_err(),
            "an unknown key is refused"
        );
    }

    #[tokio::test]
    async fn an_old_mud_toml_keeps_its_meaning() {
        // A [littleguy] section written before the autonomy keys.
        let dir = tempfile::tempdir().unwrap();
        let old: MudConfig = toml::from_str(&format!(
            "[minds]\npersistent = \"littleguy\"\n[littleguy]\nbin = {:?}\ndata_dir = {:?}\nprovider = \"openrouter\"\nmax_steps = 64\n",
            dir.path().join("no-such-lg"),
            dir.path().join("agents"),
        ))
        .unwrap();
        let lg = &old.littleguy;
        assert_eq!(lg.provider, "openrouter");
        assert!(lg.research && lg.notes && lg.compaction, "autonomy is on");
        assert_eq!(lg.heartbeat_minutes, 240);
        // The real path: minds::build passes [littleguy] to each daemon,
        // which writes its lg.toml before it tries to start.
        let pack = mud_world::WorldPack::crypt_v01().unwrap();
        let tokens = mud_mcp::TokenRegistry::new();
        let m = crate::minds::build(&old, &pack, &tokens, &|_| mud_world::Seq(0), None).unwrap();
        let marta = mud_world::EntityId::from("marta");
        assert!(m.littleguy[0].ensure(&marta).await.is_err(), "no binary");
        let text =
            std::fs::read_to_string(m.littleguy[0].entity_dir(&marta).join("lg.toml")).unwrap();
        let v: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(v["tools"]["files"].as_bool(), Some(true), "notes");
        assert_eq!(v["memory"]["enabled"].as_bool(), Some(true));
        assert!(v["system"].as_str().unwrap().contains("write_note"));
        assert_eq!(v["heartbeat"]["research"].as_bool(), Some(true));
        assert_eq!(v["heartbeat"]["every_minutes"].as_integer(), Some(240));
        assert_eq!(v["compaction"]["at_percent"].as_integer(), Some(80));
        assert_eq!(v["compaction"]["max_resets"].as_integer(), Some(3));
        assert_eq!(v["max_request_bytes"].as_integer(), Some(240_000));
        assert_eq!(v["window"].as_integer(), Some(30));
        assert_eq!(v["max_steps"].as_integer(), Some(64));
        assert_eq!(
            v["refine"]["messages"].as_integer(),
            Some(30),
            "refine = menu"
        );
    }

    #[test]
    fn the_example_sets_the_steward_and_its_council() {
        let ex: MudConfig = toml::from_str(include_str!("../../../mud.toml.example")).unwrap();
        let s = &ex.littleguy.steward;
        assert!(s.enabled);
        assert_eq!(s.council_model.as_deref(), Some("council"));
        assert!(ex.models.contains_key("council"));
        assert_eq!(
            ex.littleguy.council_alias(&ex.models).as_deref(),
            Some("council")
        );
        assert!(ex.littleguy.check_steward(&ex.models).is_ok());
        assert!(
            include_str!("../../../mud.toml.example")
                .contains("https://openrouter.ai/rankings/decisions#top-models"),
            "the example links the decision ranking"
        );
        assert_eq!(
            s,
            &mud_supervisor::steward::StewardConfig {
                council_model: Some("council".into()),
                ..mud_supervisor::steward::StewardConfig::default()
            },
            "the example sets the code defaults"
        );
    }

    #[test]
    fn refine_floors_are_enforced() {
        use mud_supervisor::littleguy::RefineMode;
        let models = BTreeMap::new();
        let check = |edit: &dyn Fn(&mut LittleguySection)| {
            let mut s = LittleguySection::default();
            edit(&mut s);
            s.check_steward(&models).map_err(|e| e.to_string())
        };
        assert_eq!(
            LittleguySection::default().refine,
            RefineMode::Menu,
            "the code default"
        );
        assert!(check(&|_| {}).is_ok());
        let every = check(&|s| s.steward.refine_every_hours = 0).unwrap_err();
        assert!(every.contains("refine_every_hours"), "{every}");
        assert!(check(&|s| s.steward.refine_every_hours = 23).is_err());
        assert!(check(&|s| s.steward.refine_every_hours = 24).is_ok());
        let wakes = check(&|s| s.steward.refine_min_wakes = 0).unwrap_err();
        assert!(wakes.contains("refine_min_wakes"), "{wakes}");
        assert!(
            check(&|s| {
                s.refine = RefineMode::Off;
                s.steward.refine_every_hours = 0;
                s.steward.refine_min_wakes = 0;
            })
            .is_ok(),
            "no floor while refine is off"
        );
        // The start-up path refuses it too.
        let cfg: MudConfig =
            toml::from_str("[littleguy.steward]\nrefine_every_hours = 0\n").unwrap();
        assert!(cfg.littleguy.check_steward(&cfg.models).is_err());
        assert!(
            toml::from_str::<MudConfig>("[littleguy]\nrefine = \"auto\"").is_err(),
            "off, menu or free"
        );
        assert!(
            toml::from_str::<MudConfig>("[littleguy.steward]\nrefine = \"menu\"").is_err(),
            "refine is a [littleguy] key"
        );
    }

    #[test]
    fn the_guardrail_menu_is_checked() {
        use mud_supervisor::steward::vet::Snippet;
        let cfg: MudConfig = toml::from_str(
            "[[littleguy.steward.guardrail]]\nid = \"brief\"\ntext = \"Keep it short.\"\n",
        )
        .unwrap();
        assert_eq!(cfg.littleguy.steward.guardrail.len(), 1);
        assert_eq!(cfg.littleguy.steward.menu()[0].id, "brief");
        assert!(cfg.littleguy.check_refine().is_ok());
        let bad = |g: Vec<(&str, &str)>| {
            let mut s = LittleguySection::default();
            s.steward.guardrail = g
                .into_iter()
                .map(|(id, text)| Snippet {
                    id: id.into(),
                    text: text.into(),
                })
                .collect();
            s.check_refine().unwrap_err().to_string()
        };
        assert!(bad(vec![("Brief", "x")]).contains("id"));
        assert!(bad(vec![("a", "x"), ("a", "y")]).contains("twice"));
        assert!(bad(vec![("a", "use {json}")]).contains("text"));
        assert!(bad(vec![("a", "")]).contains("text"));
        let mut s = LittleguySection::default();
        s.steward.guardrail = (0..17)
            .map(|i| Snippet {
                id: format!("g{i}"),
                text: "x".into(),
            })
            .collect();
        assert!(s.check_refine().unwrap_err().to_string().contains("16"));
        assert!(
            toml::from_str::<MudConfig>(
                "[[littleguy.steward.guardrail]]\nid = \"a\"\ntext = \"b\"\nextra = 1\n"
            )
            .is_err(),
            "an unknown menu key is refused"
        );
    }

    fn model(name: &str) -> ModelConfig {
        ModelConfig {
            base_url: "http://127.0.0.1:1/v1".into(),
            model: name.into(),
            key_env: "K".into(),
        }
    }

    #[test]
    fn a_one_alias_council_warns_at_start() {
        let models = BTreeMap::from([
            ("council".to_owned(), model("m")),
            ("same".to_owned(), model("m")),
            ("other".to_owned(), model("n")),
        ]);
        let free = |extra: &[&str]| LittleguySection {
            refine: mud_supervisor::littleguy::RefineMode::Free,
            steward: mud_supervisor::steward::StewardConfig {
                council_models: extra.iter().map(|a| (*a).to_owned()).collect(),
                ..mud_supervisor::steward::StewardConfig::default()
            },
            ..LittleguySection::default()
        };
        let w = free(&[]).warnings(&models);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("samples of one model"), "{w:?}");
        assert!(w[0].contains("ADR 0016"));
        assert_eq!(
            free(&["same"]).warnings(&models).len(),
            1,
            "two aliases of one model are one model"
        );
        let none = Vec::<String>::new();
        assert_eq!(free(&["other"]).warnings(&models), none);
        assert_eq!(
            free(&[]).warnings(&BTreeMap::new()),
            none,
            "no council: nothing votes"
        );
        let menu = LittleguySection::default();
        assert_eq!(menu.warnings(&models), none, "menu text is the operator's");
    }

    #[test]
    fn free_refine_needs_three_members_and_bounded_keys() {
        let models = BTreeMap::from([("council".to_owned(), model("m"))]);
        let bad = |edit: &dyn Fn(&mut LittleguySection)| {
            let mut s = LittleguySection {
                refine: mud_supervisor::littleguy::RefineMode::Free,
                ..LittleguySection::default()
            };
            edit(&mut s);
            s.check_steward(&models).unwrap_err().to_string()
        };
        assert!(
            bad(&|s| {
                s.steward.council_members = 1;
                s.steward.council_temperature = 0.0;
            })
            .contains("council_members of at least 3"),
            "free needs three members"
        );
        assert!(bad(&|s| s.steward.refine_max_chars = 99).contains("refine_max_chars"));
        assert!(bad(&|s| s.steward.refine_max_chars = 2001).contains("refine_max_chars"));
        assert!(bad(&|s| s.steward.refine_max_lines = 0).contains("refine_max_lines"));
        assert!(bad(&|s| s.steward.refine_max_lines = 13).contains("refine_max_lines"));
        assert!(bad(&|s| s.steward.refine_line_chars = 19).contains("refine_line_chars"));
        assert!(bad(&|s| s.steward.refine_line_chars = 301).contains("refine_line_chars"));
        assert!(
            bad(&|s| s.steward.refine_deny_words = vec![String::new()])
                .contains("refine_deny_words")
        );
        assert!(
            bad(&|s| s.steward.refine_deny_stems = vec!["\u{e9}t\u{e9}".into()])
                .contains("refine_deny_stems")
        );
        let ok = LittleguySection {
            refine: mud_supervisor::littleguy::RefineMode::Free,
            steward: mud_supervisor::steward::StewardConfig {
                refine_deny_words: vec!["Kale".into()],
                refine_deny_stems: vec!["hagg".into()],
                ..mud_supervisor::steward::StewardConfig::default()
            },
            ..LittleguySection::default()
        };
        assert!(ok.check_steward(&models).is_ok());
        let menu_one = LittleguySection {
            steward: mud_supervisor::steward::StewardConfig {
                council_members: 1,
                council_temperature: 0.0,
                ..mud_supervisor::steward::StewardConfig::default()
            },
            ..LittleguySection::default()
        };
        assert!(
            menu_one.check_steward(&models).is_ok(),
            "the menu takes one member"
        );
    }

    #[test]
    fn steward_keys_are_checked() {
        assert!(
            toml::from_str::<MudConfig>("[littleguy.steward]\ncouncil_modle = \"x\"").is_err(),
            "an unknown steward key is refused"
        );
        let models = BTreeMap::from([(
            "council".to_owned(),
            ModelConfig {
                base_url: "http://127.0.0.1:1/v1".into(),
                model: "m".into(),
                key_env: "K".into(),
            },
        )]);
        let bad = |edit: &dyn Fn(&mut LittleguySection)| {
            let mut s = LittleguySection::default();
            edit(&mut s);
            s.check_steward(&models).unwrap_err().to_string()
        };
        assert!(bad(&|s| s.steward.council_model = Some("typo".into())).contains("[models.typo]"));
        assert!(
            bad(&|s| s.steward.council_models = vec!["other".into()]).contains("[models.other]")
        );
        assert!(bad(&|s| s.steward.council_members = 2).contains("council_members"));
        assert!(bad(&|s| s.steward.council_members = 9).contains("council_members"));
        assert!(bad(&|s| s.steward.council_temperature = 0.0).contains("council_temperature"));
        assert!(
            bad(&|s| s.steward.max_council_votes_per_day = 0).contains("max_council_votes_per_day")
        );
        assert!(bad(&|s| s.steward.every_secs = 0).contains("every_secs"));
        assert!(bad(&|s| s.steward.title_chars = 0).contains("title_chars"));
        assert!(bad(&|s| s.steward.deny_phrases = vec![" ".into()]).contains("deny_phrases"));
        assert!(bad(&|s| s.steward.max_guardrails = 0).contains("max_guardrails"));
        assert!(bad(&|s| s.steward.activate_wait_secs = 0).contains("activate_wait_secs"));
        assert!(
            bad(&|s| s.steward.refine_drain_secs = u64::MAX).contains("refine_drain_secs"),
            "an unbounded drain is refused"
        );
        assert!(bad(&|s| s.steward.activate_wait_secs = 86_401).contains("activate_wait_secs"));
        assert!(bad(&|s| s.steward.resume_refine = vec![String::new()]).contains("resume_refine"));
        assert!(bad(&|s| s.steward.deny_phrases = vec!["é".into()]).contains("deny_phrases"));
        let one = LittleguySection {
            steward: mud_supervisor::steward::StewardConfig {
                council_members: 1,
                council_temperature: 0.0,
                ..mud_supervisor::steward::StewardConfig::default()
            },
            ..LittleguySection::default()
        };
        assert!(
            one.check_steward(&models).is_ok(),
            "one member may be greedy"
        );
        let d = LittleguySection::default();
        assert!(d.check_steward(&BTreeMap::new()).is_ok());
        assert_eq!(
            d.council_alias(&BTreeMap::new()),
            None,
            "no alias: no judge"
        );
        assert_eq!(d.council_alias(&models).as_deref(), Some("council"));
    }

    #[test]
    fn littleguy_bounds_are_checked() {
        let bad = |edit: &dyn Fn(&mut LittleguySection)| {
            let mut s = LittleguySection::default();
            edit(&mut s);
            s.check().unwrap_err().to_string()
        };
        assert_eq!(
            bad(&|s| s.compaction_at_percent = 0),
            "[littleguy] compaction_at_percent must be 1 to 100, not 0"
        );
        assert_eq!(
            bad(&|s| s.compaction_at_percent = 101),
            "[littleguy] compaction_at_percent must be 1 to 100, not 101"
        );
        assert_eq!(
            bad(&|s| s.compaction_max_resets = 0),
            "[littleguy] compaction_max_resets must be at least 1"
        );
        assert_eq!(
            bad(&|s| s.window = 0),
            "[littleguy] window must be at least 1"
        );
        let off = LittleguySection {
            compaction: false,
            max_request_bytes: 0,
            ..LittleguySection::default()
        };
        assert_eq!(off.compaction(), None);
        assert_eq!(off.warnings(&BTreeMap::new()), Vec::<String>::new());
        let unbounded = LittleguySection {
            max_request_bytes: 0,
            ..LittleguySection::default()
        };
        let w = unbounded.warnings(&BTreeMap::new());
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("max_request_bytes is 0"), "{w:?}");
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
