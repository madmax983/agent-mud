//! Builds the mind of every agent entity from the config and the cast.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context as _;
use autumn_harvest_agent::AgentModel;
use autumn_harvest_agent::openai::OpenAiModel;
use autumn_plugin_agent::{LlmClient, OpenAiCompatibleClient};
use mud_mcp::TokenRegistry;
use mud_script::{MindRuntime, ScriptHost};
use mud_supervisor::ambient::AmbientMind;
use mud_supervisor::harvest::{HarvestMind, HarvestPersona};
use mud_supervisor::littleguy::{LittleguyConfig, LittleguyMind, Persona};
use mud_supervisor::mind::Mind;
use mud_supervisor::scripted::ScriptedMind;
use mud_world::pack::{CastDef, Tier};
use mud_world::{EntityId, Game, Seq, WorldPack};

use crate::config::{ModelConfig, MudConfig, Runtime};

/// The minds of a world, and the littleguy daemons to stop at exit.
#[derive(Debug)]
pub struct Minds {
    /// One mind per agent entity.
    pub by_entity: HashMap<EntityId, Arc<dyn Mind>>,
    /// The littleguy minds, one per model alias.
    pub littleguy: Vec<Arc<LittleguyMind>>,
}

const fn runtime_for(cfg: &MudConfig, tier: Tier) -> Option<Runtime> {
    match tier {
        Tier::Persistent => Some(cfg.minds.persistent),
        Tier::Turing => Some(cfg.minds.turing),
        Tier::Ambient => Some(cfg.minds.ambient),
        Tier::None | Tier::Mob => None,
    }
}

fn model_alias(c: &CastDef) -> String {
    c.mind.model.clone().unwrap_or_else(|| "default".into())
}

fn model<'a>(cfg: &'a MudConfig, alias: &str) -> anyhow::Result<&'a ModelConfig> {
    cfg.models
        .get(alias)
        .or_else(|| cfg.models.get("default"))
        .with_context(|| format!("no [models.{alias}] (or [models.default]) in the config"))
}

fn littleguy_mind(
    cfg: &MudConfig,
    alias: &str,
    members: &[(&EntityId, &CastDef)],
    body: &str,
    tokens: &TokenRegistry,
    cursor: &dyn Fn(&EntityId) -> Seq,
) -> anyhow::Result<LittleguyMind> {
    let lg = &cfg.littleguy;
    let config = LittleguyConfig {
        bin: lg.bin.clone(),
        data_dir: lg.data_dir.clone(),
        mcp_url: cfg.server.mcp_url(),
        provider: lg.provider.clone(),
        model_name: lg
            .model_names
            .get(alias)
            .or_else(|| lg.model_names.get("default"))
            .cloned(),
        base_url: lg.base_url.clone(),
        max_steps: lg.max_steps,
        ..LittleguyConfig::default()
    };
    let mut personas = HashMap::new();
    for (id, c) in members {
        let token = tokens
            .issue_at((*id).clone(), cursor(id))
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        personas.insert(
            (*id).clone(),
            Persona {
                name: c.name.clone(),
                persona: c.persona.clone(),
                goal: c.goal.clone(),
                body: body.to_owned(),
                token,
            },
        );
    }
    Ok(LittleguyMind::new(config, personas))
}

/// The world's scripts, for the minds: the host and the game it reads.
#[derive(Debug, Clone, Copy)]
pub struct MindScripts<'a> {
    /// The scripts.
    pub host: &'a Arc<ScriptHost>,
    /// The world.
    pub game: &'a Arc<Game>,
}

/// The runtime of an entity: its tier's, unless its mind binding asks for
/// `runtime = "rhai"` and the config allows it.
fn effective_runtime(
    cfg: &MudConfig,
    id: &EntityId,
    tier_runtime: Runtime,
    scripts: Option<MindScripts<'_>>,
) -> Runtime {
    let Some(s) = scripts else {
        return tier_runtime;
    };
    if s.host.mind_runtime(id) == Some(MindRuntime::Rhai) {
        if cfg.scripts.allow_runtime_override {
            return Runtime::Scripted;
        }
        tracing::warn!(
            "{id}: runtime = \"rhai\" ignored ([scripts] allow_runtime_override = false)"
        );
    } else if s.host.has_mind(id) && tier_runtime != Runtime::Scripted {
        tracing::info!("{id}: runs on {tier_runtime:?}; its rhai mind script is unused");
    }
    tier_runtime
}

/// Builds every mind.
///
/// Littleguy entities get a fresh MCP token each, whose `mud_events`
/// cursor starts at `cursor(entity)`: after a restart, the entity's replay
/// floor (ADR 0011, [`mud_supervisor::replay::boot_floor`]). With
/// scripts, the scripted mind runs Rhai mind scripts too, and an entity
/// whose binding asks for `runtime = "rhai"` runs on it whatever its tier.
///
/// # Errors
/// A model alias is missing, a key variable is unset, or a client fails.
pub fn build(
    cfg: &MudConfig,
    pack: &WorldPack,
    tokens: &TokenRegistry,
    cursor: &dyn Fn(&EntityId) -> Seq,
    scripts: Option<MindScripts<'_>>,
) -> anyhow::Result<Minds> {
    let mut mind = ScriptedMind::from_cast(&pack.cast);
    if let Some(s) = scripts {
        mind = mind.with_scripts(Arc::clone(s.host), Arc::clone(s.game), cfg.world.seed);
    }
    let scripted: Arc<dyn Mind> = Arc::new(mind);
    let mut by_entity: HashMap<EntityId, Arc<dyn Mind>> = HashMap::new();
    let mut groups: HashMap<(Runtime, String), Vec<(&EntityId, &CastDef)>> = HashMap::new();
    for (id, c) in &pack.cast {
        let Some(rt) = runtime_for(cfg, c.mind.tier) else {
            continue;
        };
        let rt = effective_runtime(cfg, id, rt, scripts);
        match rt {
            Runtime::Scripted => {
                by_entity.insert(id.clone(), Arc::clone(&scripted));
            }
            Runtime::PluginAgent if c.mind.tier != Tier::Ambient => {
                anyhow::bail!(
                    "{id}: plugin-agent is for ambient NPCs; pick harvest or littleguy for {:?}",
                    c.mind.tier
                );
            }
            _ => groups
                .entry((rt, model_alias(c)))
                .or_default()
                .push((id, c)),
        }
    }
    // Every agent's prompt states the pack's own rule for a body, the one
    // `help` gives, never a rate of its own.
    let body = pack.body_rules();
    let mut littleguy = Vec::new();
    for ((rt, alias), members) in groups {
        let mind: Arc<dyn Mind> = match rt {
            Runtime::Harvest => {
                let m = model(cfg, &alias)?;
                let client: Arc<dyn AgentModel> = Arc::new(
                    OpenAiModel::with_key_from(&m.base_url, m.model.clone(), &m.key_env)
                        .map_err(|e| anyhow::anyhow!("model {alias}: {e}"))?,
                );
                let personas = members
                    .iter()
                    .map(|(id, c)| {
                        (
                            (*id).clone(),
                            HarvestPersona {
                                name: c.name.clone(),
                                persona: c.persona.clone(),
                                goal: c.goal.clone(),
                                body: body.clone(),
                            },
                        )
                    })
                    .collect();
                Arc::new(
                    HarvestMind::new(client, Some(cfg.littleguy.data_dir.clone()), personas)
                        .with_history(cfg.minds.harvest.history()?),
                )
            }
            Runtime::PluginAgent => {
                let m = model(cfg, &alias)?;
                let key = std::env::var(&m.key_env)
                    .with_context(|| format!("{} is not set (model {alias})", m.key_env))?;
                let client: Arc<dyn LlmClient> = Arc::new(
                    OpenAiCompatibleClient::new(m.base_url.clone(), m.model.clone(), key)
                        .map_err(|e| anyhow::anyhow!("model {alias}: {e}"))?,
                );
                let cards = members
                    .iter()
                    .map(|(id, c)| {
                        (
                            (*id).clone(),
                            (
                                c.name.clone(),
                                c.persona.clone(),
                                c.goal.clone(),
                                body.clone(),
                            ),
                        )
                    })
                    .collect();
                Arc::new(AmbientMind::new(client, cards))
            }
            Runtime::Littleguy => {
                let m = Arc::new(littleguy_mind(
                    cfg, &alias, &members, &body, tokens, cursor,
                )?);
                littleguy.push(Arc::clone(&m));
                m
            }
            Runtime::Scripted => continue,
        };
        for (id, _) in members {
            by_entity.insert(id.clone(), Arc::clone(&mind));
        }
    }
    Ok(Minds {
        by_entity,
        littleguy,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scripts_of(pack: &Arc<WorldPack>) -> (Arc<ScriptHost>, Arc<Game>) {
        let host = Arc::new(ScriptHost::bundled_crypt_v01(pack).unwrap());
        let game = Game::new(
            mud_world::World::new(Arc::clone(pack), 1),
            None,
            Arc::new(mud_world::ManualClock::new(0)),
        );
        (host, game)
    }

    #[test]
    fn minds_build_honours_runtime_rhai() {
        let pack = Arc::new(WorldPack::crypt_v01().unwrap());
        let (host, game) = scripts_of(&pack);
        let mut cfg = MudConfig::default();
        cfg.minds.ambient = Runtime::PluginAgent;
        // No model key is needed: Hale, the only ambient NPC, runs on Rhai.
        cfg.models.clear();
        let s = MindScripts {
            host: &host,
            game: &game,
        };
        let m = build(&cfg, &pack, &TokenRegistry::new(), &|_| Seq(0), Some(s)).unwrap();
        assert_eq!(m.by_entity[&EntityId::from("guard_01")].kind(), "scripted");
        assert!(
            build(&cfg, &pack, &TokenRegistry::new(), &|_| Seq(0), None).is_err(),
            "without scripts Hale needs a model"
        );
    }

    #[test]
    fn runtime_override_can_be_refused_by_config() {
        let pack = Arc::new(WorldPack::crypt_v01().unwrap());
        let (host, game) = scripts_of(&pack);
        let mut cfg = MudConfig::default();
        cfg.minds.ambient = Runtime::PluginAgent;
        cfg.scripts.allow_runtime_override = false;
        let s = MindScripts {
            host: &host,
            game: &game,
        };
        let id = EntityId::from("guard_01");
        assert_eq!(
            effective_runtime(&cfg, &id, Runtime::PluginAgent, Some(s)),
            Runtime::PluginAgent
        );
        cfg.scripts.allow_runtime_override = true;
        assert_eq!(
            effective_runtime(&cfg, &id, Runtime::PluginAgent, Some(s)),
            Runtime::Scripted
        );
        let torin = EntityId::from("torin");
        assert_eq!(
            effective_runtime(&cfg, &torin, Runtime::Littleguy, Some(s)),
            Runtime::Littleguy
        );
    }

    #[test]
    fn defaults_script_every_agent_and_issue_no_tokens() {
        let pack = WorldPack::crypt_v01().unwrap();
        let tokens = TokenRegistry::new();
        let m = build(&MudConfig::default(), &pack, &tokens, &|_| Seq(0), None).unwrap();
        let mut ids: Vec<&str> = m.by_entity.keys().map(EntityId::as_str).collect();
        ids.sort_unstable();
        assert_eq!(ids, ["guard_01", "marta", "pc_kestrel", "torin", "vich"]);
        assert!(m.by_entity.values().all(|m| m.kind() == "scripted"));
        assert!(tokens.is_empty());
    }

    #[test]
    fn littleguy_entities_get_one_token_each() {
        let pack = WorldPack::crypt_v01().unwrap();
        let tokens = TokenRegistry::new();
        let mut cfg = MudConfig::default();
        cfg.minds.persistent = Runtime::Littleguy;
        let torin = EntityId::from("torin");
        let cursor = |e: &EntityId| if *e == torin { Seq(9) } else { Seq(2) };
        let m = build(&cfg, &pack, &tokens, &cursor, None).unwrap();
        assert_eq!(tokens.len(), 3, "marta, torin, vich");
        assert_eq!(tokens.cursors_of(&torin), [Seq(9)], "issued at its floor");
        assert_eq!(tokens.cursors_of(&EntityId::from("marta")), [Seq(2)]);
        assert_eq!(m.by_entity[&EntityId::from("torin")].kind(), "littleguy");
        assert_eq!(
            m.by_entity[&EntityId::from("pc_kestrel")].kind(),
            "scripted"
        );
        assert_eq!(m.littleguy.len(), 1);
    }

    #[tokio::test]
    async fn every_agent_prompt_states_the_packs_own_body_rule() {
        // A pack whose recovery step is 5 minutes, with no come-to roll:
        // the prompt an lg serve gets says so, as `help` does.
        let mut pack = WorldPack::crypt_v01().unwrap();
        pack.recovery = mud_world::pack::RecoveryRules {
            come_to_roll: None,
            every_secs: 300,
        };
        let dir = tempfile::tempdir().unwrap();
        let tokens = TokenRegistry::new();
        let mut cfg = MudConfig::default();
        cfg.minds.persistent = Runtime::Littleguy;
        cfg.littleguy.bin = dir.path().join("no-such-lg");
        cfg.littleguy.data_dir = dir.path().join("agents");
        let m = build(&cfg, &pack, &tokens, &|_| Seq(0), None).unwrap();
        let marta = EntityId::from("marta");
        assert!(m.littleguy[0].ensure(&marta).await.is_err(), "no binary");
        let text =
            std::fs::read_to_string(m.littleguy[0].entity_dir(&marta).join("lg.toml")).unwrap();
        let v: toml::Table = toml::from_str(&text).unwrap();
        let system = v["system"].as_str().unwrap();
        assert!(system.contains(&pack.body_rules()), "{system}");
        assert!(system.contains("every 5 minutes"), "{system}");
        assert!(!system.contains("every minute"), "{system}");
        assert!(!system.contains("roll"), "{system}");
    }

    #[test]
    fn a_missing_model_alias_is_reported() {
        let pack = WorldPack::crypt_v01().unwrap();
        let mut cfg = MudConfig::default();
        cfg.minds.persistent = Runtime::Harvest;
        let e = build(&cfg, &pack, &TokenRegistry::new(), &|_| Seq(0), None).unwrap_err();
        assert!(e.to_string().contains("[models.flash]"), "{e}");
        cfg.minds.persistent = Runtime::PluginAgent;
        assert!(build(&cfg, &pack, &TokenRegistry::new(), &|_| Seq(0), None).is_err());
    }
}
