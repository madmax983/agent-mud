//! A harvest agent driving the MUD through its tools, on a scripted model.

use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;

use autumn_harvest_agent::{
    BoxFuture as HBoxFuture, ChatRequest, ChatResponse, ContentPart, StopReason, TokenUsage,
};
use mud_world::{Game, ManualClock, RoomId, Seq, World, WorldPack};
use serde_json::json;

use super::*;
use crate::gate::Gate;
use crate::router::Priority;

/// Answers with a fixed list of responses, one per call.
#[derive(Debug, Default)]
struct Scripted {
    replies: StdMutex<VecDeque<ChatResponse>>,
    seen: StdMutex<Vec<ChatRequest>>,
}

impl AgentModel for Scripted {
    fn chat<'a>(
        &'a self,
        request: &'a ChatRequest,
    ) -> HBoxFuture<'a, Result<ChatResponse, AgentError>> {
        self.seen.lock().unwrap().push(request.clone());
        let next = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| ChatResponse {
                content: vec![ContentPart::Text("done".into())],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::new(1, 1),
            });
        Box::pin(async move { Ok(next) })
    }
}

fn call(id: &str, name: &str, args: Value) -> ChatResponse {
    ChatResponse {
        content: vec![ContentPart::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args,
        }],
        stop_reason: StopReason::ToolUse,
        usage: TokenUsage::new(100, 20),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_durable_torin_posts_the_bounty_through_its_tools() {
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let game = Game::new(
        World::new(Arc::clone(&pack), 1),
        None,
        Arc::new(ManualClock::new(1_000)),
    );
    let typing = Gate::new(Arc::clone(&game), 1, 0.0);
    let model = Arc::new(Scripted::default());
    model.replies.lock().unwrap().extend([
        call("c1", "mud_look", json!({})),
        call("c2", "mud_command", json!({ "cmd": "move e" })),
        call("c3", "mud_command", json!({ "cmd": "move n" })),
        call(
            "c4",
            "mud_command",
            json!({ "cmd": "board post Ore wanted, 15s a chunk. -T" }),
        ),
        ChatResponse {
            content: vec![ContentPart::Text("posted the bounty".into())],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::new(50, 5),
        },
    ]);
    let dir = tempfile::tempdir().unwrap();
    let torin = EntityId::from("torin");
    let mind = HarvestMind::new(
        Arc::clone(&model) as Arc<dyn AgentModel>,
        Some(dir.path().to_path_buf()),
        HashMap::from([(
            torin.clone(),
            HarvestPersona {
                name: "Torin".into(),
                persona: "A smith.".into(),
                goal: "Ore.".into(),
            },
        )]),
    );
    let ctx = WakeContext {
        entity: torin.clone(),
        name: "Torin".into(),
        priority: Priority::MacroTick,
        reason: "macro-tick".into(),
        events: vec![],
        event_views: vec![],
        look: game.look(&torin),
        macro_tick: true,
        macro_ticks: 1,
        login: false,
        wake_id: "wake-torin-0-1".into(),
        now_ms: 0,
        booked: Vec::new(),
    };
    let tools = MudTools::new(Arc::clone(&game), typing, torin.clone());
    let r = mind.wake(ctx.clone(), tools.clone()).await.unwrap();
    assert_eq!(r.text.as_deref(), Some("posted the bounty"));
    assert!(r.usage.tokens() > 0, "{r:?}");
    let board = game.read(|w| {
        w.state().rooms[&RoomId::from("notice_market")]
            .board
            .clone()
    });
    assert_eq!(board.len(), 1);
    assert_eq!(board[0].text, "Ore wanted, 15s a chunk. -T");
    // The tools went out with schemas, and the wake text carried the room.
    let names: Vec<String> = model.seen.lock().unwrap()[0]
        .tools
        .iter()
        .map(|t| t.name.clone())
        .collect();
    assert_eq!(
        names,
        [
            "mud_look",
            "mud_sheet",
            "mud_events",
            "mud_command",
            "mud_wake_me",
            "mud_cancel_wake"
        ]
    );

    // The same wake again is a duplicate: no new model calls, no new post.
    let calls = model.seen.lock().unwrap().len();
    let _ = mind.wake(ctx, tools).await.unwrap();
    assert_eq!(
        model.seen.lock().unwrap().len(),
        calls,
        "the wake id is the idempotency key"
    );
    assert_eq!(
        game.read(|w| w.state().rooms[&RoomId::from("notice_market")].board.len()),
        1
    );
    assert!(dir.path().join("torin/harvest.db").exists());
    let evs = game.events(&torin, Seq(0), 50);
    assert!(
        evs["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "board_post")
    );
}

/// Looks once, then answers; holds call `hold_at` until released.
#[derive(Debug)]
struct Slow {
    calls: std::sync::atomic::AtomicUsize,
    hold_at: usize,
    release: tokio::sync::Notify,
}

impl AgentModel for Slow {
    fn chat<'a>(
        &'a self,
        _request: &'a ChatRequest,
    ) -> HBoxFuture<'a, Result<ChatResponse, AgentError>> {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            if n == self.hold_at {
                self.release.notified().await;
            }
            Ok(if n == 0 {
                call("c0", "mud_look", json!({}))
            } else {
                ChatResponse {
                    content: vec![ContentPart::Text(format!("done {n}"))],
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::new(50, 5),
                }
            })
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_timed_out_wake_is_charged_when_its_job_ends() {
    // Regression: the worker measured spend per job and sent it to a reply
    // nobody waited for, so a timed-out wake was never charged.
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let game = Game::new(
        World::new(Arc::clone(&pack), 1),
        None,
        Arc::new(ManualClock::new(1_000)),
    );
    let typing = Gate::new(Arc::clone(&game), 1, 0.0);
    let model = Arc::new(Slow {
        calls: std::sync::atomic::AtomicUsize::new(0),
        hold_at: 1,
        release: tokio::sync::Notify::new(),
    });
    let torin = EntityId::from("torin");
    let mind = HarvestMind::new(
        Arc::clone(&model) as Arc<dyn AgentModel>,
        None,
        HashMap::from([(
            torin.clone(),
            HarvestPersona {
                name: "Torin".into(),
                persona: "A smith.".into(),
                goal: "Ore.".into(),
            },
        )]),
    );
    let ctx = |id: &str| WakeContext {
        entity: torin.clone(),
        name: "Torin".into(),
        priority: Priority::RoomBatch,
        reason: "test".into(),
        events: vec![],
        event_views: vec![],
        look: Value::Null,
        macro_tick: false,
        macro_ticks: 0,
        login: false,
        wake_id: id.into(),
        now_ms: 0,
        booked: Vec::new(),
    };
    let tools = MudTools::new(Arc::clone(&game), typing, torin.clone());
    let cut = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        mind.wake(ctx("w1"), tools.clone()),
    )
    .await;
    assert!(cut.is_err(), "the first wake times out");
    assert!(tools.usage_sink().take(&torin).is_zero(), "nothing yet");
    model.release.notify_one();
    let mut late = Usage::default();
    for _ in 0..100 {
        late = tools.usage_sink().take(&torin);
        if !late.is_zero() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        late.tokens() > 0,
        "the abandoned job's spend reached the sink"
    );
    let r = mind.wake(ctx("w2"), tools.clone()).await.unwrap();
    assert!(r.usage.tokens() > 0);
    assert!(
        r.usage.tokens() < late.tokens(),
        "the next wake pays only its own call: {r:?} vs {late:?}"
    );
    assert!(tools.usage_sink().take(&torin).is_zero(), "charged once");
}
