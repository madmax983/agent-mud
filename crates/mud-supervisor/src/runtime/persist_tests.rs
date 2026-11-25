//! What the supervisor keeps across a restart (ADR 0011, phase 2), on a
//! `world.db` in a temporary folder. A "crash" makes the store refuse
//! every write from a chosen point on (what a killed process leaves is
//! the durable prefix), then the files are opened again.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

use mud_world::pack::{ScriptRule, ScriptTrigger};
use mud_world::side::RuleCountRow;
use mud_world::{EntityId, Event, Game, ManualClock, RoomId, Seq, World, WorldPack};
use serde_json::Value;

use super::*;
use crate::mind::{BoxFuture, InFlight, MindError};
use crate::scripted::ScriptedMind;

/// 20:00 on day 0 of the game clock.
const T0: u64 = 20 * 3_600_000;
const DAY: u64 = 86_400_000;

fn torin() -> EntityId {
    EntityId::from("torin")
}

fn marta() -> EntityId {
    EntityId::from("marta")
}

/// A world.db in a temporary folder, opened again at each restart.
struct Disk {
    dir: tempfile::TempDir,
    pack: Arc<WorldPack>,
}

impl Disk {
    fn new() -> Self {
        Self::with(WorldPack::crypt_v01().unwrap())
    }

    fn with(pack: WorldPack) -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            pack: Arc::new(pack),
        }
    }

    fn db(&self) -> PathBuf {
        self.dir.path().join("world.db")
    }

    fn game(&self, now: u64) -> (Arc<Game>, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new(now));
        let game = Game::open(Arc::clone(&self.pack), Some(&self.db()), 5, clock.clone()).unwrap();
        (game, clock)
    }

    fn sql<T>(&self, q: &str, f: impl Fn(&rusqlite::Row<'_>) -> rusqlite::Result<T>) -> Vec<T> {
        let conn = rusqlite::Connection::open(self.db()).unwrap();
        let mut st = conn.prepare(q).unwrap();
        st.query_map([], |r| f(r))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn booking_ids(&self) -> Vec<u64> {
        self.sql("SELECT id FROM sup_booking ORDER BY id", |r| {
            r.get::<_, i64>(0).map(|v| u64::try_from(v).unwrap())
        })
    }
}

/// One process's run: a supervisor over the disk's world.
struct Run {
    sup: Arc<Supervisor>,
    game: Arc<Game>,
    clock: Arc<ManualClock>,
    rx: broadcast::Receiver<Arc<Event>>,
    obs: broadcast::Receiver<Arc<ObserverEvent>>,
    /// The boot report, ahead of the feed, as the observer page shows it.
    boot: Vec<Arc<ObserverEvent>>,
}

fn quiet() -> SupervisorConfig {
    SupervisorConfig {
        first_macro_tick_secs: [3_600, 3_600],
        ..SupervisorConfig::default()
    }
}

/// Starts a process at `now` as the server does: opens the world, opens
/// the supervisor on what the world restored, and only then subscribes to
/// its feed.
fn start(
    disk: &Disk,
    now: u64,
    cfg: SupervisorConfig,
    minds: HashMap<EntityId, Arc<dyn Mind>>,
) -> Run {
    let (game, clock) = disk.game(now);
    start_on(disk, game, clock, cfg, minds)
}

/// [`start`] on a world the caller opened (a mind needs its game first).
fn start_on(
    disk: &Disk,
    game: Arc<Game>,
    clock: Arc<ManualClock>,
    cfg: SupervisorConfig,
    minds: HashMap<EntityId, Arc<dyn Mind>>,
) -> Run {
    let typing = Gate::new(Arc::clone(&game), 5, 0.0);
    let fallback = Arc::new(FallbackMind::from_cast(&disk.pack.cast));
    let rx = game.subscribe();
    let restored = game.take_restored().unwrap_or_default();
    let sup = Supervisor::open(Arc::clone(&game), typing, minds, fallback, cfg, restored);
    let obs = sup.subscribe();
    let boot = sup.boot_report();
    Run {
        sup,
        game,
        clock,
        rx,
        obs,
        boot,
    }
}

impl Run {
    async fn run_for(&mut self, total_ms: u64, step_ms: u64) {
        let mut t = 0;
        while t <= total_ms {
            while let Ok(ev) = self.rx.try_recv() {
                self.sup.observe(&ev);
            }
            for h in self.sup.step() {
                h.await.unwrap();
            }
            while let Ok(ev) = self.rx.try_recv() {
                self.sup.observe(&ev);
            }
            self.clock.advance(step_ms);
            t += step_ms;
        }
    }

    /// Every feed entry so far: the boot report first.
    fn feed(&mut self) -> Vec<Arc<ObserverEvent>> {
        let mut out = std::mem::take(&mut self.boot);
        while let Ok(e) = self.obs.try_recv() {
            out.push(e);
        }
        out
    }

    /// The wakes started so far: (entity, wake id, reason).
    fn started(feed: &[Arc<ObserverEvent>]) -> Vec<(EntityId, String, String)> {
        feed.iter()
            .filter_map(|e| match &**e {
                ObserverEvent::WakeStarted {
                    entity,
                    wake_id,
                    reason,
                    ..
                } => Some((entity.clone(), wake_id.clone(), reason.clone())),
                _ => None,
            })
            .collect()
    }

    /// The posts on the market board by `name` that hold `text`.
    fn posts(&self, name: &str, text: &str) -> usize {
        self.game.read(|w| {
            w.state().rooms[&RoomId::from("notice_market")]
                .board
                .iter()
                .filter(|p| p.author.name == name && p.text.contains(text))
                .count()
        })
    }

    /// What `who` said aloud, from the stored events (the durable record).
    fn said(&self, who: &EntityId) -> Vec<String> {
        self.game.events(who, Seq(0), 1_000)["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["actor"]["id"] == who.as_str() && e["kind"] == "speech")
            .filter_map(|e| e["untrusted_text"].as_str().map(str::to_owned))
            .collect()
    }
}

/// From the `n`th effectful command of `who` on (0: at once), the store
/// refuses every write: the files hold what a process killed at that point
/// left behind.
fn crash_after(game: &Arc<Game>, who: &EntityId, n: usize) {
    if n == 0 {
        game.refuse_store_writes(true).unwrap();
        return;
    }
    let seen = AtomicUsize::new(0);
    let weak = Arc::downgrade(game);
    let who = who.clone();
    game.set_commit_probe(Some(Arc::new(move |actor: &EntityId| {
        if *actor == who
            && seen.fetch_add(1, Ordering::SeqCst) + 1 == n
            && let Some(g) = weak.upgrade()
        {
            g.refuse_store_writes(true).unwrap();
        }
    })));
}

/// The scripted mind for torin only.
fn scripted_torin(pack: &WorldPack) -> HashMap<EntityId, Arc<dyn Mind>> {
    let m: Arc<dyn Mind> = Arc::new(ScriptedMind::from_cast(&pack.cast));
    HashMap::from([(torin(), m)])
}

/// What a test mind saw of one wake.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    wake_id: String,
    macro_tick: bool,
    macro_ticks: u32,
    booked: Vec<u64>,
}

/// A mind that records each wake, runs its lines, and may crash (the
/// store refuses writes) after them, before the turn closes.
#[derive(Debug, Default)]
struct Probe {
    seen: Mutex<Vec<Seen>>,
    lines: Vec<String>,
    crash: bool,
    /// The run's game, which a crash stops writing.
    game: Mutex<Option<Arc<Game>>>,
}

impl Probe {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    /// Points a crashing probe at the run whose store it stops.
    fn arm(&self, game: &Arc<Game>) {
        *self.game.lock().unwrap() = Some(Arc::clone(game));
    }
}

impl Mind for Probe {
    fn kind(&self) -> &'static str {
        "probe"
    }

    fn wake(
        &self,
        ctx: WakeContext,
        tools: MudTools,
    ) -> BoxFuture<'_, Result<MindReply, MindError>> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(Seen {
                wake_id: ctx.wake_id.clone(),
                macro_tick: ctx.macro_tick,
                macro_ticks: ctx.macro_ticks,
                booked: ctx.booked.iter().map(|b| b.id).collect(),
            });
            for l in &self.lines {
                tools.command(l).await;
            }
            let game = self.game.lock().unwrap().clone();
            if self.crash {
                game.unwrap().refuse_store_writes(true).unwrap();
            }
            Ok(MindReply::default())
        })
    }
}

fn probe(lines: &[&str], crash: bool) -> Arc<Probe> {
    Arc::new(Probe {
        lines: lines.iter().map(|l| (*l).to_owned()).collect(),
        crash,
        ..Probe::default()
    })
}

fn only(who: &EntityId, m: &Arc<Probe>) -> HashMap<EntityId, Arc<dyn Mind>> {
    HashMap::from([(who.clone(), Arc::clone(m) as Arc<dyn Mind>)])
}

// ---- torin's bounty, the plan cursor and resumes ----

#[tokio::test]
async fn torin_posts_the_bounty_once_across_restarts() {
    // n counts torin's effectful lines before the crash: 0 is a crash
    // after the tick was queued, before any line took effect.
    for n in 0..=5 {
        let disk = Disk::new();
        let mut r = start(&disk, T0, quiet(), scripted_torin(&disk.pack));
        r.sup.fire_macro_tick(&torin());
        crash_after(&r.game, &torin(), n);
        r.run_for(2_000, 500).await;
        assert_eq!(
            r.posts("Torin", "BOUNTY"),
            1,
            "the crashed run posts in memory"
        );
        drop(r);

        let mut r = start(&disk, T0 + 1_000, quiet(), scripted_torin(&disk.pack));
        let side = (r.sup.wakes().all().len(), r.game.plan_of(&torin()));
        r.run_for(240_000, 2_000).await;
        assert_eq!(
            r.posts("Torin", "BOUNTY"),
            1,
            "crash after {n} lines: {side:?}"
        );
        assert_eq!(
            r.game
                .read(|w| w.entity(&torin()).unwrap().room.to_string()),
            "smithy",
            "crash after {n}: torin walks home"
        );
        assert_eq!(r.game.plan_of(&torin()), None, "the plan is done");
        // The next tick is number 2: it posts nothing.
        r.sup.fire_macro_tick(&torin());
        r.run_for(2_000, 500).await;
        assert_eq!(r.posts("Torin", "BOUNTY"), 1, "tick 2 after a crash at {n}");
        let snap = r.sup.snapshot();
        let t = snap["entities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == "torin")
            .cloned()
            .unwrap();
        assert_eq!(t["macro_ticks"], 2, "crash after {n}");
    }
}

#[tokio::test]
async fn a_scripted_plan_resumes_from_its_cursor() {
    let disk = Disk::new();
    let mut r = start(&disk, T0, quiet(), scripted_torin(&disk.pack));
    r.sup.fire_macro_tick(&torin());
    crash_after(&r.game, &torin(), 1);
    r.run_for(2_000, 500).await;
    drop(r);
    let mut r = start(&disk, T0 + 1_000, quiet(), scripted_torin(&disk.pack));
    let plan = r.game.plan_of(&torin()).unwrap();
    assert_eq!(plan.cursor, 1, "move e took effect");
    assert!(plan.errand, "a macro tick's plan is an errand");
    r.run_for(4_000, 500).await;
    let feed = r.feed();
    let started = Run::started(&feed);
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0].2, "resume plan at line 2");
    let ok = feed.iter().any(|e| {
        matches!(&**e, ObserverEvent::WakeFinished { ok: true, note: Some(n), .. }
            if n.starts_with("resumed at line 2") && !n.contains("move e ->"))
    });
    assert!(ok, "the resume ran lines 2 to 5 only: {feed:?}");
}

#[tokio::test]
async fn a_refused_line_may_rerun_but_an_effectful_one_never_does() {
    let disk = Disk::new();
    let mut cast = disk.pack.cast.clone();
    if let Some(c) = cast.get_mut(&torin()) {
        c.script = vec![ScriptRule {
            on: ScriptTrigger::MacroTick,
            nth: Some(1),
            who: mud_world::pack::Who::Any,
            room: None,
            keywords: Vec::new(),
            commands: vec!["say one".into(), "move up".into(), "say two".into()],
        }];
    }
    let mind = || -> HashMap<EntityId, Arc<dyn Mind>> {
        HashMap::from([(
            torin(),
            Arc::new(ScriptedMind::from_cast(&cast)) as Arc<dyn Mind>,
        )])
    };
    let mut r = start(&disk, T0, quiet(), mind());
    r.sup.fire_macro_tick(&torin());
    crash_after(&r.game, &torin(), 1);
    r.run_for(2_000, 500).await;
    drop(r);
    let mut r = start(&disk, T0 + 1_000, quiet(), mind());
    assert_eq!(r.game.plan_of(&torin()).unwrap().cursor, 1);
    r.run_for(4_000, 500).await;
    assert_eq!(r.said(&torin()), ["one", "two"], "each said once");
    let note = r.feed().iter().find_map(|e| match &**e {
        ObserverEvent::WakeFinished { note: Some(n), .. } => Some(n.clone()),
        _ => None,
    });
    assert!(
        note.is_some_and(|n| n.contains("move up -> refused")),
        "the refused line ran again"
    );
}

/// Writes a plan row for `who`, as a wake that made it would have.
fn plant_plan(game: &Game, who: &EntityId, lines: &[&str], cursor: u32, errand: bool, at: u64) {
    let wake_id = format!("wake-old-{who}");
    game.act(&[
        SideOp::PlanSet {
            entity: who.clone(),
            wake_id: wake_id.clone(),
            lines: lines.iter().map(|l| (*l).to_owned()).collect(),
            errand,
            started_ms: at,
        },
        SideOp::PlanCursor {
            entity: who.clone(),
            wake_id,
            cursor,
        },
    ])
    .unwrap();
}

#[tokio::test]
async fn an_old_reactive_plan_is_abandoned_and_an_errand_is_not() {
    let marta = marta();
    let disk = Disk::new();
    {
        let (game, _) = disk.game(T0);
        plant_plan(&game, &torin(), &["say a", "say b", "say c"], 1, true, T0);
        plant_plan(&game, &marta, &["say x", "say y"], 1, false, T0);
    }
    let minds = || -> HashMap<EntityId, Arc<dyn Mind>> {
        let m: Arc<dyn Mind> = Arc::new(ScriptedMind::default());
        HashMap::from([(torin(), Arc::clone(&m)), (marta.clone(), m)])
    };
    // Thirty days later: the errand resumes, the reactive plan does not.
    let mut r = start(&disk, T0 + 30 * DAY, quiet(), minds());
    r.run_for(4_000, 500).await;
    assert_eq!(r.said(&torin()), ["b", "c"]);
    assert!(r.said(&marta).is_empty(), "abandoned");
    let feed = r.feed();
    assert!(
        feed.iter().any(|e| matches!(&**e,
            ObserverEvent::PlanAbandoned { entity, cursor: 1, len: 2, .. } if *entity == marta)),
        "{feed:?}"
    );
    assert_eq!(r.game.plan_of(&marta), None);
    assert_eq!(r.game.plan_of(&torin()), None);
    drop(r);

    // A reactive plan inside the window resumes.
    let disk = Disk::new();
    {
        let (game, _) = disk.game(T0);
        plant_plan(&game, &marta, &["say x", "say y"], 1, false, T0);
    }
    let mut r = start(&disk, T0 + 60_000, quiet(), minds());
    r.run_for(4_000, 500).await;
    assert_eq!(r.said(&marta), ["y"]);
}

#[tokio::test]
async fn a_plan_whose_cursor_reached_its_end_is_done_at_boot() {
    let disk = Disk::new();
    {
        let (game, _) = disk.game(T0);
        plant_plan(&game, &torin(), &["say a"], 1, true, T0);
    }
    let mut r = start(&disk, T0 + 1_000, quiet(), scripted_torin(&disk.pack));
    r.run_for(2_000, 500).await;
    assert!(Run::started(&r.feed()).is_empty(), "nothing to resume");
    assert!(r.said(&torin()).is_empty(), "empty");
    assert_eq!(r.game.plan_of(&torin()), None);
}

#[tokio::test]
async fn a_resume_waits_for_its_entity_to_come_online() {
    let disk = Disk::new();
    {
        let (game, _) = disk.game(T0);
        plant_plan(&game, &torin(), &["say a", "say b"], 1, true, T0);
    }
    let m: Arc<dyn Mind> = Arc::new(ScriptedMind::default());
    let mut r = start(&disk, T0 + 1_000, quiet(), HashMap::from([(torin(), m)]));
    r.game.take_offline(&torin());
    r.run_for(2_000, 500).await;
    assert!(r.said(&torin()).is_empty(), "offline: it waits");
    assert!(!r.sup.router.lock().unwrap().is_idle(&torin()));
    r.game.bring_online(&torin());
    r.run_for(2_000, 500).await;
    assert_eq!(r.said(&torin()), ["b"]);
}

// ---- bookings ----

#[tokio::test]
async fn a_taken_booking_whose_wake_never_acted_fires_again_with_a_fresh_id() {
    let disk = Disk::new();
    let first = probe(&["look"], true);
    let mut r = start(&disk, T0, quiet(), only(&torin(), &first));
    first.arm(&r.game);
    let b = r
        .sup
        .wakes()
        .book(&torin(), T0, 1, "check the bin")
        .unwrap();
    r.clock.set(b.due_ms);
    r.run_for(1_000, 500).await;
    assert_eq!(first.seen().len(), 1);
    assert_eq!(first.seen()[0].booked, [b.id]);
    assert_eq!(disk.booking_ids(), [b.id], "a look consumed nothing");
    drop(r);

    let again = probe(&["say on it"], false);
    let mut r = start(&disk, b.due_ms + 2_000, quiet(), only(&torin(), &again));
    r.run_for(1_000, 500).await;
    let seen = again.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].booked, [b.id], "the booking fires again");
    assert_ne!(seen[0].wake_id, first.seen()[0].wake_id, "with a fresh id");
    assert!(disk.booking_ids().is_empty(), "consumed this time");
}

#[tokio::test]
async fn a_booking_consumed_with_the_first_effectful_act_does_not_fire_again() {
    let disk = Disk::new();
    let first = probe(&["say on it"], true);
    let mut r = start(&disk, T0, quiet(), only(&torin(), &first));
    first.arm(&r.game);
    let b = r
        .sup
        .wakes()
        .book(&torin(), T0, 1, "check the bin")
        .unwrap();
    r.clock.set(b.due_ms);
    r.run_for(1_000, 500).await;
    assert_eq!(first.seen()[0].booked, [b.id]);
    drop(r);
    assert!(disk.booking_ids().is_empty(), "gone with the say");

    let again = probe(&[], false);
    let mut r = start(&disk, b.due_ms + 2_000, quiet(), only(&torin(), &again));
    r.run_for(2_000, 500).await;
    assert!(again.seen().is_empty(), "nothing fires again");
    assert!(r.sup.wakes().pending(&torin()).is_empty(), "empty");
}

#[tokio::test]
async fn a_booking_overdue_beyond_max_late_is_dropped_with_an_event() {
    let disk = Disk::new();
    let m = probe(&[], false);
    let r = start(&disk, T0, quiet(), only(&torin(), &m));
    let late = r.sup.wakes().book(&torin(), T0, 1, "late").unwrap();
    let kept = r.sup.wakes().book(&torin(), T0, 60, "kept").unwrap();
    drop(r);
    let cfg = SupervisorConfig {
        booking_max_late_ms: 30 * 60_000,
        ..quiet()
    };
    // The first is 31 minutes late, the second 0: one dropped, one fires.
    let mut r = start(&disk, late.due_ms + 31 * 60_000, cfg, only(&torin(), &m));
    let feed = r.feed();
    assert!(
        feed.iter().any(|e| matches!(&**e,
            ObserverEvent::BookingDropped { entity, id, .. } if *entity == torin() && *id == late.id)),
        "{feed:?}"
    );
    for e in &feed {
        assert_eq!(
            serde_json::to_value(&**e).unwrap()["note"],
            Value::Null,
            "the note stays out of the feed"
        );
    }
    assert_eq!(disk.booking_ids(), [kept.id]);
    r.clock.set(kept.due_ms);
    r.run_for(1_000, 500).await;
    assert_eq!(m.seen().last().unwrap().booked, [kept.id]);
}

#[tokio::test]
async fn booking_ids_never_repeat_after_restart() {
    let disk = Disk::new();
    let m = probe(&["say done"], false);
    let mut ids = BTreeSet::new();
    for boot in 0..3u64 {
        let mut r = start(&disk, T0 + boot * DAY, quiet(), only(&torin(), &m));
        for k in 0..2 {
            let b = r
                .sup
                .wakes()
                .book(&torin(), T0 + boot * DAY, 1 + k, "x")
                .unwrap();
            assert!(ids.insert(b.id), "id {} again", b.id);
        }
        // Fire and consume them all, so no row is left to raise the next id.
        r.run_for(3 * 60_000, 30_000).await;
        assert!(disk.booking_ids().is_empty(), "empty");
    }
}

#[tokio::test]
async fn hard_budget_requeue_keeps_booking_rows() {
    let disk = Disk::new();
    let m = probe(&["say on it"], false);
    let cfg = SupervisorConfig {
        daily_token_budget: Some(100),
        ..quiet()
    };
    let mut r = start(&disk, T0, cfg, only(&torin(), &m));
    let b = r.sup.wakes().book(&torin(), T0, 1, "x").unwrap();
    // Taken while within budget, then the budget runs out before it runs.
    for (e, list) in r.sup.wakes().take_due(b.due_ms, |_| true) {
        let note = lock(&r.sup.router).deliver_booked(&e, list, b.due_ms);
        drop(note);
    }
    r.sup.record_spend(
        &torin(),
        Usage {
            input_tokens: 1_000,
            ..Usage::default()
        },
    );
    r.clock.set(b.due_ms);
    r.run_for(0, 500).await;
    let feed = r.feed();
    assert!(
        feed.iter().any(|e| matches!(&**e,
            ObserverEvent::WakeStarted { mind, .. } if mind == "fallback")),
        "{feed:?}"
    );
    assert!(m.seen().is_empty(), "the own mind never ran");
    assert_eq!(disk.booking_ids(), [b.id], "the row stays");
    assert!(!r.sup.wakes().in_flight(b.id), "given back");
    assert_eq!(r.sup.wakes().pending(&torin()).len(), 1);
    // At the hard cap it is not taken again: no wake at all, not even
    // the fallback's.
    r.run_for(2_000, 500).await;
    assert!(m.seen().is_empty(), "empty");
    let again = r.feed();
    assert!(
        !again
            .iter()
            .any(|e| matches!(&**e, ObserverEvent::WakeStarted { .. })),
        "{again:?}"
    );
    // The next UTC day the budget rolls and the booking fires.
    r.clock.set(DAY + 3_600_000);
    r.run_for(1_000, 500).await;
    assert_eq!(m.seen()[0].booked, [b.id]);
    assert!(disk.booking_ids().is_empty(), "empty");
}

#[tokio::test]
async fn an_offline_pcs_obligations_wait_until_it_returns() {
    #[derive(Debug)]
    struct Leaves(Mutex<Option<Arc<Game>>>);
    impl Mind for Leaves {
        fn kind(&self) -> &'static str {
            "leaves"
        }
        fn wake(
            &self,
            ctx: WakeContext,
            _: MudTools,
        ) -> BoxFuture<'_, Result<MindReply, MindError>> {
            Box::pin(async move {
                let game = self.0.lock().unwrap().clone().unwrap();
                game.take_offline(&ctx.entity);
                game.refuse_store_writes(true).unwrap();
                Ok(MindReply::default())
            })
        }
    }
    let disk = Disk::new();
    // The mind steps out of the world in the middle of its wake, and the
    // process dies before the turn closes.
    let leaves = Arc::new(Leaves(Mutex::default()));
    let mut r = start(
        &disk,
        T0,
        quiet(),
        HashMap::from([(torin(), Arc::clone(&leaves) as Arc<dyn Mind>)]),
    );
    *leaves.0.lock().unwrap() = Some(Arc::clone(&r.game));
    r.game.bring_online(&torin());
    let b = r.sup.wakes().book(&torin(), T0, 1, "x").unwrap();
    r.clock.set(b.due_ms);
    r.run_for(0, 500).await;
    assert_eq!(Run::started(&r.feed()).len(), 1);
    drop(r);
    assert_eq!(disk.booking_ids(), [b.id], "take_offline consumed nothing");

    let m = probe(&["say back"], false);
    let mut r = start(&disk, b.due_ms + 1_000, quiet(), only(&torin(), &m));
    r.game.take_offline(&torin());
    r.run_for(5_000, 500).await;
    assert!(m.seen().is_empty(), "offline: the booking waits");
    assert_eq!(disk.booking_ids(), [b.id]);
    r.game.bring_online(&torin());
    r.run_for(1_000, 500).await;
    assert_eq!(m.seen()[0].booked, [b.id]);
    assert!(disk.booking_ids().is_empty(), "empty");
}

// ---- the macro schedule ----

/// The macro schedule stored for torin.
fn stored_next(disk: &Disk) -> Option<u64> {
    disk.sql(
        "SELECT next_macro_ms FROM sup_entity WHERE entity = 'torin'",
        |r| r.get::<_, Option<i64>>(0),
    )
    .into_iter()
    .next()
    .flatten()
    .map(|v| u64::try_from(v).unwrap())
}

#[tokio::test]
async fn an_owed_macro_tick_is_delivered_with_the_same_number() {
    let disk = Disk::new();
    let first = probe(&["look"], true);
    let mut r = start(&disk, T0, quiet(), only(&torin(), &first));
    first.arm(&r.game);
    r.sup.fire_macro_tick(&torin());
    r.run_for(0, 500).await;
    assert_eq!(first.seen()[0].macro_ticks, 1);
    drop(r);

    let again = probe(&["say on it"], false);
    let mut r = start(&disk, T0 + 1_000, quiet(), only(&torin(), &again));
    r.run_for(130_000, 1_000).await;
    let seen = again.seen();
    assert_eq!(seen.len(), 1, "delivered once, within the spread: {seen:?}");
    assert!(seen[0].macro_tick);
    assert_eq!(seen[0].macro_ticks, 1, "the same number");
    assert_ne!(seen[0].wake_id, first.seen()[0].wake_id);
}

#[tokio::test]
async fn a_restart_does_not_refire_a_future_macro_tick() {
    let disk = Disk::new();
    let m = probe(&["say tick"], false);
    let mut r = start(&disk, T0, quiet(), only(&torin(), &m));
    r.sup.fire_macro_tick(&torin());
    r.run_for(0, 500).await;
    let next = lock(&r.sup.state)[&torin()].next_macro_ms;
    drop(r);
    assert_eq!(stored_next(&disk), Some(next));
    let mut r = start(&disk, T0 + 1_000, quiet(), only(&torin(), &m));
    assert_eq!(
        lock(&r.sup.state)[&torin()].next_macro_ms,
        next,
        "it stands"
    );
    r.run_for(600_000, 10_000).await;
    assert_eq!(m.seen().len(), 1, "no tick at boot");
}

#[tokio::test]
async fn a_past_macro_tick_fires_once_within_the_spread() {
    let disk = Disk::new();
    let m = probe(&["say tick"], false);
    let mut r = start(&disk, T0, quiet(), only(&torin(), &m));
    r.sup.fire_macro_tick(&torin());
    r.run_for(0, 500).await;
    drop(r);
    // Thirty days down: dozens of ticks missed, one fires.
    let back = T0 + 30 * DAY;
    let mut r = start(&disk, back, quiet(), only(&torin(), &m));
    let at = lock(&r.sup.state)[&torin()].next_macro_ms;
    assert!((back..=back + 120_000).contains(&at), "{at}");
    assert_eq!(stored_next(&disk), Some(at), "written back");
    r.run_for(3_600_000, 10_000).await;
    let ticks: Vec<u32> = m.seen().iter().map(|s| s.macro_ticks).collect();
    assert_eq!(ticks, [1, 2], "one catch-up tick in the hour after boot");
}

#[tokio::test]
async fn a_backward_clock_jump_clamps_the_next_macro_tick() {
    let disk = Disk::new();
    let m = probe(&["say tick"], false);
    let mut r = start(&disk, T0 + 10 * DAY, quiet(), only(&torin(), &m));
    r.sup.fire_macro_tick(&torin());
    r.run_for(0, 500).await;
    drop(r);
    let mut r = start(&disk, T0, quiet(), only(&torin(), &m));
    let at = lock(&r.sup.state)[&torin()].next_macro_ms;
    assert!(
        (T0..=T0 + 6 * 3_600_000).contains(&at),
        "clamped to the longest interval: {at}"
    );
    r.run_for(0, 500).await;
}

// ---- wake ids ----

#[tokio::test]
async fn wake_ids_never_repeat_across_restarts() {
    let disk = Disk::new();
    let m = probe(&["say hi"], false);
    let mut ids = BTreeSet::new();
    let mut boots = BTreeSet::new();
    for boot in 0..3u64 {
        let mut r = start(&disk, T0, quiet(), only(&torin(), &m));
        boots.insert(r.game.boot());
        for _ in 0..3 {
            r.sup.fire_macro_tick(&torin());
            r.run_for(0, 500).await;
        }
        let started = Run::started(&r.feed());
        assert_eq!(started.len(), 3, "boot {boot}");
        for (_, id, _) in started {
            let (key, who) = crate::persist::WakeKey::parse(&id).unwrap();
            assert_eq!(who, torin());
            assert_eq!(key.epoch, r.game.epoch());
            assert!(ids.insert(id), "an id repeated");
        }
    }
    assert_eq!(boots.len(), 3, "every open bumps the boot");
}

#[tokio::test]
async fn a_world_reset_never_reuses_wake_ids_with_kept_lg_db() {
    // The minds' own files (lg.db, harvest.db) outlive a world reset; the
    // new world.db mints a new epoch, so their ids never meet the old ones.
    let m = probe(&[], false);
    let mut ids = BTreeSet::new();
    for (k, now) in [T0, T0 + 1].into_iter().enumerate() {
        let disk = Disk::new();
        let mut r = start(&disk, now, quiet(), only(&torin(), &m));
        assert_eq!(r.game.boot(), 1, "reset {k}");
        r.sup.fire_macro_tick(&torin());
        r.run_for(0, 500).await;
    }
    for s in m.seen() {
        assert!(ids.insert(s.wake_id), "an id repeated across the reset");
    }
    assert_eq!(ids.len(), 2);
}

#[tokio::test]
async fn in_memory_world_ids_differ_across_runs() {
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let m = probe(&[], false);
    for now in [T0, T0 + 1] {
        let clock = Arc::new(ManualClock::new(now));
        let game = Game::new(World::new(Arc::clone(&pack), 5), None, clock);
        let typing = Gate::new(Arc::clone(&game), 5, 0.0);
        let sup = Supervisor::new(
            Arc::clone(&game),
            typing,
            only(&torin(), &m),
            Arc::new(FallbackMind::from_cast(&pack.cast)),
            quiet(),
        );
        sup.fire_macro_tick(&torin());
        for h in sup.step() {
            h.await.unwrap();
        }
        assert_eq!(game.boot(), 0);
    }
    let seen = m.seen();
    assert_eq!(seen.len(), 2);
    assert_ne!(seen[0].wake_id, seen[1].wake_id);
}

// ---- settle before close ----

/// A mind whose op outlives its wake: the wake times out, the op still
/// runs a command a moment later, then settles.
#[derive(Debug, Default)]
struct Late {
    in_flight: InFlight,
    log: Arc<Mutex<Vec<String>>>,
}

impl Mind for Late {
    fn kind(&self) -> &'static str {
        "late"
    }

    fn wake(
        &self,
        ctx: WakeContext,
        tools: MudTools,
    ) -> BoxFuture<'_, Result<MindReply, MindError>> {
        let guard = self.in_flight.start(&ctx.entity);
        let log = Arc::clone(&self.log);
        Box::pin(async move {
            log.lock().unwrap().push(format!(
                "woke {:?}",
                ctx.booked.iter().map(|b| b.id).collect::<Vec<_>>()
            ));
            if ctx.booked.len() == 1 && ctx.booked[0].note == "first" {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    tools.command("say late").await;
                    log.lock().unwrap().push("late command".into());
                    drop(guard);
                });
                std::future::pending::<()>().await;
            }
            Ok(MindReply::default())
        })
    }

    fn settled(&self, entity: &EntityId) -> BoxFuture<'_, ()> {
        self.in_flight.settled(entity)
    }
}

#[tokio::test]
async fn a_late_lg_command_does_not_consume_the_next_wake() {
    let disk = Disk::new();
    let late = Arc::new(Late::default());
    let cfg = SupervisorConfig {
        wake_timeout_ms: 50,
        ..quiet()
    };
    let minds = HashMap::from([(torin(), Arc::clone(&late) as Arc<dyn Mind>)]);
    let r = start(&disk, T0, cfg, minds);
    let first = r.sup.wakes().book(&torin(), T0, 1, "first").unwrap();
    let second = r.sup.wakes().book(&torin(), T0, 2, "second").unwrap();
    r.clock.set(first.due_ms);
    // The first wake (and the first retention pass) start.
    let one = r.sup.step();
    // The first wake times out; its op is still running.
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(*late.log.lock().unwrap(), [format!("woke [{}]", first.id)]);
    r.clock.set(second.due_ms);
    let two = r.sup.step();
    assert!(two.is_empty(), "busy until the first settles");
    for h in one {
        h.await.unwrap();
    }
    assert_eq!(
        disk.booking_ids(),
        [second.id],
        "the late say consumed its own wake"
    );
    let two = r.sup.step();
    for h in two {
        h.await.unwrap();
    }
    let log = late.log.lock().unwrap().clone();
    assert_eq!(
        log,
        [
            format!("woke [{}]", first.id),
            "late command".to_owned(),
            format!("woke [{}]", second.id),
        ]
    );
    assert!(
        disk.booking_ids().is_empty(),
        "the second closed and consumed"
    );
}

// ---- rule counters ----

/// marta's cast with two login rules, the first one first.
fn login_cast(
    pack: &WorldPack,
    extra_above: bool,
) -> std::collections::BTreeMap<EntityId, mud_world::pack::CastDef> {
    let rule = |nth: u32, line: &str| ScriptRule {
        on: ScriptTrigger::Login,
        nth: Some(nth),
        who: mud_world::pack::Who::Any,
        room: None,
        keywords: Vec::new(),
        commands: vec![line.to_owned()],
    };
    let mut cast: std::collections::BTreeMap<EntityId, mud_world::pack::CastDef> = pack
        .cast
        .iter()
        .filter(|(id, _)| **id == marta())
        .map(|(id, c)| (id.clone(), c.clone()))
        .collect();
    if let Some(c) = cast.get_mut(&marta()) {
        c.script = vec![rule(1, "say first login"), rule(2, "say second login")];
        if extra_above {
            c.script.insert(0, rule(9, "say ninth login"));
        }
    }
    cast
}

async fn login_once(r: &mut Run) {
    r.game.bring_online(&marta());
    lock(&r.sup.router).force(&marta(), Priority::Immediate, "login", r.game.now_ms());
    r.run_for(0, 500).await;
}

#[tokio::test]
async fn rule_counters_survive_restart_for_login_rules() {
    let disk = Disk::new();
    let mind = |above: bool| -> HashMap<EntityId, Arc<dyn Mind>> {
        let m: Arc<dyn Mind> = Arc::new(ScriptedMind::from_cast(&login_cast(&disk.pack, above)));
        HashMap::from([(marta(), m)])
    };
    let mut r = start(&disk, T0, quiet(), mind(false));
    login_once(&mut r).await;
    assert_eq!(r.said(&marta()), ["first login"]);
    drop(r);
    // A rule inserted above leaves the counts of the others alone.
    let mut r = start(&disk, T0 + 1_000, quiet(), mind(true));
    login_once(&mut r).await;
    assert_eq!(r.said(&marta()), ["first login", "second login"]);
    let rows = disk.sql(
        "SELECT count FROM sup_rule_count WHERE entity = 'marta' ORDER BY count",
        |r| r.get::<_, i64>(0),
    );
    assert_eq!(rows, [1, 2, 2], "one row per rule: the new one fired once");
}

#[test]
fn inserting_a_rule_above_keeps_counts_and_an_edited_rule_counts_again() {
    let pack = WorldPack::crypt_v01().unwrap();
    let before = login_cast(&pack, false);
    let rules = &before[&marta()].script;
    let rows: Vec<RuleCountRow> = rules
        .iter()
        .map(|r| RuleCountRow {
            digest: crate::scripted::rule_digest(r),
            dup: 0,
            count: 1,
        })
        .collect();
    // An inserted rule matches no row; the old two keep theirs.
    let after = ScriptedMind::from_cast(&login_cast(&pack, true));
    after.restore(&marta(), &rows);
    assert_eq!(after.count(&marta(), 0), 0);
    assert_eq!(after.count(&marta(), 1), 1);
    assert_eq!(after.count(&marta(), 2), 1);
    // An edited rule counts from 0.
    let mut edited = login_cast(&pack, false);
    if let Some(c) = edited.get_mut(&marta()) {
        c.script[0].commands = vec!["say hello again".into()];
    }
    let edited = ScriptedMind::from_cast(&edited);
    edited.restore(&marta(), &rows);
    assert_eq!(edited.count(&marta(), 0), 0);
    assert_eq!(edited.count(&marta(), 1), 1);
}

// ---- the boot report reaches the feed ----

#[tokio::test]
async fn a_boot_through_open_reports_a_dropped_booking_and_an_abandoned_plan() {
    let disk = Disk::new();
    let m = probe(&[], false);
    {
        let r = start(&disk, T0, quiet(), only(&torin(), &m));
        r.sup.wakes().book(&torin(), T0, 1, "secret note").unwrap();
        plant_plan(&r.game, &marta(), &["say x", "say y"], 1, false, T0);
    }
    let minds = HashMap::from([
        (torin(), Arc::clone(&m) as Arc<dyn Mind>),
        (marta(), Arc::new(ScriptedMind::default()) as Arc<dyn Mind>),
    ]);
    // Two days down: the booking is too late, the reactive plan too old.
    let mut r = start(&disk, T0 + 2 * DAY, quiet(), minds);
    let feed = r.feed();
    assert!(
        feed.iter().any(
            |e| matches!(&**e, ObserverEvent::BookingDropped { entity, .. } if *entity == torin())
        ),
        "a subscriber after open sees the drop: {feed:?}"
    );
    assert!(
        feed.iter().any(
            |e| matches!(&**e, ObserverEvent::PlanAbandoned { entity, .. } if *entity == marta())
        ),
        "and the abandoned plan: {feed:?}"
    );
    assert!(disk.booking_ids().is_empty(), "the row is gone");
}

// ---- a failed close ----

#[tokio::test]
async fn a_failed_close_keeps_its_consume_for_the_next_commit() {
    let disk = Disk::new();
    // The mind only looks; then the disk fills before the turn closes.
    let m = probe(&["look"], true);
    let mut r = start(&disk, T0, quiet(), only(&torin(), &m));
    m.arm(&r.game);
    let b = r.sup.wakes().book(&torin(), T0, 1, "x").unwrap();
    r.clock.set(b.due_ms);
    r.run_for(0, 500).await;
    assert_eq!(m.seen().len(), 1);
    assert_eq!(disk.booking_ids(), [b.id], "the close could not write");
    assert!(
        r.sup.wakes().pending(&torin()).is_empty(),
        "the wake took it"
    );
    // The disk recovers: the next commit carries the close's consume, so
    // the store agrees with the book again.
    r.game.refuse_store_writes(false).unwrap();
    r.game.tick();
    assert!(
        disk.booking_ids().is_empty(),
        "consumed with the next commit"
    );
}

// ---- the lock order ----

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "a supervisor lock is held")]
fn a_durable_write_under_a_supervisor_lock_fails_in_debug_builds() {
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let game = Game::new(
        World::new(Arc::clone(&pack), 5),
        None,
        Arc::new(ManualClock::new(T0)),
    );
    let typing = Gate::new(Arc::clone(&game), 5, 0.0);
    let sup = Supervisor::new(
        game,
        typing,
        HashMap::new(),
        Arc::new(FallbackMind::from_cast(&pack.cast)),
        quiet(),
    );
    let held = lock(&sup.state);
    let booked = sup.wakes().book(&torin(), T0, 1, "x");
    drop(held);
    drop(booked);
}

// ---- wake ids and the Rhai seed ----

/// The wake ids torin's first wake gets, with marta woken first or not, on
/// an in-memory world at `T0`.
async fn torin_first_wake_id(marta_first: bool) -> String {
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let game = Game::new(
        World::new(Arc::clone(&pack), 5),
        None,
        Arc::new(ManualClock::new(T0)),
    );
    let typing = Gate::new(Arc::clone(&game), 5, 0.0);
    let (t, m) = (probe(&[], false), probe(&[], false));
    let sup = Supervisor::new(
        game,
        typing,
        HashMap::from([
            (torin(), Arc::clone(&t) as Arc<dyn Mind>),
            (marta(), Arc::clone(&m) as Arc<dyn Mind>),
        ]),
        Arc::new(FallbackMind::from_cast(&pack.cast)),
        quiet(),
    );
    if marta_first {
        for _ in 0..3 {
            sup.fire_macro_tick(&marta());
            for h in sup.step() {
                h.await.unwrap();
            }
        }
    }
    sup.fire_macro_tick(&torin());
    for h in sup.step() {
        h.await.unwrap();
    }
    t.seen()[0].wake_id.clone()
}

#[tokio::test]
async fn an_entitys_wake_ids_do_not_depend_on_other_entities_wakes() {
    // The id seeds a Rhai mind's dice: another agent's wakes must not
    // shift it.
    assert_eq!(
        torin_first_wake_id(false).await,
        torin_first_wake_id(true).await
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_step_wakes_get_the_same_ids_on_every_run() {
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let mut first: Option<Vec<(EntityId, String)>> = None;
    for _ in 0..20 {
        let game = Game::new(
            World::new(Arc::clone(&pack), 5),
            None,
            Arc::new(ManualClock::new(T0)),
        );
        let typing = Gate::new(Arc::clone(&game), 5, 0.0);
        let probes: Vec<(EntityId, Arc<Probe>)> = ["torin", "marta", "guard_01", "pc_kestrel"]
            .into_iter()
            .map(|e| (EntityId::from(e), probe(&[], false)))
            .collect();
        let sup = Supervisor::new(
            Arc::clone(&game),
            typing,
            probes
                .iter()
                .map(|(e, p)| (e.clone(), Arc::clone(p) as Arc<dyn Mind>))
                .collect(),
            Arc::new(FallbackMind::from_cast(&pack.cast)),
            quiet(),
        );
        for (e, _) in &probes {
            game.bring_online(e);
            lock(&sup.router).force(e, Priority::Immediate, "test", T0);
        }
        for h in sup.step() {
            h.await.unwrap();
        }
        let ids: Vec<(EntityId, String)> = probes
            .iter()
            .map(|(e, p)| (e.clone(), p.seen()[0].wake_id.clone()))
            .collect();
        match &first {
            None => first = Some(ids),
            Some(f) => assert_eq!(f, &ids, "a wake id depends on the scheduler"),
        }
    }
}

// ---- stale rule counts ----

#[tokio::test]
async fn a_stale_rule_count_row_is_dropped_at_boot() {
    let disk = Disk::new();
    let mind = |cast: std::collections::BTreeMap<EntityId, mud_world::pack::CastDef>| {
        HashMap::from([(
            marta(),
            Arc::new(ScriptedMind::from_cast(&cast)) as Arc<dyn Mind>,
        )])
    };
    let original = login_cast(&disk.pack, false);
    let mut edited = original.clone();
    if let Some(c) = edited.get_mut(&marta()) {
        c.script[0].commands = vec!["say hello again".into()];
    }
    let mut r = start(&disk, T0, quiet(), mind(original.clone()));
    login_once(&mut r).await;
    assert_eq!(r.said(&marta()), ["first login"]);
    drop(r);
    let rows = || {
        disk.sql(
            "SELECT count FROM sup_rule_count WHERE entity = 'marta'",
            |r| r.get::<_, i64>(0),
        )
        .len()
    };
    assert_eq!(rows(), 2);
    // The edit leaves the first rule's row matching nothing: it goes.
    let r = start(&disk, T0 + 1_000, quiet(), mind(edited));
    drop(r);
    assert_eq!(rows(), 1, "the stale row is dropped");
    // Reverted, the first rule counts from 0 again: it fires.
    let mut r = start(&disk, T0 + 2_000, quiet(), mind(original));
    login_once(&mut r).await;
    assert_eq!(
        r.said(&marta()),
        ["first login", "first login", "second login"]
    );
}

// ---- errands resume after any outage ----

#[tokio::test]
async fn a_login_errand_resumes_after_a_long_outage() {
    let disk = Disk::new();
    let mut cast = login_cast(&disk.pack, false);
    if let Some(c) = cast.get_mut(&marta()) {
        c.script = vec![ScriptRule {
            on: ScriptTrigger::Login,
            nth: Some(1),
            who: mud_world::pack::Who::Any,
            room: None,
            keywords: Vec::new(),
            commands: vec!["say one".into(), "say two".into()],
        }];
    }
    let mind = || {
        HashMap::from([(
            marta(),
            Arc::new(ScriptedMind::from_cast(&cast)) as Arc<dyn Mind>,
        )])
    };
    let mut r = start(&disk, T0, quiet(), mind());
    crash_after(&r.game, &marta(), 1);
    login_once(&mut r).await;
    drop(r);
    let mut r = start(&disk, T0 + 30 * DAY, quiet(), mind());
    let plan = r.game.plan_of(&marta()).unwrap();
    assert!(plan.errand, "a login's plan is an errand");
    r.run_for(4_000, 500).await;
    assert_eq!(r.said(&marta()), ["one", "two"], "it resumed a month later");
}

#[tokio::test]
async fn a_booked_rhai_errand_resumes_after_a_long_outage() {
    let disk = Disk::new();
    let hale = EntityId::from("guard_01");
    let mut sources = mud_script::ScriptSources {
        manifest: Some("[minds.guard_01]\nfile = \"h.rhai\"\nruntime = \"rhai\"\n".into()),
        ..mud_script::ScriptSources::default()
    };
    sources.files.insert(
        "h.rhai".into(),
        "fn on_wake(ctx) { if ctx.booked.len() > 0 { [\"say one\", \"say two\"] } else { [] } }\n"
            .into(),
    );
    let host = Arc::new(mud_script::ScriptHost::from_sources(&sources, &disk.pack).unwrap());
    let mind = |game: &Arc<Game>| {
        HashMap::from([(
            hale.clone(),
            Arc::new(ScriptedMind::from_cast(&disk.pack.cast).with_scripts(
                Arc::clone(&host),
                Arc::clone(game),
                7,
            )) as Arc<dyn Mind>,
        )])
    };
    let (game, clock) = disk.game(T0);
    let mut r = start_on(&disk, game.clone(), clock, quiet(), mind(&game));
    let b = r.sup.wakes().book(&hale, T0, 1, "rounds").unwrap();
    r.clock.set(b.due_ms);
    crash_after(&r.game, &hale, 1);
    r.run_for(1_000, 500).await;
    drop(r);
    let (game, clock) = disk.game(T0 + 30 * DAY);
    let mut r = start_on(&disk, game.clone(), clock, quiet(), mind(&game));
    let plan = r.game.plan_of(&hale).unwrap();
    assert!(plan.errand, "a booked wake's plan is an errand");
    r.run_for(4_000, 500).await;
    assert_eq!(r.said(&hale), ["one", "two"], "it resumed a month later");
}

// ---- a crash inside a resume ----

#[tokio::test]
async fn a_crash_inside_a_resume_never_repeats_its_effects() {
    let disk = Disk::new();
    let mut r = start(&disk, T0, quiet(), scripted_torin(&disk.pack));
    r.sup.fire_macro_tick(&torin());
    crash_after(&r.game, &torin(), 1);
    r.run_for(2_000, 500).await;
    drop(r);
    // The resume runs `move n` and the post, and dies right after it.
    let mut r = start(&disk, T0 + 1_000, quiet(), scripted_torin(&disk.pack));
    crash_after(&r.game, &torin(), 2);
    r.run_for(4_000, 500).await;
    assert_eq!(r.posts("Torin", "BOUNTY"), 1);
    drop(r);
    let mut r = start(&disk, T0 + 2_000, quiet(), scripted_torin(&disk.pack));
    assert_eq!(
        r.game.plan_of(&torin()).map(|p| p.cursor),
        Some(3),
        "the resume's cursor went with the post"
    );
    r.run_for(4_000, 500).await;
    assert_eq!(
        r.posts("Torin", "BOUNTY"),
        1,
        "one BOUNTY after two crashes"
    );
    assert_eq!(
        r.game
            .read(|w| w.entity(&torin()).unwrap().room.to_string()),
        "smithy"
    );
    assert_eq!(r.game.plan_of(&torin()), None);
}

// ---- taken bookings given back ----

#[tokio::test]
async fn a_turing_logout_gives_its_taken_booking_back() {
    let mut pack = WorldPack::crypt_v01().unwrap();
    let kestrel = EntityId::from("pc_kestrel");
    if let Some(spec) = pack
        .cast
        .get_mut(&kestrel)
        .and_then(|c| c.mind.turing.as_mut())
    {
        spec.active_windows_utc = vec!["20:00-21:00".into()];
        spec.daily_offset_minutes = 0;
    }
    let disk = Disk::with(pack);
    let m = probe(&[], false);
    let mut r = start(&disk, T0, quiet(), only(&kestrel, &m));
    r.run_for(0, 500).await;
    assert!(
        r.game.read(|w| w.entity(&kestrel).unwrap().online),
        "in session"
    );
    let b = r.sup.wakes().book(&kestrel, T0, 1, "x").unwrap();
    // Taken and waiting for its wake when the session ends.
    for (e, list) in r.sup.wakes().take_due(b.due_ms, |_| true) {
        drop(lock(&r.sup.router).deliver_booked(&e, list, b.due_ms));
    }
    assert!(r.sup.wakes().in_flight(b.id));
    let out = T0 + 90 * 60_000;
    r.clock.set(out);
    r.sup.turing_sessions(out);
    assert!(
        !r.game.read(|w| w.entity(&kestrel).unwrap().online),
        "logged out"
    );
    assert!(!r.sup.wakes().in_flight(b.id), "given back");
    // The next session: the booking fires in this process.
    r.clock.set(DAY + T0);
    r.run_for(60_000, 5_000).await;
    assert!(
        m.seen().iter().any(|s| s.booked == [b.id]),
        "{:?}",
        m.seen()
    );
    assert!(disk.booking_ids().is_empty(), "empty");
}

#[tokio::test]
async fn a_wake_whose_entity_left_before_it_ran_gives_its_booking_back() {
    let disk = Disk::new();
    let m = probe(&["say on it"], false);
    let mut r = start(&disk, T0, quiet(), only(&torin(), &m));
    let b = r.sup.wakes().book(&torin(), T0, 1, "x").unwrap();
    for (e, list) in r.sup.wakes().take_due(b.due_ms, |_| true) {
        drop(lock(&r.sup.router).deliver_booked(&e, list, b.due_ms));
    }
    r.clock.set(b.due_ms);
    r.game.take_offline(&torin());
    r.run_for(0, 500).await;
    assert!(m.seen().is_empty(), "offline: the wake did not run");
    assert!(!r.sup.wakes().in_flight(b.id), "given back");
    r.game.bring_online(&torin());
    r.run_for(1_000, 500).await;
    assert_eq!(m.seen()[0].booked, [b.id], "it fires on return");
    assert!(disk.booking_ids().is_empty(), "empty");
}
