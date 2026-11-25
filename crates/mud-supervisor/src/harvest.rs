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
//!
//! History stays bounded (ADR 0011): the agent continues as new every
//! [`HarvestHistory::ops_per_run`] wakes, carrying its state (its lifetime
//! token count with it) into a fresh run, so replay cost per wake stays
//! flat; and after a job, at most once per
//! [`HarvestHistory::purge_every_ms`] of game time, the worker purges
//! sealed runs older than [`HarvestHistory::keep_days`]. That window counts
//! on harvest's own clock (`closed_at`), the one exception to the game
//! clock, and it touches only the mind's own file. A RUNNING run is never
//! purged.
//!
//! Spend is the change in the agent's lifetime token count. The count the
//! budget was charged up to is stored with the day's tallies
//! (`harvest_tokens_seen`) and comes back at boot
//! ([`Mind::restore_tokens_seen`]), so tokens spent before a crash and not
//! yet charged are charged once, on the first wake after the boot.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use autumn_harvest_agent::{
    AGENT_WORKFLOW_NAME, AgentError, AgentHarness, AgentId, AgentModel, AgentOp, AgentProfile,
    FnTool, ToolEffect, sqlite,
};
use autumn_harvest_sqlite::{PurgePolicy, SqliteRuntime};
use mud_world::EntityId;
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::mind::{
    BoxFuture, InFlight, InFlightGuard, Mind, MindError, MindReply, MudTools, Usage, WakeContext,
    system_prompt, tool_specs, wake_message,
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
    /// The pack's rule for a body ([`mud_world::WorldPack::body_rules`]).
    pub body: String,
}

/// How a harvest agent's history stays bounded (`[minds.harvest]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarvestHistory {
    /// Wakes per run: the agent continues as new after this many.
    pub ops_per_run: u64,
    /// Sealed runs older than this many days are purged; 0 keeps them all.
    pub keep_days: u64,
    /// The least game time between two purges, in ms.
    pub purge_every_ms: u64,
}

impl Default for HarvestHistory {
    fn default() -> Self {
        Self {
            ops_per_run: 32,
            keep_days: 3,
            purge_every_ms: 3_600_000,
        }
    }
}

/// The sealed runs one purge call removes at most, in batches of
/// [`autumn_harvest_sqlite::PURGE_BATCH`]: one call stays short.
const PURGE_BATCHES: u32 = 4;

/// One wake for the worker: its text, its id, when it started (game
/// clock), where the reply goes, and the in-flight guard of its job.
struct Job {
    text: String,
    wake_id: String,
    now_ms: u64,
    reply: oneshot::Sender<Result<MindReply, MindError>>,
    _guard: InFlightGuard,
}

/// The lifetime token count each entity's budget was charged up to.
type Marks = Arc<std::sync::Mutex<HashMap<EntityId, u64>>>;

/// Durable in-process agents.
pub struct HarvestMind {
    model: Arc<dyn AgentModel>,
    data_dir: Option<PathBuf>,
    window: u32,
    history: HarvestHistory,
    personas: HashMap<EntityId, HarvestPersona>,
    workers: Mutex<HashMap<EntityId, mpsc::Sender<Job>>>,
    /// Jobs queued or running, per entity: a wake settles when its job is
    /// done, even after the wake gave up on it.
    in_flight: InFlight,
    /// The token marks: restored at boot, raised by each job before its
    /// spend is handed over.
    marks: Marks,
}

impl std::fmt::Debug for HarvestMind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HarvestMind")
            .field("data_dir", &self.data_dir)
            .field("entities", &self.personas.keys().collect::<Vec<_>>())
            .field("history", &self.history)
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

/// The tokens the agent has used in all, if its state can be read.
/// The query starts now, so the future holds no borrow of the runtime.
fn tokens_used(
    rt: &SqliteRuntime,
    id: &AgentId,
) -> impl std::future::Future<Output = Option<u64>> + Send + 'static {
    let query = Box::pin(sqlite::state(rt, id));
    async move { query.await.ok().flatten().map(|s| s.tokens_used) }
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
            history: HarvestHistory::default(),
            personas,
            workers: Mutex::new(HashMap::new()),
            in_flight: InFlight::default(),
            marks: Arc::default(),
        }
    }

    /// The same mind with these history bounds.
    #[must_use]
    pub const fn with_history(mut self, history: HarvestHistory) -> Self {
        self.history = history;
        self
    }

    /// The stored token marks, as boot hands them over (the spec's
    /// `with_baselines`; [`Mind::restore_tokens_seen`] does the same one
    /// entity at a time).
    #[must_use]
    pub fn with_baselines(self, baselines: HashMap<EntityId, u64>) -> Self {
        self.marks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(baselines);
        self
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
                        &persona.body,
                    ))
                    .window(self.window)
                    .max_ops_per_run(self.history.ops_per_run.max(1)),
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
        let (sink, who) = (Arc::clone(tools.usage_sink()), entity.clone());
        let (marks, history) = (Arc::clone(&self.marks), self.history);
        tokio::spawn(async move {
            // Spend is measured from the last count that was charged, not
            // from the start of this job, so a job whose wake was abandoned
            // (or failed) is still paid for, and so are tokens spent before
            // a crash that the store had not charged (the stored mark). A
            // mark above the count (a new harvest.db) starts from the count.
            let now_used = tokens_used(&rt, &id).await.unwrap_or(0);
            let mut last_seen = {
                let mut m = marks.lock().unwrap_or_else(PoisonError::into_inner);
                let start = m.get(&who).map_or(now_used, |&seen| seen.min(now_used));
                m.insert(who.clone(), start);
                drop(m);
                start
            };
            let mut last_purge: Option<u64> = None;
            while let Some(job) = rx.recv().await {
                let Job {
                    text,
                    wake_id,
                    now_ms,
                    reply,
                    _guard,
                } = job;
                let op = AgentOp::message_with_id(text, wake_id.clone());
                let out = match sqlite::send(&mut rt, &id, &op, Some(&wake_id)) {
                    Err(e) => Err(MindError::Failed(e.to_string())),
                    Ok(_) => match sqlite::drive(&mut rt, &id).await {
                        Err(e) => Err(MindError::Failed(e.to_string())),
                        Ok(_) => match sqlite::state(&rt, &id).await {
                            Ok(Some(s)) => Ok(MindReply {
                                text: s.last_answer.clone(),
                                ..MindReply::default()
                            }),
                            Ok(None) => Ok(MindReply::default()),
                            Err(e) => Err(MindError::Failed(e.to_string())),
                        },
                    },
                };
                let mut spent = Usage::default();
                if let Some(now) = tokens_used(&rt, &id).await {
                    spent.input_tokens = now.saturating_sub(last_seen);
                    last_seen = now;
                    // Raised before the spend is handed over: the wake's
                    // close stores both once the job is done.
                    marks
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(who.clone(), now);
                }
                match out {
                    Ok(mut r) => {
                        r.usage = spent;
                        // Nobody waits (the wake timed out): charge it anyway.
                        if let Err(Ok(r)) = reply.send(Ok(r)) {
                            sink.add(&who, r.usage);
                        }
                    }
                    Err(e) => {
                        sink.add(&who, spent);
                        let _ = reply.send(Err(e));
                    }
                }
                if purge_due(history, last_purge, now_ms) {
                    last_purge = Some(now_ms);
                    purge(&mut rt, &who, history.keep_days);
                }
            }
        });
        workers.insert(entity.clone(), tx.clone());
        drop(workers);
        Ok(tx)
    }
}

/// Whether a purge is due after a job at `now_ms` (game clock).
const fn purge_due(history: HarvestHistory, last: Option<u64>, now_ms: u64) -> bool {
    if history.keep_days == 0 {
        return false;
    }
    match last {
        None => true,
        Some(at) => now_ms.saturating_sub(at) >= history.purge_every_ms,
    }
}

/// Purges the agent's sealed runs older than `keep_days` (harvest's own
/// clock), a few batches at a time. A failure is logged: the next purge
/// tries again.
fn purge(rt: &mut SqliteRuntime, entity: &EntityId, keep_days: u64) {
    let age = Duration::from_secs(keep_days.saturating_mul(86_400));
    let policy = PurgePolicy::new()
        .override_age(AGENT_WORKFLOW_NAME, age)
        .batch_limit(PURGE_BATCHES);
    match rt.purge_with(&policy) {
        Ok(report) if report.removed > 0 => {
            tracing::info!(%entity, removed = report.removed, complete = report.complete, "purged old harvest runs");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(%entity, error = %e, "purging old harvest runs failed"),
    }
}

impl Mind for HarvestMind {
    fn kind(&self) -> &'static str {
        "harvest"
    }

    fn restore_tokens_seen(&self, entity: &EntityId, seen: u64) {
        self.marks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(entity.clone(), seen);
    }

    fn tokens_seen(&self, entity: &EntityId) -> Option<u64> {
        self.marks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(entity)
            .copied()
    }

    /// Resolves once the entity's jobs are done.
    fn settled(&self, entity: &EntityId) -> BoxFuture<'_, ()> {
        self.in_flight.settled(entity)
    }

    fn wake(
        &self,
        ctx: WakeContext,
        tools: MudTools,
    ) -> BoxFuture<'_, Result<MindReply, MindError>> {
        Box::pin(async move {
            let tx = self.worker(&ctx.entity, &tools).await?;
            let (reply_tx, reply_rx) = oneshot::channel();
            let job = self.in_flight.start(&ctx.entity);
            tx.send(Job {
                text: wake_message(&ctx),
                wake_id: ctx.wake_id.clone(),
                now_ms: ctx.now_ms,
                reply: reply_tx,
                _guard: job,
            })
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
