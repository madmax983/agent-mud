//! Durable minds in process, on autumn-harvest.
//!
//! When no `lg` binary is installed, a persistent entity can still be a
//! durable agent: a harvest `AgentHarness` with the four MUD tools, on its
//! own SQLite file (`<dir>/<entity>/harvest.db`). Each wake is one
//! attended message op whose wake id is its idempotency key, so a crash
//! mid-wake replays without paying for a finished model call again.
//!
//! The SQLite runtime is a single writer. One task owns each entity's
//! runtime and serves its wakes in order.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use autumn_harvest_agent::{
    AgentError, AgentHarness, AgentId, AgentModel, AgentOp, AgentProfile, FnTool, ToolEffect,
    sqlite,
};
use autumn_harvest_sqlite::SqliteRuntime;
use mud_world::EntityId;
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::mind::{
    BoxFuture, Mind, MindError, MindReply, MudTools, Usage, WakeContext, system_prompt, tool_specs,
    wake_message,
};

/// The agent kind of every MUD entity in harvest.
pub const AGENT_KIND: &str = "mud";

/// What one entity's agent needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarvestPersona {
    /// The display name.
    pub name: String,
    /// The persona.
    pub persona: String,
    /// The private goal.
    pub goal: String,
}

type Job = (
    String,
    String,
    oneshot::Sender<Result<MindReply, MindError>>,
);

/// Durable in-process agents.
pub struct HarvestMind {
    model: Arc<dyn AgentModel>,
    data_dir: Option<PathBuf>,
    window: u32,
    personas: HashMap<EntityId, HarvestPersona>,
    workers: Mutex<HashMap<EntityId, mpsc::Sender<Job>>>,
}

impl std::fmt::Debug for HarvestMind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HarvestMind")
            .field("data_dir", &self.data_dir)
            .field("entities", &self.personas.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// The four MUD tools as harvest tools, scoped to one entity.
fn harvest_tools(tools: &MudTools) -> Vec<Arc<dyn autumn_harvest_agent::Tool>> {
    tool_specs()
        .into_iter()
        .map(|(name, description, schema)| {
            let t = tools.clone();
            let effect = if name == "mud_command" {
                ToolEffect::Write
            } else {
                ToolEffect::ReadOnly
            };
            FnTool::new(name, description, schema, move |input: Value| {
                let t = t.clone();
                async move { Ok::<Value, AgentError>(t.call(name, &input).await) }
            })
            .effect(effect)
            .shared()
        })
        .collect()
}

impl HarvestMind {
    /// A mind over `model`. With no `data_dir`, each agent lives in memory.
    #[must_use]
    pub fn new(
        model: Arc<dyn AgentModel>,
        data_dir: Option<PathBuf>,
        personas: HashMap<EntityId, HarvestPersona>,
    ) -> Self {
        Self {
            model,
            data_dir,
            window: 30,
            personas,
            workers: Mutex::new(HashMap::new()),
        }
    }

    fn open(
        &self,
        entity: &EntityId,
        tools: &MudTools,
    ) -> Result<(SqliteRuntime, AgentId), MindError> {
        let persona = self
            .personas
            .get(entity)
            .ok_or_else(|| MindError::Unavailable(format!("no persona for {entity}")))?;
        let unavailable = |e: &dyn std::fmt::Display| MindError::Unavailable(e.to_string());
        let mut rt = match &self.data_dir {
            Some(dir) => {
                let d = dir.join(entity.as_str());
                std::fs::create_dir_all(&d).map_err(|e| unavailable(&e))?;
                SqliteRuntime::open(d.join("harvest.db")).map_err(|e| unavailable(&e))?
            }
            None => SqliteRuntime::open_in_memory().map_err(|e| unavailable(&e))?,
        };
        let harness = AgentHarness::new(Arc::clone(&self.model))
            .tools(harvest_tools(tools))
            .agent(
                AgentProfile::new(AGENT_KIND)
                    .system(system_prompt(
                        &persona.name,
                        &persona.persona,
                        &persona.goal,
                    ))
                    .window(self.window),
            );
        sqlite::register(&mut rt, Arc::new(harness));
        let id = AgentId::new(AGENT_KIND, entity.as_str()).map_err(|e| unavailable(&e))?;
        Ok((rt, id))
    }

    async fn worker(
        &self,
        entity: &EntityId,
        tools: &MudTools,
    ) -> Result<mpsc::Sender<Job>, MindError> {
        let mut workers = self.workers.lock().await;
        if let Some(tx) = workers.get(entity).filter(|tx| !tx.is_closed()) {
            return Ok(tx.clone());
        }
        let (mut rt, id) = self.open(entity, tools)?;
        let (tx, mut rx) = mpsc::channel::<Job>(16);
        tokio::spawn(async move {
            while let Some((text, wake_id, reply)) = rx.recv().await {
                let before = sqlite::state(&rt, &id)
                    .await
                    .ok()
                    .flatten()
                    .map_or(0, |s| s.tokens_used);
                let op = AgentOp::message_with_id(text, wake_id.clone());
                let out = match sqlite::send(&mut rt, &id, &op, Some(&wake_id)) {
                    Err(e) => Err(MindError::Failed(e.to_string())),
                    Ok(_) => match sqlite::drive(&mut rt, &id).await {
                        Err(e) => Err(MindError::Failed(e.to_string())),
                        Ok(_) => match sqlite::state(&rt, &id).await {
                            Ok(Some(s)) => Ok(MindReply {
                                text: s.last_answer.clone(),
                                usage: Usage {
                                    calls: 0,
                                    input_tokens: s.tokens_used.saturating_sub(before),
                                    output_tokens: 0,
                                    cost_usd_micros: 0,
                                },
                                commands: 0,
                            }),
                            Ok(None) => Ok(MindReply::default()),
                            Err(e) => Err(MindError::Failed(e.to_string())),
                        },
                    },
                };
                let _ = reply.send(out);
            }
        });
        workers.insert(entity.clone(), tx.clone());
        drop(workers);
        Ok(tx)
    }
}

impl Mind for HarvestMind {
    fn kind(&self) -> &'static str {
        "harvest"
    }

    fn wake(
        &self,
        ctx: WakeContext,
        tools: MudTools,
    ) -> BoxFuture<'_, Result<MindReply, MindError>> {
        Box::pin(async move {
            let tx = self.worker(&ctx.entity, &tools).await?;
            let (reply_tx, reply_rx) = oneshot::channel();
            tx.send((wake_message(&ctx), ctx.wake_id.clone(), reply_tx))
                .await
                .map_err(|_| MindError::Unavailable("the agent task stopped".into()))?;
            reply_rx
                .await
                .map_err(|_| MindError::Unavailable("the agent task stopped".into()))?
        })
    }
}

#[cfg(test)]
mod tests;
