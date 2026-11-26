//! Kill and restart (ADR 0011): a real process is killed with SIGKILL at
//! an exact point, the files are opened again, and no effect is lost or
//! doubled. torin's BOUNTY is posted once, a booking fires once, and no
//! wake id is used twice.
//!
//! The test binary runs itself again as the child, with
//! `MUD_KILL_CHILD=<point>:<dir>`. The child reads its game clock from
//! `<dir>/clock`, opens `<dir>/world.db` with torin on the scripted mind,
//! and plays until the point. There it writes `<dir>/marker` and stops for
//! good, holding the world, so the files hold exactly what a kill at that
//! point leaves. The parent then kills it and opens the files one second
//! later.
//!
//! The Steward's cases (ADR 0015) talk to a fake `lg serve` that lives in
//! this parent process, so it outlives the killed child: it counts every
//! `decide` it gets across the kill, and, in the refine cases, every
//! `overlay_put` and `overlay_activate`.
#![cfg(unix)]
#![expect(clippy::unwrap_used, reason = "test helpers and the child fail loudly")]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write as _;
use std::io::{BufRead as _, BufReader};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use mud_supervisor::gate::Gate;
use mud_supervisor::littleguy::{PersonaText, RefineMode};
use mud_supervisor::mind::{BoxFuture, Mind, MindError, MindReply, MudTools, Usage, WakeContext};
use mud_supervisor::runtime::{ObserverEvent, Supervisor, SupervisorConfig};
use mud_supervisor::scripted::{FallbackMind, ScriptedMind};
use mud_supervisor::steward::council::{Ballot, Judge, Vote};
use mud_supervisor::steward::{OwnedDaemons, Steward, StewardConfig, StewardEvent, StewardPoint};
use mud_world::side::StewardStored;
use mud_world::{EntityId, Event, Game, ManualClock, RoomId, WorldPack};
use tokio::sync::broadcast;

const CHILD: &str = "MUD_KILL_CHILD";
/// 20:00 on day 0 of the game clock.
const T0: u64 = 20 * 3_600_000;

fn torin() -> EntityId {
    EntityId::from("torin")
}

fn cfg() -> SupervisorConfig {
    SupervisorConfig {
        first_macro_tick_secs: [86_400, 86_400],
        // No world tick: its regeneration would roll dice the control
        // run does not.
        world_tick_ms: u64::MAX / 4,
        ..SupervisorConfig::default()
    }
}

/// One process on `<dir>/world.db`.
struct Proc {
    game: Arc<Game>,
    sup: Arc<Supervisor>,
    clock: Arc<ManualClock>,
    rx: broadcast::Receiver<Arc<Event>>,
    obs: broadcast::Receiver<Arc<ObserverEvent>>,
    /// The Steward's rows, split off before the supervisor took the rest.
    steward_rows: BTreeMap<EntityId, StewardStored>,
}

fn open(dir: &Path, now: u64, torin_mind: Arc<dyn Mind>) -> Proc {
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let clock = Arc::new(ManualClock::new(now));
    let game = Game::open(
        Arc::clone(&pack),
        Some(&dir.join("world.db")),
        5,
        clock.clone(),
    )
    .unwrap();
    let typing = Gate::new(Arc::clone(&game), 5, 0.0);
    let rx = game.subscribe();
    let mut restored = game.take_restored().unwrap_or_default();
    let steward_rows = std::mem::take(&mut restored.steward);
    let sup = Supervisor::open(
        Arc::clone(&game),
        typing,
        HashMap::from([(torin(), torin_mind)]),
        Arc::new(FallbackMind::from_cast(&pack.cast)),
        cfg(),
        restored,
    );
    let obs = sup.subscribe();
    Proc {
        game,
        sup,
        clock,
        rx,
        obs,
        steward_rows,
    }
}

impl Proc {
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

    fn bounties(&self) -> usize {
        self.game.read(|w| {
            w.state().rooms[&RoomId::from("notice_market")]
                .board
                .iter()
                .filter(|p| p.author.name == "Torin" && p.text.contains("BOUNTY"))
                .count()
        })
    }

    fn torin_room(&self) -> String {
        self.game
            .read(|w| w.entity(&torin()).unwrap().room.to_string())
    }

    fn dice(&self) -> u64 {
        self.game.read(|w| w.state().dice_state)
    }

    fn macro_ticks(&self) -> u64 {
        let snap = self.sup.snapshot();
        snap["entities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == "torin")
            .and_then(|e| e["macro_ticks"].as_u64())
            .unwrap()
    }

    /// The wakes started so far: (wake id, reason, mind) per wake.
    fn started(&mut self) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        while let Ok(e) = self.obs.try_recv() {
            if let ObserverEvent::WakeStarted {
                wake_id,
                reason,
                mind,
                ..
            } = &*e
            {
                out.push((wake_id.clone(), reason.clone(), mind.clone()));
            }
        }
        out
    }
}

// ---- the child ----

/// Writes `<dir>/marker` in one rename, so the parent never reads half.
fn mark(dir: &Path, text: &str) {
    let tmp = dir.join("marker.tmp");
    std::fs::write(&tmp, text).unwrap();
    std::fs::rename(&tmp, dir.join("marker")).unwrap();
}

/// Stops this process for good: the kill comes while it waits.
fn stop_here() -> ! {
    loop {
        std::thread::park();
    }
}

/// Appends each wake id to `<dir>/wakes` before the mind runs it.
#[derive(Debug)]
struct Recording {
    inner: Arc<dyn Mind>,
    file: PathBuf,
}

impl Mind for Recording {
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }

    fn wake(
        &self,
        ctx: WakeContext,
        tools: MudTools,
    ) -> BoxFuture<'_, Result<MindReply, MindError>> {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.file)
            .unwrap();
        writeln!(f, "{}", ctx.wake_id).unwrap();
        f.sync_data().unwrap();
        self.inner.wake(ctx, tools)
    }
}

/// A mind that marks the point and stops the process as its wake opens.
#[derive(Debug)]
struct Stall(PathBuf);

impl Mind for Stall {
    fn kind(&self) -> &'static str {
        "stall"
    }

    fn wake(&self, ctx: WakeContext, _: MudTools) -> BoxFuture<'_, Result<MindReply, MindError>> {
        let booked: Vec<String> = ctx.booked.iter().map(|b| b.id.to_string()).collect();
        mark(
            &self.0,
            &format!(
                "{} {} {}",
                ctx.macro_ticks,
                ctx.macro_tick,
                booked.join(",")
            ),
        );
        stop_here()
    }
}

/// A mind that answers every wake aloud.
#[derive(Debug)]
struct Answer;

impl Mind for Answer {
    fn kind(&self) -> &'static str {
        "answer"
    }

    fn wake(&self, _: WakeContext, tools: MudTools) -> BoxFuture<'_, Result<MindReply, MindError>> {
        Box::pin(async move {
            tools.command("say aye").await;
            Ok(MindReply::default())
        })
    }
}

fn recording(dir: &Path, inner: Arc<dyn Mind>) -> Arc<dyn Mind> {
    Arc::new(Recording {
        inner,
        file: dir.join("wakes"),
    })
}

fn scripted() -> Arc<dyn Mind> {
    let pack = WorldPack::crypt_v01().unwrap();
    Arc::new(ScriptedMind::from_cast(&pack.cast))
}

async fn child(point: &str, dir: &Path) {
    let now: u64 = std::fs::read_to_string(dir.join("clock"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mind = match point {
        "idle" | "tell-open" => recording(dir, Arc::new(Stall(dir.to_path_buf()))),
        "tell-pending" | "tell-answered" => recording(dir, Arc::new(Answer)),
        _ => recording(dir, scripted()),
    };
    let mut p = open(dir, now, mind);
    std::fs::write(dir.join("boot"), p.game.boot().to_string()).unwrap();
    match point {
        "book" => {
            let b = p
                .sup
                .wakes()
                .book(&torin(), now, 30, "check the bin")
                .unwrap();
            // The agent was told ok: the kill comes after the reply.
            mark(dir, &format!("{} {}", b.id, b.due_ms));
            stop_here();
        }
        "idle" => {
            let b = p
                .sup
                .wakes()
                .book(&torin(), now, 1, "check the bin")
                .unwrap();
            p.sup.fire_macro_tick(&torin());
            p.clock.set(b.due_ms);
            p.run_for(1_000, 500).await;
        }
        "retention" => retention_child(&p, dir),
        "steward-intent" | "steward-decide" => steward_child(point, &mut p, dir).await,
        "refine-put" | "refine-stored" | "refine-activate" | "refine-paused" => {
            refine_child(point, &mut p, dir).await;
        }
        "tell-pending" | "tell-open" | "tell-answered" => tell_child(point, &mut p, dir).await,
        "spend" => {
            // Past 80% of the default 400 000: soft. Stored at once.
            p.sup.record_spend(
                &torin(),
                Usage {
                    calls: 3,
                    input_tokens: 300_000,
                    output_tokens: 30_000,
                    cost_usd_micros: 1_234,
                },
            );
            let (level, spend) = p.sup.budget_of(&torin()).unwrap();
            mark(dir, &format!("{level:?} {}", spend.local.tokens));
            stop_here();
        }
        lines => {
            let n: usize = lines.trim_start_matches("line").parse().unwrap();
            let seen = AtomicUsize::new(0);
            let at = dir.to_path_buf();
            p.game
                .set_commit_probe(Some(Arc::new(move |actor: &EntityId| {
                    if *actor == torin() && seen.fetch_add(1, Ordering::SeqCst) + 1 == n {
                        mark(&at, &n.to_string());
                        stop_here();
                    }
                })));
            p.sup.fire_macro_tick(&torin());
            p.run_for(5_000, 500).await;
        }
    }
}

/// A human tells torin something; the kill comes before torin's wake is
/// taken (`tell-pending`), as it opens (`tell-open`), or once its answer
/// committed (`tell-answered`). The marker is the tell's seq.
async fn tell_child(point: &str, p: &mut Proc, dir: &Path) {
    let ada = p.game.login("ada").unwrap();
    assert!(p.game.execute(&ada, "tell torin got ore?").ok);
    let tell = p.game.read(|w| w.last_seq().0);
    match point {
        "tell-pending" => {
            mark(dir, &tell.to_string());
            stop_here();
        }
        "tell-answered" => {
            let at = dir.to_path_buf();
            p.game
                .set_commit_probe(Some(Arc::new(move |actor: &EntityId| {
                    if *actor == torin() {
                        mark(&at, &tell.to_string());
                        stop_here();
                    }
                })));
        }
        _ => {}
    }
    // `tell-open`: the stall marks (its own text) as the wake opens.
    p.run_for(5_000, 500).await;
}

/// How many events the retention child writes: past the newest
/// `EVENT_LOG_CAP` that are always kept.
const RETENTION_EVENTS: u64 = 13_000;

/// Writes enough events, ages them thirty days, then archives and trims
/// one row per transaction, so the kill lands in the middle of it.
fn retention_child(p: &Proc, dir: &Path) {
    for chunk in 0..RETENTION_EVENTS / 200 {
        p.game.write(|w, now| {
            for i in 0..200 {
                let r = w.execute(&torin(), &format!("say {}", chunk * 200 + i), now);
                assert!(r.ok);
            }
        });
    }
    let mut r = p.game.retention().unwrap();
    r.keep_days = 1;
    r.batch = 1;
    p.game.set_retention(r);
    let later = p.game.now_ms() + 30 * 86_400_000;
    p.clock.set(later);
    mark(dir, "retaining");
    p.game.retain(later).unwrap();
    // Done before the kill: the parent then sees a finished run.
    stop_here();
}

// ---- the Steward (ADR 0015) ----

/// The daemons of the Steward cases: torin's, on `<dir>/lg.sock`, which
/// the parent serves.
#[derive(Debug)]
struct KillDaemons(PathBuf);

impl OwnedDaemons for KillDaemons {
    fn entities(&self) -> Vec<EntityId> {
        vec![torin()]
    }

    fn running_socket<'a>(&'a self, _: &'a EntityId) -> BoxFuture<'a, Option<PathBuf>> {
        Box::pin(std::future::ready(Some(self.0.clone())))
    }

    fn start_key<'a>(&'a self, _: &'a EntityId) -> BoxFuture<'a, Option<u32>> {
        Box::pin(std::future::ready(Some(1)))
    }

    fn brief(&self, _: &EntityId) -> Option<PersonaText> {
        Some(PersonaText {
            name: "Torin".into(),
            persona: "A smith.".into(),
            goal: "Corner the ore.".into(),
            body: String::new(),
        })
    }

    fn settle<'a>(&'a self, _: &'a EntityId, _: u64) -> BoxFuture<'a, Usage> {
        Box::pin(std::future::ready(Usage::default()))
    }

    fn set_quiet<'a>(&'a self, _: &'a EntityId, _: bool) -> BoxFuture<'a, Result<(), MindError>> {
        Box::pin(std::future::ready(Ok(())))
    }

    fn is_quiet(&self, _: &EntityId) -> bool {
        false
    }
}

/// A council that accepts every goal.
#[derive(Debug)]
struct Yes;

impl Judge for Yes {
    fn vote<'a>(&'a self, _: &'a Ballot) -> BoxFuture<'a, Result<(Vote, Usage), MindError>> {
        Box::pin(std::future::ready(Ok((
            Vote {
                yes: true,
                ..Vote::default()
            },
            Usage::default(),
        ))))
    }
}

fn steward_of(p: &mut Proc, dir: &Path) -> Arc<Steward> {
    steward_with(p, dir, StewardConfig::default())
}

fn steward_with(p: &mut Proc, dir: &Path, cfg: StewardConfig) -> Arc<Steward> {
    Steward::new(
        Arc::clone(&p.sup),
        vec![Arc::new(KillDaemons(dir.join("lg.sock"))) as Arc<dyn OwnedDaemons>],
        Some(Arc::new(Yes) as Arc<dyn Judge>),
        cfg,
        std::mem::take(&mut p.steward_rows),
        p.game.boot(),
    )
}

/// torin proposed goal 1; the child's Steward accepts it and is killed
/// after the intent's write (`steward-intent`) or once its decide is out
/// (`steward-decide`: the parent's daemon marks it and never answers).
async fn steward_child(point: &str, p: &mut Proc, dir: &Path) {
    let steward = steward_of(p, dir);
    if point == "steward-intent" {
        let at = dir.to_path_buf();
        steward.set_probe(Some(Arc::new(move |pt, _: &EntityId| {
            if pt == StewardPoint::IntentWritten {
                mark(&at, "intent");
                stop_here();
            }
        })));
    }
    steward.pass().await;
}

/// The fake `lg serve` of the Steward cases, in the parent: goal 1
/// proposed; it applies each `decide` as it reads it and counts it. With
/// `hold`, it marks the child's marker and answers a decide only once
/// `released`.
struct ParentDaemon {
    goals: Mutex<Vec<(i64, String)>>,
    decides: Mutex<Vec<String>>,
    hold: AtomicBool,
    released: AtomicBool,
    marker: PathBuf,
}

impl ParentDaemon {
    fn start(dir: &Path, hold: bool) -> Arc<Self> {
        let me = Arc::new(Self {
            goals: Mutex::new(vec![(1, "proposed".into())]),
            decides: Mutex::new(Vec::new()),
            hold: AtomicBool::new(hold),
            released: AtomicBool::new(false),
            marker: dir.join("marker"),
        });
        let listener = UnixListener::bind(dir.join("lg.sock")).unwrap();
        let d = Arc::clone(&me);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let d = Arc::clone(&d);
                std::thread::spawn(move || d.serve(stream));
            }
        });
        me
    }

    fn plans(&self) -> String {
        let goals: Vec<serde_json::Value> = self
            .goals
            .lock()
            .unwrap()
            .iter()
            .map(|(id, status)| {
                serde_json::json!({ "id": id, "status": status, "title": "Sell ore",
                    "why": "coin", "check_every_minutes": 60, "notes": [] })
            })
            .collect();
        serde_json::json!({ "kind": "plans", "goals": goals, "ideas": [], "answered": [] })
            .to_string()
    }

    fn serve(&self, stream: std::os::unix::net::UnixStream) {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let answer = match v["kind"].as_str().unwrap() {
            "approvals" => r#"{"kind":"off","message":"no approvals"}"#.to_owned(),
            "plans" => self.plans(),
            "notes" => r#"{"kind":"notes","notes":[]}"#.to_owned(),
            "decide" => {
                let decision = v["decision"].as_str().unwrap().to_owned();
                self.decides
                    .lock()
                    .unwrap()
                    .push(format!("{decision} {}", v["id"]));
                for g in self.goals.lock().unwrap().iter_mut() {
                    if g.0 == v["id"].as_i64().unwrap() && g.1 == "proposed" && decision == "accept"
                    {
                        g.1 = "active".into();
                    }
                }
                if self.hold.load(Ordering::SeqCst) {
                    mark(self.marker.parent().unwrap(), "decide");
                    while !self.released.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                self.plans()
            }
            other => format!(r#"{{"kind":"error","message":"{other}"}}"#),
        };
        let mut stream = reader.into_inner();
        let _ = writeln!(stream, "{answer}");
    }

    fn decides(&self) -> Vec<String> {
        self.decides.lock().unwrap().clone()
    }
}

/// What the Steward published on `p`'s feed so far.
fn steward_events(p: &mut Proc) -> Vec<StewardEvent> {
    let mut out = Vec::new();
    while let Ok(e) = p.obs.try_recv() {
        if let ObserverEvent::Steward(s) = &*e {
            out.push(s.clone());
        }
    }
    out
}

/// After the kill: the Steward of a new process settles the goal with
/// exactly one `decide accept` in all, one `accepted` and one live accept.
async fn steward_after_kill(dir: &Path, daemon: &ParentDaemon, decides_at_kill: usize) {
    assert_eq!(daemon.decides().len(), decides_at_kill);
    let mut p = open(dir, T0 + 1_000, scripted());
    let steward = steward_of(&mut p, dir);
    let row = steward.row(&torin()).await.unwrap();
    assert!(
        row.intent.is_some(),
        "the intent was written before the kill"
    );
    for _ in 0..4 {
        steward.pass().await;
        p.clock.advance(600_000);
    }
    assert_eq!(daemon.decides(), ["accept 1"], "exactly one accept");
    let events = steward_events(&mut p);
    assert_eq!(
        events.iter().filter(|e| e.action == "accepted").count(),
        1,
        "{events:?}"
    );
    let row = steward.row(&torin()).await.unwrap();
    assert!(row.intent.is_none());
    assert_eq!(row.accepts.reserved, 1);
    after_restart(dir, &p, &[]);
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_after_accept_intent_before_decide() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = ParentDaemon::start(dir.path(), false);
    let marked = kill_at("steward-intent", dir.path(), T0, Duration::ZERO);
    assert_eq!(marked, "intent");
    steward_after_kill(dir.path(), &daemon, 0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_after_decide_before_its_answer() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = ParentDaemon::start(dir.path(), true);
    let marked = kill_at("steward-decide", dir.path(), T0, Duration::ZERO);
    assert_eq!(marked, "decide");
    // The child is dead: the held answer goes nowhere.
    daemon.hold.store(false, Ordering::SeqCst);
    daemon.released.store(true, Ordering::SeqCst);
    steward_after_kill(dir.path(), &daemon, 1).await;
}

// ---- refine (ADR 0015, phase 4) ----

fn refine_cfg() -> StewardConfig {
    StewardConfig {
        refine: RefineMode::Menu,
        refine_min_wakes: 0,
        ..StewardConfig::default()
    }
}

/// The child's Steward meets torin's daemon for the first time: it puts
/// and activates the empty baseline. It is killed after the put's intent
/// (`refine-put`), once lg stored the put (`refine-stored`), or once its
/// activate is out (`refine-activate`: the parent's daemon marks it and
/// answers only after the kill). With a human's overlay already active it
/// pauses (`refine-paused`) and is killed after the pass.
async fn refine_child(point: &str, p: &mut Proc, dir: &Path) {
    let steward = steward_with(p, dir, refine_cfg());
    let at = dir.to_path_buf();
    let stop_at = match point {
        "refine-put" => Some(StewardPoint::IntentWritten),
        "refine-stored" => Some(StewardPoint::OverlayStored),
        _ => None,
    };
    steward.set_probe(Some(Arc::new(move |pt, _: &EntityId| {
        if Some(pt) == stop_at {
            mark(&at, &format!("{pt:?}"));
            stop_here();
        }
    })));
    steward.pass().await;
    if point == "refine-paused" {
        let row = steward.row(&torin()).await.unwrap();
        mark(dir, &row.overlay.paused.map(|p| p.code).unwrap_or_default());
        stop_here();
    }
}

/// The fake `lg serve` of the refine cases, in the parent: no goals, an
/// lg-like overlay pointer, and a log of every request kind. With `hold`,
/// it marks the child's marker at the first `overlay_activate` and applies
/// and answers it only once `released`.
struct RefineDaemon {
    versions: Mutex<Vec<(u64, String)>>,
    active: Mutex<Option<u64>>,
    kinds: Mutex<Vec<String>>,
    hold: AtomicBool,
    released: AtomicBool,
    marker: PathBuf,
}

impl RefineDaemon {
    fn start(dir: &Path, hold: bool, preexisting: bool) -> Arc<Self> {
        let me = Arc::new(Self {
            versions: Mutex::new(Vec::new()),
            active: Mutex::new(None),
            kinds: Mutex::new(Vec::new()),
            hold: AtomicBool::new(hold),
            released: AtomicBool::new(false),
            marker: dir.join("marker"),
        });
        if preexisting {
            me.versions
                .lock()
                .unwrap()
                .push((1, r#"{"guardrails":"- an operator's own"}"#.into()));
            *me.active.lock().unwrap() = Some(1);
        }
        let listener = UnixListener::bind(dir.join("lg.sock")).unwrap();
        let d = Arc::clone(&me);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let d = Arc::clone(&d);
                std::thread::spawn(move || d.serve(stream));
            }
        });
        me
    }

    fn overlays(&self) -> String {
        let active = *self.active.lock().unwrap();
        let entries: Vec<serde_json::Value> = self
            .versions
            .lock()
            .unwrap()
            .iter()
            .map(|(v, _)| {
                serde_json::json!({ "version": v, "author": "operator:mud",
                    "created_at": "2026-11-25T00:00:00Z", "active": Some(*v) == active, "parts": [] })
            })
            .collect();
        serde_json::json!({ "kind": "overlays", "active": active, "entries": entries }).to_string()
    }

    fn serve(&self, stream: std::os::unix::net::UnixStream) {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let kind = v["kind"].as_str().unwrap().to_owned();
        self.kinds.lock().unwrap().push(kind.clone());
        let answer = match kind.as_str() {
            "approvals" => r#"{"kind":"off","message":"no approvals"}"#.to_owned(),
            "plans" => r#"{"kind":"plans","goals":[],"ideas":[],"answered":[]}"#.to_owned(),
            "notes" => r#"{"kind":"notes","notes":[]}"#.to_owned(),
            "overlays" => self.overlays(),
            "refine_propose" => r#"{"kind":"kept"}"#.to_owned(),
            "overlay_show" => {
                let want = v["version"].as_u64().unwrap();
                let overlay = self
                    .versions
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|(x, _)| *x == want)
                    .map(|(_, o)| o.clone())
                    .unwrap();
                serde_json::json!({ "kind": "overlay", "entry": { "version": want,
                    "author": "operator:mud", "created_at": "2026-11-25T00:00:00Z",
                    "active": false, "parts": [] }, "overlay": overlay })
                .to_string()
            }
            "overlay_put" => {
                let mut versions = self.versions.lock().unwrap();
                let version = versions.last().map_or(1, |(x, _)| x + 1);
                versions.push((version, v["overlay"].as_str().unwrap().to_owned()));
                drop(versions);
                serde_json::json!({ "kind": "overlay_stored", "version": version }).to_string()
            }
            "overlay_activate" => {
                if self.hold.swap(false, Ordering::SeqCst) {
                    mark(self.marker.parent().unwrap(), "activate");
                    while !self.released.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
                let version = v["version"].as_u64().unwrap();
                let previous = self.active.lock().unwrap().replace(version);
                serde_json::json!({ "kind": "overlay_active", "version": version, "previous": previous })
                    .to_string()
            }
            other => format!(r#"{{"kind":"error","message":"{other}"}}"#),
        };
        let mut stream = reader.into_inner();
        let _ = writeln!(stream, "{answer}");
    }

    fn count(&self, kind: &str) -> usize {
        self.kinds
            .lock()
            .unwrap()
            .iter()
            .filter(|k| *k == kind)
            .count()
    }
}

/// After the kill: a new process's Steward ends with exactly one put and
/// one activate of the baseline in all, and the baseline active.
async fn refine_after_kill(dir: &Path, daemon: &RefineDaemon) {
    let mut p = open(dir, T0 + 1_000, scripted());
    let steward = steward_with(&mut p, dir, refine_cfg());
    for _ in 0..4 {
        steward.pass().await;
        p.clock.advance(600_000);
    }
    assert_eq!(daemon.count("overlay_put"), 1, "exactly one put");
    assert_eq!(daemon.count("overlay_activate"), 1, "exactly one activate");
    let row = steward.row(&torin()).await.unwrap();
    assert!(row.intent.is_none(), "{row:?}");
    assert_eq!(row.overlay.baseline, Some(1));
    assert_eq!(row.overlay.pointer, Some(1));
    assert!(row.overlay.paused.is_none());
    assert_eq!(*daemon.active.lock().unwrap(), Some(1));
    assert_eq!(daemon.versions.lock().unwrap()[0].1, "{}");
    after_restart(dir, &p, &[]);
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_after_put_intent_before_overlay_put() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = RefineDaemon::start(dir.path(), false, false);
    let marked = kill_at("refine-put", dir.path(), T0, Duration::ZERO);
    assert_eq!(marked, "IntentWritten");
    assert_eq!(daemon.count("overlay_put"), 0);
    refine_after_kill(dir.path(), &daemon).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_after_overlay_put_before_activate_intent() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = RefineDaemon::start(dir.path(), false, false);
    let marked = kill_at("refine-stored", dir.path(), T0, Duration::ZERO);
    assert_eq!(marked, "OverlayStored");
    assert_eq!(daemon.count("overlay_put"), 1);
    assert_eq!(daemon.count("overlay_activate"), 0);
    // The restart finds the version by its content (`overlay_show`), and
    // shows it again before it activates it.
    refine_after_kill(dir.path(), &daemon).await;
    assert_eq!(daemon.count("overlay_show"), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_after_activate_sent_before_its_answer() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = RefineDaemon::start(dir.path(), true, false);
    let marked = kill_at("refine-activate", dir.path(), T0, Duration::ZERO);
    assert_eq!(marked, "activate");
    // The child is dead: lg's loop applies it now, and its answer goes
    // nowhere. The restart never sends it again: it adopts the move to
    // the baseline from lg's pointer.
    daemon.released.store(true, Ordering::SeqCst);
    while daemon.active.lock().unwrap().is_none() {
        std::thread::sleep(Duration::from_millis(5));
    }
    refine_after_kill(dir.path(), &daemon).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refine_pause_survives_a_kill() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = RefineDaemon::start(dir.path(), false, true);
    let marked = kill_at("refine-paused", dir.path(), T0, Duration::ZERO);
    assert_eq!(marked, "preexisting");
    let mut p = open(dir.path(), T0 + 1_000, scripted());
    let steward = steward_with(&mut p, dir.path(), refine_cfg());
    let row = steward.row(&torin()).await.unwrap();
    assert_eq!(
        row.overlay.paused.unwrap().code,
        "preexisting",
        "still paused"
    );
    for _ in 0..3 {
        steward.pass().await;
        p.clock.advance(200 * 3_600_000);
    }
    for kind in ["overlay_put", "overlay_activate", "refine_propose"] {
        assert_eq!(daemon.count(kind), 0, "no {kind} while paused");
    }
    assert_eq!(
        *daemon.active.lock().unwrap(),
        Some(1),
        "the operator's stands"
    );
    after_restart(dir.path(), &p, &[]);
}

#[test]
fn kill_child() {
    // The child's entry point; a no-op in the parent's own test run.
    let Ok(spec) = std::env::var(CHILD) else {
        return;
    };
    let (point, dir) = spec.split_once(':').unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(child(point, Path::new(dir)));
    // A child that returns never reached its point.
    std::process::exit(3);
}

// ---- the parent ----

fn spawn_child(point: &str, dir: &Path) -> Child {
    let log = std::fs::File::create(dir.join("child.log")).unwrap();
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "kill_child", "--nocapture", "--test-threads=1"])
        .env(CHILD, format!("{point}:{}", dir.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .unwrap()
}

/// Runs the child to `point` at game time `now`, SIGKILLs it there, and
/// returns what it marked.
fn kill_at(point: &str, dir: &Path, now: u64, after: Duration) -> String {
    std::fs::write(dir.join("clock"), now.to_string()).unwrap();
    let mut child = spawn_child(point, dir);
    let deadline = Instant::now() + Duration::from_secs(120);
    let marker = dir.join("marker");
    while !marker.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            let log = std::fs::read_to_string(dir.join("child.log")).unwrap_or_default();
            panic!("the child exited ({status}) before {point}: {log}");
        }
        assert!(Instant::now() < deadline, "the child never reached {point}");
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(after);
    // `Child::kill` is SIGKILL on unix: nothing runs after it.
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success(), "killed, not finished: {status}");
    std::fs::read_to_string(marker).unwrap()
}

/// What every case checks after the restart: the schema version, the boot
/// bumped past the child's, and no wake id of the child used again.
fn after_restart(dir: &Path, p: &Proc, parent_ids: &[String]) {
    let conn = rusqlite::Connection::open(dir.join("world.db")).unwrap();
    let v: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, 2);
    let child_boot: u64 = std::fs::read_to_string(dir.join("boot"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(p.game.boot() > child_boot, "the boot was bumped");
    let child_ids: BTreeSet<String> = std::fs::read_to_string(dir.join("wakes"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    for id in parent_ids {
        assert!(!child_ids.contains(id), "wake id {id} used again");
    }
}

fn booking_rows(dir: &Path) -> Vec<(u64, String)> {
    let conn = rusqlite::Connection::open(dir.join("world.db")).unwrap();
    let mut q = conn
        .prepare("SELECT id, note FROM sup_booking ORDER BY id")
        .unwrap();
    q.query_map([], |r| {
        Ok((u64::try_from(r.get::<_, i64>(0)?).unwrap(), r.get(1)?))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

/// torin's first macro tick played through in one process, never killed.
async fn control() -> (u64, String, usize) {
    let dir = tempfile::tempdir().unwrap();
    let mut p = open(dir.path(), T0, scripted());
    p.sup.fire_macro_tick(&torin());
    p.run_for(5_000, 500).await;
    (p.dice(), p.torin_room(), p.bounties())
}

#[tokio::test]
async fn kill_after_book_reply() {
    let dir = tempfile::tempdir().unwrap();
    let marked = kill_at("book", dir.path(), T0, Duration::ZERO);
    let (id, due) = marked.split_once(' ').unwrap();
    let (id, due): (u64, u64) = (id.parse().unwrap(), due.parse().unwrap());
    assert_eq!(booking_rows(dir.path()), [(id, "check the bin".to_owned())]);
    let mut p = open(dir.path(), T0 + 1_000, scripted());
    let pending = p.sup.wakes().pending(&torin());
    assert_eq!(pending.len(), 1);
    assert_eq!((pending[0].id, pending[0].due_ms), (id, due));
    let next = p
        .sup
        .wakes()
        .book(&torin(), T0 + 1_000, 5, "again")
        .unwrap();
    assert!(next.id > id, "ids never repeat: {} after {id}", next.id);
    let ids: Vec<String> = p.started().into_iter().map(|s| s.0).collect();
    after_restart(dir.path(), &p, &ids);
}

#[tokio::test]
async fn kill_before_any_act() {
    let dir = tempfile::tempdir().unwrap();
    let marked = kill_at("idle", dir.path(), T0, Duration::ZERO);
    let rows = booking_rows(dir.path());
    assert_eq!(rows.len(), 1, "the booking is still there");
    assert_eq!(
        marked,
        format!("1 true {}", rows[0].0),
        "one wake took the tick, as number 1, and the booking"
    );
    // Nothing was consumed: the tick is still owed and torin never moved.
    let mut p = open(dir.path(), T0 + 61_000, scripted());
    assert_eq!(p.macro_ticks(), 0, "nothing consumed");
    assert_eq!(p.bounties(), 0);
    p.run_for(130_000, 1_000).await;
    assert_eq!(p.bounties(), 1, "exactly one BOUNTY after the restart");
    assert_eq!(p.macro_ticks(), 1, "the owed tick came as number 1");
    assert_eq!(
        booking_rows(dir.path()),
        [],
        "the booking fired and was consumed"
    );
    assert_eq!(p.torin_room(), "smithy");
    let started = p.started();
    assert!(!started.is_empty(), "torin woke after the restart");
    let ids: Vec<String> = started.into_iter().map(|s| s.0).collect();
    after_restart(dir.path(), &p, &ids);
    let (dice, room, posts) = control().await;
    assert_eq!(
        (p.dice(), p.torin_room(), p.bounties()),
        (dice, room, posts)
    );
}

/// Kills torin after `n` effectful lines of his five, and checks the
/// resume.
async fn kill_after_lines(n: u32) {
    let dir = tempfile::tempdir().unwrap();
    let marked = kill_at(&format!("line{n}"), dir.path(), T0, Duration::ZERO);
    assert_eq!(marked, n.to_string());
    // The plan as the kill left it (a boot ends a finished one).
    let conn = rusqlite::Connection::open(dir.path().join("world.db")).unwrap();
    let (cursor, errand): (i64, i64) = conn
        .query_row(
            "SELECT cursor, errand FROM sup_plan WHERE entity = 'torin'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (cursor, errand),
        (i64::from(n), 1),
        "the cursor commits with its line"
    );
    let mut p = open(dir.path(), T0 + 1_000, scripted());
    assert_eq!(p.macro_ticks(), 1, "the tick was consumed with move e");
    let posted_before = usize::from(n >= 3);
    assert_eq!(p.bounties(), posted_before);
    p.run_for(5_000, 500).await;
    assert_eq!(p.bounties(), 1, "exactly one BOUNTY (killed after {n})");
    assert_eq!(p.torin_room(), "smithy", "torin ends in the smithy");
    assert_eq!(p.game.plan_of(&torin()), None);
    let started = p.started();
    if n == 5 {
        assert!(started.is_empty(), "nothing to resume: {started:?}");
    } else {
        assert_eq!(started.len(), 1, "{started:?}");
        assert_eq!(started[0].1, format!("resume plan at line {}", n + 1));
        assert_eq!(started[0].2, "plan");
    }
    let mut ids: Vec<String> = started.into_iter().map(|s| s.0).collect();
    // The next macro tick is number 2 and posts nothing.
    p.sup.fire_macro_tick(&torin());
    p.run_for(2_000, 500).await;
    assert_eq!(p.macro_ticks(), 2);
    assert_eq!(p.bounties(), 1);
    ids.extend(p.started().into_iter().map(|s| s.0));
    after_restart(dir.path(), &p, &ids);
}

#[tokio::test]
async fn kill_after_first_move() {
    kill_after_lines(1).await;
    // The dice resume as in a run that was never killed.
    let dir = tempfile::tempdir().unwrap();
    kill_at("line1", dir.path(), T0, Duration::ZERO);
    let mut p = open(dir.path(), T0 + 1_000, scripted());
    p.run_for(5_000, 500).await;
    let (dice, room, posts) = control().await;
    assert_eq!(
        (p.dice(), p.torin_room(), p.bounties()),
        (dice, room, posts)
    );
}

#[tokio::test]
async fn kill_after_post() {
    kill_after_lines(3).await;
}

#[tokio::test]
async fn kill_after_last_line() {
    kill_after_lines(5).await;
}

#[tokio::test]
async fn kill_during_retention() {
    let dir = tempfile::tempdir().unwrap();
    kill_at("retention", dir.path(), T0, Duration::from_millis(30));
    let p = open(dir.path(), T0 + 31 * 86_400_000, scripted());
    let conn = rusqlite::Connection::open(dir.path().join("world.db")).unwrap();
    let (lo, hi, n): (i64, i64, i64) = conn
        .query_row("SELECT MIN(seq), MAX(seq), COUNT(*) FROM events", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    let (lo, hi, n) = (
        u64::try_from(lo).unwrap(),
        u64::try_from(hi).unwrap(),
        u64::try_from(n).unwrap(),
    );
    assert_eq!(hi - lo + 1, n, "the table's seqs are contiguous");
    assert!(n >= 10_000, "the newest keep_min are intact: {n}");
    assert_eq!(
        hi,
        p.game.read(|w| w.last_seq().0),
        "the newest event is kept"
    );
    let archived = mud_world::archive::seqs(&dir.path().join("archive")).unwrap();
    assert!(!archived.is_empty(), "the kill came after some batches");
    assert!(
        archived.windows(2).all(|w| w[1] == w[0] + 1) && archived[0] == 1,
        "the archive is contiguous from 1, with no line twice"
    );
    let last = *archived.last().unwrap();
    assert!(
        lo <= last + 1,
        "no event fell between the archive and the table"
    );
    assert!(lo > 1, "the kill came after a trim committed");
    // A re-run after the kill finishes without a duplicate line.
    let done = p.game.retain(p.game.now_ms()).unwrap();
    assert!(done.trimmed > 0, "{done:?}");
    let archived = mud_world::archive::seqs(&dir.path().join("archive")).unwrap();
    assert!(archived.windows(2).all(|w| w[1] == w[0] + 1));
    let lo: i64 = conn
        .query_row("SELECT MIN(seq) FROM events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(u64::try_from(lo).unwrap(), archived.last().unwrap() + 1);
    after_restart(dir.path(), &p, &[]);
}

#[tokio::test]
async fn kill_with_local_spend_flushed() {
    let dir = tempfile::tempdir().unwrap();
    let marked = kill_at("spend", dir.path(), T0, Duration::ZERO);
    assert_eq!(marked, "Soft 330000");
    let p = open(dir.path(), T0 + 1_000, scripted());
    let (level, spend) = p.sup.budget_of(&torin()).unwrap();
    assert_eq!(
        format!("{level:?} {}", spend.local.tokens),
        marked,
        "the budget level is unchanged by the kill"
    );
    assert_eq!((spend.local.calls, spend.local.cost_usd_micros), (3, 1_234));
    after_restart(dir.path(), &p, &[]);
}

/// What torin said aloud, from the stored events.
fn ayes(p: &Proc) -> usize {
    p.game.events(&torin(), mud_world::Seq(0), 1_000)["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["actor"]["id"] == "torin" && e["untrusted_text"] == "aye")
        .count()
}

/// A tell killed at `point` is answered once after the restart, within
/// the replay window, and a third process replays nothing.
async fn kill_in_a_tell(point: &str, replayed: bool) {
    let dir = tempfile::tempdir().unwrap();
    let marked = kill_at(point, dir.path(), T0, Duration::ZERO);
    let mut p = open(dir.path(), T0 + 1_000, Arc::new(Answer));
    let queued = p.sup.boot_report().iter().any(|e| {
        matches!(&**e, ObserverEvent::Route(mud_supervisor::router::RouteNote::Queued { entity, .. }) if *entity == torin())
    });
    assert_eq!(queued, replayed, "{point}: {marked}");
    p.run_for(5_000, 500).await;
    assert_eq!(ayes(&p), 1, "{point}: answered once");
    let started = p.started();
    assert_eq!(started.len(), usize::from(replayed), "{point}: {started:?}");
    if replayed {
        assert!(started[0].1.starts_with("replayed tell"), "{started:?}");
    }
    let ids: Vec<String> = started.into_iter().map(|s| s.0).collect();
    after_restart(dir.path(), &p, &ids);
    drop(p);
    let mut p = open(dir.path(), T0 + 8_000, Arc::new(Answer));
    p.run_for(5_000, 500).await;
    assert!(
        p.started().is_empty(),
        "{point}: a third boot replays nothing"
    );
    assert_eq!(ayes(&p), 1, "{point}");
}

#[tokio::test]
async fn kill_before_a_tell_is_taken() {
    kill_in_a_tell("tell-pending", true).await;
}

#[tokio::test]
async fn kill_as_a_tell_wake_opens() {
    kill_in_a_tell("tell-open", true).await;
}

#[tokio::test]
async fn kill_after_a_tell_is_answered() {
    kill_in_a_tell("tell-answered", false).await;
}

#[test]
fn a_child_that_cannot_reach_its_point_is_reported() {
    // The marker protocol itself: a missing point exits, never hangs.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("clock"), T0.to_string()).unwrap();
    let mut child = spawn_child("line9", dir.path());
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(3), "torin has five lines, not nine");
}
