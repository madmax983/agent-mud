//! Ambient NPCs on autumn-plugin-agent: light, ephemeral, cheap.
//!
//! Guards, drunks and townsfolk do not need durable memory. Each keeps a
//! short rolling transcript in memory and a fixed persona card, and runs a
//! bounded `autumn_plugin_agent::Agent` loop over the four MUD tools. A
//! restart forgets the transcript, which is the point: they are scenery
//! that can talk back.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use autumn_plugin_agent::{
    Agent, AgentError, ChatMessage, ChatRequest, ChatResponse, ChatRole, FnTool, LlmClient,
    ToolEffect,
};
use mud_world::EntityId;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::mind::{
    BoxFuture, Mind, MindError, MindReply, MudTools, Usage, UsageMeter, UsageRecorder, WakeContext,
    system_prompt, tool_specs, wake_message,
};

/// A client that records each call's spend as it comes in, so a wake that
/// fails or is dropped by the timeout is still charged for its calls.
#[derive(Debug)]
struct Metered {
    inner: Arc<dyn LlmClient>,
    spent: UsageRecorder,
}

impl LlmClient for Metered {
    fn chat<'a>(
        &'a self,
        request: &'a ChatRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ChatResponse, AgentError>> + Send + 'a>> {
        Box::pin(async move {
            let r = self.inner.chat(request).await?;
            self.spent.add(Usage {
                calls: 1,
                input_tokens: u64::from(r.usage.input_tokens),
                output_tokens: u64::from(r.usage.output_tokens),
                cost_usd_micros: 0,
            });
            Ok(r)
        })
    }

    fn list_models(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, AgentError>> + Send + '_>> {
        self.inner.list_models()
    }

    fn provider_name(&self) -> &'static str {
        self.inner.provider_name()
    }
}

/// The most messages an ambient NPC remembers.
pub const HISTORY_CAP: usize = 24;

/// The tool rounds of one ambient wake.
pub const MAX_STEPS: u32 = 4;

/// Light NPC minds.
pub struct AmbientMind {
    client: Arc<dyn LlmClient>,
    cards: HashMap<EntityId, (String, String, String)>,
    history: Mutex<HashMap<EntityId, Vec<ChatMessage>>>,
}

impl std::fmt::Debug for AmbientMind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AmbientMind")
            .field("provider", &self.client.provider_name())
            .field("entities", &self.cards.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// Keeps the last `cap` messages, starting at a user message so a tool
/// call is never cut from its result.
#[must_use]
pub fn trim_history(mut messages: Vec<ChatMessage>, cap: usize) -> Vec<ChatMessage> {
    if messages.len() <= cap {
        return messages;
    }
    let mut start = messages.len() - cap;
    while start < messages.len() && messages[start].role != ChatRole::User {
        start += 1;
    }
    messages.drain(..start);
    messages
}

impl AmbientMind {
    /// A mind over `client`. `cards` holds each entity's name, persona and goal.
    #[must_use]
    pub fn new(
        client: Arc<dyn LlmClient>,
        cards: HashMap<EntityId, (String, String, String)>,
    ) -> Self {
        Self {
            client,
            cards,
            history: Mutex::new(HashMap::new()),
        }
    }

    fn agent(
        &self,
        card: &(String, String, String),
        tools: &MudTools,
        spent: UsageRecorder,
    ) -> Agent {
        let client: Arc<dyn LlmClient> = Arc::new(Metered {
            inner: Arc::clone(&self.client),
            spent,
        });
        let mut agent = Agent::new(client)
            .system_prompt(system_prompt(&card.0, &card.1, &card.2))
            .max_steps(MAX_STEPS);
        for (name, description, schema) in tool_specs() {
            let t = tools.clone();
            let effect = if name == "mud_command" {
                ToolEffect::Write
            } else {
                ToolEffect::ReadOnly
            };
            agent = agent.tool(
                FnTool::new(name, description, schema, move |input: Value| {
                    let t = t.clone();
                    async move { Ok::<Value, AgentError>(t.call(name, &input).await) }
                })
                .effect(effect)
                .shared(),
            );
        }
        agent
    }
}

impl Mind for AmbientMind {
    fn kind(&self) -> &'static str {
        "plugin-agent"
    }

    fn wake(
        &self,
        ctx: WakeContext,
        tools: MudTools,
    ) -> BoxFuture<'_, Result<MindReply, MindError>> {
        Box::pin(async move {
            let card = self
                .cards
                .get(&ctx.entity)
                .ok_or_else(|| MindError::Unavailable(format!("no persona for {}", ctx.entity)))?;
            let history = self
                .history
                .lock()
                .await
                .get(&ctx.entity)
                .cloned()
                .unwrap_or_default();
            // Dropped unsettled (an error, a timeout), the meter charges the
            // calls made so far through the supervisor's sink.
            let meter = UsageMeter::new(&tools);
            let turn = self
                .agent(card, &tools, meter.recorder())
                .run_turn(history, &wake_message(&ctx))
                .await
                .map_err(|e| MindError::Failed(e.to_string()))?;
            // The reply reports the spend from here on.
            let _ = meter.settle();
            let usage = turn.outcome.usage();
            let text = turn.outcome.text().map(str::to_owned);
            self.history
                .lock()
                .await
                .insert(ctx.entity.clone(), trim_history(turn.messages, HISTORY_CAP));
            Ok(MindReply {
                text,
                usage: Usage {
                    calls: u64::from(turn.outcome.steps_used()),
                    input_tokens: u64::from(usage.input_tokens),
                    output_tokens: u64::from(usage.output_tokens),
                    cost_usd_micros: 0,
                },
                commands: 0,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::sync::Mutex as StdMutex;

    use autumn_plugin_agent::{ChatRequest, ChatResponse, ContentPart, StopReason, TokenUsage};
    use mud_world::{Game, ManualClock, Seq, World, WorldPack};
    use serde_json::json;

    use super::*;
    use crate::gate::Gate;
    use crate::router::Priority;

    #[derive(Debug, Default)]
    struct Script(StdMutex<VecDeque<ChatResponse>>);

    impl LlmClient for Script {
        fn chat<'a>(
            &'a self,
            _r: &'a ChatRequest,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<ChatResponse, AgentError>> + Send + 'a>>
        {
            let next = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| ChatResponse {
                    content: vec![ContentPart::Text("ok".into())],
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::new(1, 1),
                });
            Box::pin(async move { Ok(next) })
        }

        fn list_models(
            &self,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<Vec<String>, AgentError>> + Send + '_>>
        {
            Box::pin(async { Ok(vec!["script".into()]) })
        }

        fn provider_name(&self) -> &'static str {
            "script"
        }
    }

    #[tokio::test]
    async fn the_guard_warns_a_newcomer_through_its_tools() {
        let pack = Arc::new(WorldPack::crypt_v01().unwrap());
        let game = Game::new(
            World::new(Arc::clone(&pack), 1),
            None,
            Arc::new(ManualClock::new(1_000)),
        );
        let typing = Gate::new(Arc::clone(&game), 1, 0.0);
        let ada = game.login("ada").unwrap();
        let client = Arc::new(Script::default());
        client.0.lock().unwrap().extend([
            ChatResponse {
                content: vec![ContentPart::ToolCall {
                    id: "c1".into(),
                    name: "mud_command".into(),
                    arguments: json!({ "cmd": "say stay out of the alley, stranger" }),
                }],
                stop_reason: StopReason::ToolUse,
                usage: TokenUsage::new(200, 30),
            },
            ChatResponse {
                content: vec![ContentPart::Text("warned the newcomer".into())],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::new(220, 10),
            },
        ]);
        let guard = EntityId::from("guard_01");
        let mind = AmbientMind::new(
            client,
            HashMap::from([(
                guard.clone(),
                (
                    "Hale".into(),
                    "A watchman.".into(),
                    "Keep the peace.".into(),
                ),
            )]),
        );
        let ctx = WakeContext {
            entity: guard.clone(),
            name: "Hale".into(),
            priority: Priority::RoomBatch,
            reason: "enter by ada".into(),
            events: vec![],
            event_views: vec![],
            look: game.look(&guard),
            macro_tick: false,
            macro_ticks: 0,
            login: false,
            wake_id: "w1".into(),
        };
        let r = mind
            .wake(ctx, MudTools::new(Arc::clone(&game), typing, guard.clone()))
            .await
            .unwrap();
        assert_eq!(r.text.as_deref(), Some("warned the newcomer"));
        assert_eq!(r.usage.tokens(), 460);
        let heard = game.events(&ada, Seq(0), 50);
        assert!(
            heard["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["untrusted_text"] == "stay out of the alley, stranger")
        );
        assert!(
            !mind.history.lock().await[&guard].is_empty(),
            "it remembers the exchange"
        );
    }

    /// Looks once, then never answers again.
    #[derive(Debug, Default)]
    struct Hangs(std::sync::atomic::AtomicBool);

    impl LlmClient for Hangs {
        fn chat<'a>(
            &'a self,
            _r: &'a ChatRequest,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<ChatResponse, AgentError>> + Send + 'a>>
        {
            let first = !self.0.swap(true, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                if !first {
                    std::future::pending::<()>().await;
                }
                Ok(ChatResponse {
                    content: vec![ContentPart::ToolCall {
                        id: "c1".into(),
                        name: "mud_look".into(),
                        arguments: json!({}),
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: TokenUsage::new(200, 30),
                })
            })
        }

        fn list_models(
            &self,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<Vec<String>, AgentError>> + Send + '_>>
        {
            Box::pin(async { Ok(vec![]) })
        }

        fn provider_name(&self) -> &'static str {
            "hangs"
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_timed_out_wake_still_reports_what_it_spent() {
        // Regression: the spend of a wake dropped by the timeout was lost.
        let pack = Arc::new(WorldPack::crypt_v01().unwrap());
        let game = Game::new(
            World::new(Arc::clone(&pack), 1),
            None,
            Arc::new(ManualClock::new(1_000)),
        );
        let typing = Gate::new(Arc::clone(&game), 1, 0.0);
        let guard = EntityId::from("guard_01");
        let mind = AmbientMind::new(
            Arc::new(Hangs::default()),
            HashMap::from([(guard.clone(), ("Hale".into(), "W.".into(), "P.".into()))]),
        );
        let ctx = WakeContext {
            entity: guard.clone(),
            name: "Hale".into(),
            priority: Priority::RoomBatch,
            reason: "test".into(),
            events: vec![],
            event_views: vec![],
            look: Value::Null,
            macro_tick: false,
            macro_ticks: 0,
            login: false,
            wake_id: "w1".into(),
        };
        let tools = MudTools::new(Arc::clone(&game), typing, guard.clone());
        let cut = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            mind.wake(ctx, tools.clone()),
        )
        .await;
        assert!(cut.is_err(), "the wake timed out");
        let owed = tools.usage_sink().take(&guard);
        assert_eq!(owed.tokens(), 230, "{owed:?}");
        assert_eq!(owed.calls, 1);
    }

    #[test]
    fn trimming_starts_at_a_user_message() {
        let msgs = vec![
            ChatMessage::text(ChatRole::User, "a"),
            ChatMessage::text(ChatRole::Assistant, "b"),
            ChatMessage::text(ChatRole::User, "c"),
            ChatMessage::text(ChatRole::Assistant, "d"),
        ];
        let t = trim_history(msgs.clone(), 3);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].role, ChatRole::User);
        assert_eq!(trim_history(msgs, 10).len(), 4);
    }
}
