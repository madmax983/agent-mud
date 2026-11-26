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

/// The one persona of these tests: Torin the smith, as `id`.
fn smith(id: EntityId) -> HashMap<EntityId, HarvestPersona> {
    HashMap::from([(
        id,
        HarvestPersona {
            name: "Torin".into(),
            persona: "A smith.".into(),
            goal: "Ore.".into(),
            body: String::new(),
            wakes: true,
        },
    )])
}

/// The system text of a request the model saw.
fn system_of(request: &ChatRequest) -> String {
    request
        .messages
        .iter()
        .filter(|m| m.role == autumn_harvest_agent::ChatRole::System)
        .flat_map(|m| &m.content)
        .filter_map(|p| match p {
            ContentPart::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
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
        smith(torin.clone()),
    );
    let ctx = WakeContext {
        entity: torin.clone(),
        name: "Torin".into(),
        priority: Priority::MacroTick,
        reason: "macro-tick".into(),
        events: vec![],
        event_views: vec![],
        look: game.look(&torin),
        you: serde_json::Value::Null,
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
    let system = system_of(&model.seen.lock().unwrap()[0]);
    assert!(
        system.ends_with(&persistent_habits(crate::mind::ToolNames::BARE)),
        "{system}"
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

/// An ambient harvest mind (`wakes: false`) is shown the four world tools
/// and no wake habit: it may not book (ADR 0005).
#[tokio::test(flavor = "multi_thread")]
async fn an_ambient_harvest_mind_sees_no_wake_tools_or_habit() {
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let game = Game::new(
        World::new(Arc::clone(&pack), 1),
        None,
        Arc::new(ManualClock::new(1_000)),
    );
    let typing = Gate::new(Arc::clone(&game), 1, 0.0);
    let model = Arc::new(Scripted::default());
    let hale = EntityId::from("guard_01");
    let mut personas = smith(hale.clone());
    if let Some(p) = personas.get_mut(&hale) {
        p.wakes = false;
    }
    let mind = HarvestMind::new(Arc::clone(&model) as Arc<dyn AgentModel>, None, personas);
    let ctx = WakeContext {
        entity: hale.clone(),
        name: "Hale".into(),
        priority: Priority::MacroTick,
        reason: "macro-tick".into(),
        events: vec![],
        event_views: vec![],
        look: game.look(&hale),
        you: serde_json::Value::Null,
        macro_tick: true,
        macro_ticks: 1,
        login: false,
        wake_id: "wake-guard_01-0-1".into(),
        now_ms: 0,
        booked: Vec::new(),
    };
    let tools = MudTools::new(Arc::clone(&game), typing, hale.clone());
    mind.wake(ctx, tools).await.unwrap();
    let seen = model.seen.lock().unwrap().clone();
    assert!(!seen.is_empty(), "the model was asked");
    let names: Vec<String> = seen[0].tools.iter().map(|t| t.name.clone()).collect();
    assert_eq!(
        names,
        ["mud_look", "mud_sheet", "mud_events", "mud_command"]
    );
    let system = system_of(&seen[0]);
    assert!(
        system.contains("Ore."),
        "the persona's own prompt: {system}"
    );
    assert!(!system.contains("mud_wake_me"), "{system}");
    assert!(
        !system.contains(&persistent_habits(crate::mind::ToolNames::BARE)),
        "{system}"
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
        smith(torin.clone()),
    );
    let ctx = |id: &str| WakeContext {
        entity: torin.clone(),
        name: "Torin".into(),
        priority: Priority::RoomBatch,
        reason: "test".into(),
        events: vec![],
        event_views: vec![],
        look: Value::Null,
        you: serde_json::Value::Null,
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

/// Answers only when released.
#[derive(Debug, Default)]
struct Held {
    go: tokio::sync::Notify,
}

impl AgentModel for Held {
    fn chat<'a>(
        &'a self,
        _request: &'a ChatRequest,
    ) -> HBoxFuture<'a, Result<ChatResponse, AgentError>> {
        Box::pin(async move {
            self.go.notified().await;
            Ok(ChatResponse {
                content: vec![ContentPart::Text("done".into())],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::new(1, 1),
            })
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_timed_out_wake_settles_only_once_its_job_is_done() {
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let game = Game::new(
        World::new(Arc::clone(&pack), 1),
        None,
        Arc::new(ManualClock::new(1_000)),
    );
    let typing = Gate::new(Arc::clone(&game), 1, 0.0);
    let model = Arc::new(Held::default());
    let torin = EntityId::from("torin");
    let mind = HarvestMind::new(
        Arc::clone(&model) as Arc<dyn AgentModel>,
        None,
        smith(torin.clone()),
    );
    let ctx = WakeContext {
        entity: torin.clone(),
        name: "Torin".into(),
        priority: Priority::Booked,
        reason: "test".into(),
        events: vec![],
        event_views: vec![],
        look: game.look(&torin),
        you: serde_json::Value::Null,
        macro_tick: false,
        macro_ticks: 0,
        login: false,
        wake_id: "wake-torin-0-1".into(),
        now_ms: 0,
        booked: Vec::new(),
    };
    let tools = MudTools::new(Arc::clone(&game), typing, torin.clone());
    let cut =
        tokio::time::timeout(std::time::Duration::from_millis(300), mind.wake(ctx, tools)).await;
    assert!(cut.is_err(), "the wake timed out");
    let mut settled = mind.settled(&torin);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), &mut settled)
            .await
            .is_err(),
        "the job still runs: not settled"
    );
    model.go.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(10), settled)
        .await
        .expect("settled once the job was done");
}

/// The states of the agent's runs in its `harvest.db`, with their counts.
fn runs(db: &std::path::Path) -> HashMap<String, i64> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut q = conn
        .prepare(
            "SELECT state, COUNT(*) FROM harvest_executions WHERE workflow_name = ?1 GROUP BY state",
        )
        .unwrap();
    q.query_map([AGENT_WORKFLOW_NAME], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// The most events any one run of the agent holds.
fn most_events_per_run(db: &std::path::Path) -> i64 {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row(
        "SELECT COALESCE(MAX(n), 0) FROM (SELECT COUNT(*) AS n FROM harvest_events GROUP BY exec_id)",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

/// Continue-as-new and retention keep `harvest.db` bounded. The spec's
/// run is 300 wakes at the default 32 ops per run; in a debug build that
/// takes about 38 s (replay grows within a run, from about 20 ms to 140 ms
/// a wake, and drops back at each continue-as-new), so this test keeps the
/// shape at 8 ops per run: ten runs, nine or more of them sealed.
#[tokio::test(flavor = "multi_thread")]
async fn harvest_db_history_is_bounded() {
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let game = Game::new(
        World::new(Arc::clone(&pack), 1),
        None,
        Arc::new(ManualClock::new(1_000)),
    );
    let typing = Gate::new(Arc::clone(&game), 1, 0.0);
    let dir = tempfile::tempdir().unwrap();
    let torin = EntityId::from("torin");
    let history = HarvestHistory {
        ops_per_run: 8,
        ..HarvestHistory::default()
    };
    let mind = HarvestMind::new(
        Arc::new(Scripted::default()) as Arc<dyn AgentModel>,
        Some(dir.path().to_path_buf()),
        smith(torin.clone()),
    )
    .with_history(history);
    let tools = MudTools::new(Arc::clone(&game), typing, torin.clone());
    let wakes = 80_u64;
    let started = std::time::Instant::now();
    let mut charged = 0;
    for n in 0..wakes {
        let ctx = WakeContext {
            entity: torin.clone(),
            name: "Torin".into(),
            priority: Priority::Booked,
            reason: "test".into(),
            events: vec![],
            event_views: vec![],
            look: Value::Null,
            you: serde_json::Value::Null,
            macro_tick: false,
            macro_ticks: 0,
            login: false,
            wake_id: format!("wake-{n}"),
            now_ms: n * 1_000,
            booked: Vec::new(),
        };
        let r = mind.wake(ctx, tools.clone()).await.unwrap();
        charged += r.usage.tokens();
    }
    let took = started.elapsed();
    eprintln!(
        "{wakes} wakes at ops_per_run = {}: {took:?}, {:?} per wake",
        history.ops_per_run,
        took / u32::try_from(wakes).unwrap()
    );
    assert_eq!(
        mind.tokens_seen(&torin),
        Some(charged),
        "the mark is the spend"
    );
    let db = dir.path().join("torin").join("harvest.db");
    let before = runs(&db);
    let sealed: i64 = before
        .iter()
        .filter(|(state, _)| state.as_str() != "RUNNING")
        .map(|(_, n)| n)
        .sum();
    assert!(sealed >= 9, "the agent continued as new: {before:?}");
    assert_eq!(before.get("RUNNING"), Some(&1), "{before:?}");
    let most = most_events_per_run(&db);
    assert!(
        most <= i64::try_from(history.ops_per_run).unwrap() * 16,
        "each run stays bounded: {most} events"
    );
    drop(mind);

    let mut rt = SqliteRuntime::open(&db).unwrap();
    sqlite::register(
        &mut rt,
        Arc::new(
            AgentHarness::new(Arc::new(Scripted::default()) as Arc<dyn AgentModel>)
                .agent(AgentProfile::new(AGENT_KIND)),
        ),
    );
    let report = rt
        .purge_with(&PurgePolicy::new().override_age(AGENT_WORKFLOW_NAME, Duration::ZERO))
        .unwrap();
    assert_eq!(report.removed, u64::try_from(sealed).unwrap());
    let after = runs(&db);
    assert_eq!(after.len(), 1, "only the running run is left: {after:?}");
    assert_eq!(after.get("RUNNING"), Some(&1));
    let id = AgentId::new(AGENT_KIND, torin.as_str()).unwrap();
    let state = sqlite::state(&rt, &id).await.unwrap().unwrap();
    assert_eq!(
        state.tokens_used, charged,
        "the token count carried through"
    );
}

/// The agent's sealed runs, by id.
fn sealed_ids(db: &std::path::Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut q = conn
        .prepare(
            "SELECT exec_id FROM harvest_executions
             WHERE workflow_name = ?1 AND state <> 'RUNNING' ORDER BY exec_id",
        )
        .unwrap();
    q.query_map([AGENT_WORKFLOW_NAME], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// The worker's own purge, after a job: sealed runs older than
/// `keep_days` go, at most once per `purge_every_ms` of game time; fresh
/// sealed runs and the RUNNING run stay.
#[tokio::test(flavor = "multi_thread")]
async fn the_worker_purges_sealed_runs_older_than_keep_days() {
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let game = Game::new(
        World::new(Arc::clone(&pack), 1),
        None,
        Arc::new(ManualClock::new(1_000)),
    );
    let typing = Gate::new(Arc::clone(&game), 1, 0.0);
    let dir = tempfile::tempdir().unwrap();
    let torin = EntityId::from("torin");
    let history = HarvestHistory {
        ops_per_run: 2,
        ..HarvestHistory::default()
    };
    let mind = HarvestMind::new(
        Arc::new(Scripted::default()) as Arc<dyn AgentModel>,
        Some(dir.path().to_path_buf()),
        smith(torin.clone()),
    )
    .with_history(history);
    let tools = MudTools::new(Arc::clone(&game), typing, torin.clone());
    let mut n = 0_u64;
    let mut wake_at = async |now_ms: u64| {
        n += 1;
        let ctx = WakeContext {
            entity: torin.clone(),
            name: "Torin".into(),
            priority: Priority::Booked,
            reason: "test".into(),
            events: vec![],
            event_views: vec![],
            look: Value::Null,
            you: serde_json::Value::Null,
            macro_tick: false,
            macro_ticks: 0,
            login: false,
            wake_id: format!("wake-{n}"),
            now_ms,
            booked: Vec::new(),
        };
        mind.wake(ctx, tools.clone()).await.unwrap();
        // The purge runs after the reply, before the job settles.
        mind.settled(&torin).await;
    };
    // The first job of the process purges: nothing is old yet.
    for i in 0..6 {
        wake_at(i * 1_000).await;
    }
    let db = dir.path().join("torin").join("harvest.db");
    let aged = sealed_ids(&db);
    assert!(aged.len() >= 2, "the agent continued as new: {aged:?}");
    // Those runs closed four days ago on harvest's clock (microseconds).
    let four_days_us: i64 = 4 * 86_400 * 1_000_000;
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE harvest_executions SET closed_at = closed_at - ?1 WHERE closed_at IS NOT NULL",
            [four_days_us],
        )
        .unwrap();
    // Within the purge interval nothing goes, and fresh runs are sealed.
    for i in 6..10 {
        wake_at(i * 1_000).await;
    }
    let all = sealed_ids(&db);
    assert!(
        aged.iter().all(|id| all.contains(id)),
        "no purge within purge_every_ms"
    );
    let fresh: Vec<String> = all.into_iter().filter(|id| !aged.contains(id)).collect();
    assert!(!fresh.is_empty(), "a fresh run was sealed");
    // An interval later, the next job purges the old runs only.
    wake_at(10_000 + history.purge_every_ms).await;
    let left = sealed_ids(&db);
    assert!(
        aged.iter().all(|id| !left.contains(id)),
        "the old runs are gone: {left:?}"
    );
    assert!(
        fresh.iter().all(|id| left.contains(id)),
        "runs younger than keep_days stay"
    );
    assert_eq!(runs(&db).get("RUNNING"), Some(&1), "the running run stays");
}

#[test]
fn purges_come_at_most_once_per_interval_and_never_with_keep_days_zero() {
    let h = HarvestHistory::default();
    assert!(purge_due(h, None, 0), "the first job of a process purges");
    assert!(!purge_due(h, Some(1_000), 1_000 + h.purge_every_ms - 1));
    assert!(purge_due(h, Some(1_000), 1_000 + h.purge_every_ms));
    let keep = HarvestHistory { keep_days: 0, ..h };
    assert!(!purge_due(keep, None, 0), "0 keeps every run");
}
