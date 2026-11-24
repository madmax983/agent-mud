//! The supervisor end to end, on scripted minds and a manual clock.

use std::collections::HashMap;
use std::sync::Arc;

use mud_world::{EntityId, Game, ManualClock, RoomId, Seq, World, WorldPack};

use super::*;
use crate::scripted::ScriptedMind;

struct Rig {
    sup: Arc<Supervisor>,
    game: Arc<Game>,
    clock: Arc<ManualClock>,
}

fn rig(cfg: SupervisorConfig) -> Rig {
    rig_with(cfg, HashMap::new())
}

/// A rig on scripted minds, except for the minds in `own`.
fn rig_with(cfg: SupervisorConfig, own: HashMap<EntityId, Arc<dyn Mind>>) -> Rig {
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let clock = Arc::new(ManualClock::new(20 * 3_600_000));
    let game = Game::new(World::new(Arc::clone(&pack), 5), None, clock.clone());
    let typing = Gate::new(Arc::clone(&game), 5, 0.0);
    let scripted: Arc<dyn Mind> = Arc::new(ScriptedMind::from_cast(&pack.cast));
    let mut minds: HashMap<EntityId, Arc<dyn Mind>> = pack
        .cast
        .iter()
        .filter(|(_, c)| matches!(c.mind.tier, Tier::Ambient | Tier::Persistent | Tier::Turing))
        .map(|(id, _)| (id.clone(), Arc::clone(&scripted)))
        .collect();
    minds.extend(own);
    let fallback = Arc::new(FallbackMind::from_cast(&pack.cast));
    let sup = Supervisor::new(Arc::clone(&game), typing, minds, fallback, cfg);
    Rig { sup, game, clock }
}

impl Rig {
    /// Steps the clock in `ms` increments, feeding events and finishing wakes.
    async fn run_for(&self, total_ms: u64, step_ms: u64, rx: &mut broadcast::Receiver<Arc<Event>>) {
        let mut t = 0;
        while t <= total_ms {
            while let Ok(ev) = rx.try_recv() {
                self.sup.observe(&ev);
            }
            for h in self.sup.step() {
                h.await.unwrap();
            }
            while let Ok(ev) = rx.try_recv() {
                self.sup.observe(&ev);
            }
            self.clock.advance(step_ms);
            t += step_ms;
        }
    }

    fn heard(&self, who: &EntityId) -> Vec<String> {
        self.game.events(who, Seq(0), 500)["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["untrusted_text"].as_str().map(str::to_owned))
            .collect()
    }
}

fn quiet() -> SupervisorConfig {
    SupervisorConfig {
        first_macro_tick_secs: [3_600, 3_600],
        ..SupervisorConfig::default()
    }
}

#[tokio::test]
async fn the_v01_smoke_path() {
    let r = rig(quiet());
    let mut rx = r.game.subscribe();
    let mut obs = r.sup.subscribe();
    // Macro-tick 1: Torin posts the ore bounty.
    r.sup.fire_macro_tick(&EntityId::from("torin"));
    r.run_for(1_000, 250, &mut rx).await;
    let board = r.game.read(|w| {
        w.state().rooms[&RoomId::from("notice_market")]
            .board
            .clone()
    });
    assert_eq!(board.len(), 1, "torin posted");
    assert_eq!(board[0].author.name, "Torin");
    // Kestrel's session starts: it reads the board and heads to the Flagon.
    r.run_for(2_000, 250, &mut rx).await;
    let k = EntityId::from("pc_kestrel");
    assert!(r.game.read(|w| w.entity(&k).unwrap().online));
    let k_saw = r.game.events(&k, Seq(0), 500);
    assert!(
        k_saw["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "board_read" && e["actor"]["is_self"] == true),
        "kestrel read the board"
    );
    assert_eq!(
        r.game.read(|w| w.entity(&k).unwrap().room.to_string()),
        "rusty_flagon_taproom"
    );
    // A human walks into the Flagon: kestrel tries to recruit them.
    let ada = r.game.login("ada").unwrap();
    r.game.execute(&ada, "e");
    r.run_for(10_000, 250, &mut rx).await;
    let heard = r.heard(&ada);
    assert!(
        heard
            .iter()
            .any(|t| t.contains("yo ada") && t.contains("ossuary")),
        "{heard:?}"
    );
    assert!(
        heard.iter().any(|t| t.contains("ale's a silver")),
        "marta greets too: {heard:?}"
    );
    // The observer saw wakes for each.
    let mut woke = std::collections::BTreeSet::new();
    while let Ok(e) = obs.try_recv() {
        if let ObserverEvent::WakeStarted { entity, .. } = &*e {
            woke.insert(entity.to_string());
        }
    }
    for who in ["torin", "pc_kestrel", "marta"] {
        assert!(woke.contains(who), "{who} never woke: {woke:?}");
    }
    let snap = r.sup.snapshot();
    let torin = snap["entities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "torin")
        .unwrap();
    assert_eq!(torin["macro_ticks"], 1);
    assert!(torin["goal"].as_str().unwrap().contains("ore"));
}

#[tokio::test]
async fn macro_ticks_come_on_their_own_and_wait_for_empty_rooms() {
    let cfg = SupervisorConfig {
        first_macro_tick_secs: [1, 1],
        macro_tick_scale: 0.001,
        ..SupervisorConfig::default()
    };
    let r = rig(cfg);
    let mut rx = r.game.subscribe();
    r.run_for(2_000, 500, &mut rx).await;
    let snap = r.sup.snapshot();
    let ticks = |id: &str| {
        snap["entities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == id)
            .unwrap()["macro_ticks"]
            .as_u64()
            .unwrap()
    };
    assert!(ticks("torin") >= 1);
    assert_eq!(ticks("guard_01"), 0, "ambient NPCs have no macro-tick");
}

fn board_len(r: &Rig) -> usize {
    r.game
        .read(|w| w.state().rooms[&RoomId::from("notice_market")].board.len())
}

fn torin_ticks(r: &Rig) -> u64 {
    r.sup.snapshot()["entities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "torin")
        .unwrap()["macro_ticks"]
        .as_u64()
        .unwrap()
}

#[tokio::test]
async fn a_macro_tick_merged_into_a_pending_wake_still_runs() {
    // Regression: the tick merged into torin's pending RoomBatch wake, was
    // counted, but ran as a RoomBatch, so the `nth = 1` bounty never posted.
    let r = rig(quiet());
    let mut rx = r.game.subscribe();
    let ada = r.game.login("ada").unwrap();
    r.game.execute(&ada, "w");
    r.game.execute(&ada, "e");
    while let Ok(ev) = rx.try_recv() {
        r.sup.observe(&ev);
    }
    let torin = EntityId::from("torin");
    assert!(
        lock(&r.sup.router)
            .pending()
            .iter()
            .any(|(e, p, _)| e == &torin && *p == Priority::RoomBatch),
        "torin has a pending room batch"
    );
    r.sup.fire_macro_tick(&torin);
    r.run_for(10_000, 250, &mut rx).await;
    assert_eq!(board_len(&r), 1, "torin posted the bounty");
    assert_eq!(torin_ticks(&r), 1);
}

#[tokio::test]
async fn a_distracted_spell_starts_only_when_the_pc_is_idle() {
    // Regression: spells started whenever the PC stood in a safe room, even
    // mid-wake or with a reply to a human queued.
    let r = rig(quiet());
    let k = EntityId::from("pc_kestrel");
    let afk = |r: &Rig| lock(&r.sup.state)[&k].afk_until;
    lock(&r.sup.state).get_mut(&k).unwrap().afk.next_at_ms = 1;
    let now = r.game.now_ms();
    r.sup.turing_sessions(now);
    assert!(r.game.read(|w| w.entity(&k).unwrap().online));
    assert_eq!(afk(&r), None, "not with the login wake pending");
    let wakes = lock(&r.sup.router).due(now);
    assert!(wakes.iter().any(|w| w.entity == k));
    r.sup.turing_sessions(now);
    assert_eq!(afk(&r), None, "not mid-wake");
    lock(&r.sup.router).finish(&k, now);
    r.sup.turing_sessions(now);
    assert!(afk(&r).is_some(), "idle in a safe room: away");
}

#[tokio::test]
async fn two_macro_ticks_merged_while_busy_count_once() {
    let r = rig(quiet());
    let torin = EntityId::from("torin");
    lock(&r.sup.router).force(&torin, Priority::RoomBatch, "x", 0);
    let wakes = lock(&r.sup.router).due(r.game.now_ms());
    assert_eq!(wakes.len(), 1);
    r.sup.fire_macro_tick(&torin);
    r.sup.fire_macro_tick(&torin);
    assert_eq!(torin_ticks(&r), 0, "nothing delivered yet");
    let now = r.game.now_ms();
    lock(&r.sup.router).finish(&torin, now);
    let mut rx = r.game.subscribe();
    r.run_for(1_000, 250, &mut rx).await;
    assert_eq!(torin_ticks(&r), 1, "one delivered tick");
    assert_eq!(board_len(&r), 1, "and it is the first");
}

#[tokio::test]
async fn a_spent_budget_falls_back_to_canned_flavor() {
    let mut cfg = quiet();
    cfg.budgets.insert(EntityId::from("torin"), Some(100));
    let r = rig(cfg);
    let mut rx = r.game.subscribe();
    r.sup.record_spend(
        &EntityId::from("torin"),
        Usage {
            calls: 1,
            input_tokens: 90,
            output_tokens: 20,
            cost_usd_micros: 5,
        },
    );
    let ada = r.game.login("ada").unwrap();
    r.game.execute(&ada, "w");
    r.run_for(5_000, 250, &mut rx).await;
    let heard = r.heard(&ada);
    assert!(
        !heard.iter().any(|t| t.contains("Got ore")),
        "the hard-capped script does not run: {heard:?}"
    );
    r.game.execute(&ada, "say torin, you there?");
    r.run_for(2_000, 250, &mut rx).await;
    let lines: Vec<String> = r.game.events(&ada, Seq(0), 500)["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "emote")
        .filter_map(|e| e["untrusted_text"].as_str().map(str::to_owned))
        .collect();
    assert!(
        lines.iter().any(|t| t == "grunts and keeps hammering."),
        "{lines:?}"
    );
    let snap = r.sup.snapshot();
    let torin = snap["entities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "torin")
        .unwrap();
    assert_eq!(torin["budget"]["level"], "hard");
}

#[test]
fn a_per_entity_unlimited_budget_overrides_the_default() {
    // Regression: `[supervisor.budgets] pc_kestrel = 0` meant a hard cap
    // of zero tokens, while `daily_token_budget = 0` meant no limit.
    let mut cfg = quiet();
    cfg.daily_token_budget = Some(10);
    cfg.budgets.insert(EntityId::from("torin"), None);
    let r = rig(cfg);
    let big = Usage {
        calls: 1,
        input_tokens: 1_000_000,
        output_tokens: 1,
        cost_usd_micros: 0,
    };
    r.sup.record_spend(&EntityId::from("torin"), big);
    r.sup.record_spend(&EntityId::from("marta"), big);
    let snap = r.sup.snapshot();
    let level = |id: &str| {
        snap["entities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == id)
            .unwrap()["budget"]["level"]
            .clone()
    };
    assert_eq!(level("torin"), "normal");
    assert_eq!(level("marta"), "hard");
}

#[tokio::test]
async fn sessions_end_outside_the_window() {
    let r = rig(quiet());
    let mut rx = r.game.subscribe();
    r.run_for(500, 250, &mut rx).await;
    let k = EntityId::from("pc_kestrel");
    assert!(r.game.read(|w| w.entity(&k).unwrap().online));
    // Kestrel's window in crypt-v01 is all day; a narrow one logs it out.
    let mut spec = r
        .game
        .read(|w| w.pack().cast[&k].mind.turing.clone().unwrap());
    spec.active_windows_utc = vec!["01:00-02:00".into()];
    spec.daily_offset_minutes = 0;
    assert!(!crate::turing::in_session(
        &spec,
        1,
        "kestrel",
        r.game.now_ms()
    ));
}

/// Regression: `snapshot` held `state` while it waited for the world, and
/// `macro_ticks` held the world while it waited for `state`. Run together,
/// they deadlocked the whole game.
#[test]
fn snapshot_and_step_never_deadlock() {
    // A runtime of its own: on a deadlock it is leaked rather than dropped,
    // so the test fails instead of hanging on shutdown.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let handle = rt.handle().clone();
    let r = {
        let _rt = handle.enter();
        rig(quiet())
    };
    let (done_tx, done_rx) = std::sync::mpsc::channel::<&'static str>();
    let sup = Arc::clone(&r.sup);
    let tx = done_tx.clone();
    std::thread::spawn(move || {
        for _ in 0..3_000 {
            let _ = sup.snapshot();
        }
        let _ = tx.send("snapshot");
    });
    let sup = Arc::clone(&r.sup);
    let clock = Arc::clone(&r.clock);
    std::thread::spawn(move || {
        let _rt = handle.enter();
        for _ in 0..3_000 {
            drop(sup.step());
            clock.advance(10);
        }
        let _ = done_tx.send("step");
    });
    for _ in 0..2 {
        let Ok(who) = done_rx.recv_timeout(std::time::Duration::from_secs(30)) else {
            std::mem::forget(rt);
            panic!("snapshot() and step() deadlocked");
        };
        assert!(["snapshot", "step"].contains(&who));
    }
}

/// Spends `now` and never answers; its model reports `later` after the
/// wake has been given up.
#[derive(Debug)]
struct Stalls {
    now: Usage,
    later: Usage,
}

impl Mind for Stalls {
    fn kind(&self) -> &'static str {
        "stalls"
    }

    fn wake(
        &self,
        _ctx: WakeContext,
        tools: MudTools,
    ) -> crate::mind::BoxFuture<'_, Result<MindReply, crate::mind::MindError>> {
        Box::pin(async move {
            let meter = crate::mind::UsageMeter::new(&tools);
            meter.recorder().add(self.now);
            let (sink, who, later) = (
                Arc::clone(tools.usage_sink()),
                tools.entity().clone(),
                self.later,
            );
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                sink.add(&who, later);
            });
            std::future::pending::<()>().await;
            Ok(MindReply {
                usage: meter.settle(),
                ..MindReply::default()
            })
        })
    }
}

fn spent(r: &Rig, who: &str) -> u64 {
    r.sup.snapshot()["entities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == who)
        .unwrap()["budget"]["tokens"]
        .as_u64()
        .unwrap()
}

#[tokio::test]
async fn a_timed_out_wake_is_still_charged() {
    // Regression: a timed-out or failed wake recorded zero spend, so a slow
    // model kept the entity at Normal however much it spent.
    let torin = EntityId::from("torin");
    let tokens = |n| Usage {
        calls: 1,
        input_tokens: n,
        output_tokens: 0,
        cost_usd_micros: 0,
    };
    let mind: Arc<dyn Mind> = Arc::new(Stalls {
        now: tokens(700),
        later: tokens(50),
    });
    let cfg = SupervisorConfig {
        wake_timeout_ms: 30,
        ..quiet()
    };
    let r = rig_with(cfg, HashMap::from([(torin.clone(), mind)]));
    let mut obs = r.sup.subscribe();
    r.sup.fire_macro_tick(&torin);
    for h in r.sup.step() {
        h.await.unwrap();
    }
    assert_eq!(spent(&r, "torin"), 700, "the spend before the timeout");
    let finished = std::iter::from_fn(|| obs.try_recv().ok()).find_map(|e| match &*e {
        ObserverEvent::WakeFinished {
            entity, ok, usage, ..
        } if entity == &torin => Some((*ok, *usage)),
        _ => None,
    });
    assert_eq!(finished, Some((false, tokens(700))));
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(r.sup.step());
    assert_eq!(spent(&r, "torin"), 750, "and what came in after it");
}

/// Remembers the booked notes of each wake, and books one itself.
#[derive(Debug, Default)]
struct Booker {
    seen: std::sync::Mutex<Vec<(Priority, Vec<String>)>>,
}

impl Mind for Booker {
    fn kind(&self) -> &'static str {
        "booker"
    }

    fn wake(
        &self,
        ctx: WakeContext,
        tools: MudTools,
    ) -> crate::mind::BoxFuture<'_, Result<MindReply, crate::mind::MindError>> {
        Box::pin(async move {
            let notes = ctx.booked.iter().map(|b| b.note.clone()).collect();
            let first = {
                let mut seen = self.seen.lock().unwrap();
                seen.push((ctx.priority, notes));
                seen.len() == 1
            };
            if first {
                let r = tools
                    .call(
                        "mud_wake_me",
                        &serde_json::json!({ "minutes": 2, "note": "post the bounty again" }),
                    )
                    .await;
                assert_eq!(r["ok"], true, "{r}");
            }
            Ok(MindReply::default())
        })
    }
}

#[tokio::test]
async fn a_wake_an_agent_booked_comes_back_and_can_act() {
    let torin = EntityId::from("torin");
    let booker = Arc::new(Booker::default());
    let r = rig_with(
        quiet(),
        HashMap::from([(torin.clone(), Arc::clone(&booker) as Arc<dyn Mind>)]),
    );
    let mut rx = r.game.subscribe();
    r.sup.fire_macro_tick(&torin);
    r.run_for(1_000, 250, &mut rx).await;
    assert_eq!(
        r.sup.wakes().pending(&torin).len(),
        1,
        "booked through its tools"
    );
    let snap = r.sup.snapshot();
    let row = snap["entities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "torin")
        .unwrap()
        .clone();
    assert_eq!(
        row["booked"][0]["note"], "post the bounty again",
        "the observer sees it"
    );
    // Not yet: two minutes have not passed.
    r.run_for(60_000, 1_000, &mut rx).await;
    assert_eq!(booker.seen.lock().unwrap().len(), 1);
    r.run_for(70_000, 1_000, &mut rx).await;
    let seen = booker.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_eq!(seen[1].0, Priority::Booked);
    assert_eq!(seen[1].1, ["post the bounty again"]);
    assert!(r.sup.wakes().pending(&torin).is_empty(), "delivered once");
}

#[tokio::test]
async fn an_agent_out_of_the_world_keeps_its_booked_wake_until_it_is_back() {
    let torin = EntityId::from("torin");
    let booker = Arc::new(Booker::default());
    let r = rig_with(
        quiet(),
        HashMap::from([(torin.clone(), Arc::clone(&booker) as Arc<dyn Mind>)]),
    );
    let mut rx = r.game.subscribe();
    let now = r.game.now_ms();
    r.sup
        .wakes()
        .book(&torin, now, 1, "check the grate")
        .unwrap();
    r.game.take_offline(&torin);
    r.run_for(120_000, 5_000, &mut rx).await;
    assert!(booker.seen.lock().unwrap().is_empty(), "no wake while away");
    assert_eq!(r.sup.wakes().pending(&torin).len(), 1, "kept");
    r.game.bring_online(&torin);
    r.run_for(5_000, 1_000, &mut rx).await;
    let seen = booker.seen.lock().unwrap().clone();
    assert_eq!(
        seen.first().map(|s| s.1.clone()),
        Some(vec!["check the grate".to_owned()])
    );
}

#[tokio::test]
async fn the_step_archives_then_trims_old_events_off_the_workers() {
    use mud_world::Clock;
    use mud_world::engine::EVENT_LOG_CAP;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("world.db");
    let arch = dir.path().join("archive");
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let clock = Arc::new(ManualClock::new(20 * 3_600_000));
    let game = Game::open(Arc::clone(&pack), Some(&db), 5, clock.clone()).unwrap();
    game.set_retention(mud_world::Retention {
        keep_days: 1,
        keep_min: 0,
        every_ms: 60_000,
        batch: 5_000,
        archive_dir: arch.clone(),
    });
    let ada = game.login("ada").unwrap();
    game.write(|w, now| {
        for i in 0..EVENT_LOG_CAP + 50 {
            w.execute(&ada, &format!("say {i}"), now);
        }
    });
    let last = game.read(World::last_seq).0;
    let typing = Gate::new(Arc::clone(&game), 5, 0.0);
    let fallback = Arc::new(FallbackMind::from_cast(&pack.cast));
    let sup = Supervisor::new(Arc::clone(&game), typing, HashMap::new(), fallback, quiet());
    // Too young: the step runs retention, which finds nothing to trim.
    for h in sup.step() {
        h.await.unwrap();
    }
    assert!(!arch.exists());
    clock.advance(2 * 86_400_000);
    let mut audit = game.subscribe_audit();
    for h in sup.step() {
        h.await.unwrap();
    }
    let old = last - EVENT_LOG_CAP as u64;
    assert_eq!(
        mud_world::archive::seqs(&arch).unwrap(),
        (1..=old).collect::<Vec<_>>()
    );
    let mut archived = None;
    while let Ok(a) = audit.try_recv() {
        if let mud_world::Audit::EventsArchived { trimmed, .. } = &*a {
            archived = Some(*trimmed);
        }
    }
    assert_eq!(archived, Some(old));
    // Claimed until `every_ms` passes.
    assert!(!game.retention_due(clock.now_ms()));
    assert_eq!(game.store_errors(), 0);
}
