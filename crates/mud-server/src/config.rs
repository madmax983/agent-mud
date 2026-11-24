//! `mud.toml`: what to serve, which world, and which minds.
//!
//! Every section has a default, so `mud serve` runs with no file at all:
//! the bundled world, scripted minds, no model keys.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

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
    /// How long a terminal or telnet client may take to give a name, in
    /// ms. `0`: no limit.
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
            login_timeout_ms: 60_000,
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
    /// For the name, before login. `None`: no limit.
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
}

impl Default for WorldConfig {
    fn default() -> Self {
        Self {
            pack: None,
            db: Some(PathBuf::from("data/world.db")),
            seed: 1,
        }
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
}

impl Default for MindsConfig {
    fn default() -> Self {
        Self {
            persistent: Runtime::Scripted,
            turing: Runtime::Scripted,
            ambient: Runtime::Scripted,
        }
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
}

impl Default for SupervisorSection {
    fn default() -> Self {
        Self {
            daily_token_budget: 400_000,
            budgets: BTreeMap::new(),
            typing_scale: 1.0,
            macro_tick_scale: 1.0,
            first_macro_tick_secs: [30, 120],
            step_ms: 250,
        }
    }
}

impl MudConfig {
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
}
