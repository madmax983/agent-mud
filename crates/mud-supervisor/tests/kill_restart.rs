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
#![cfg(unix)]
#![expect(clippy::unwrap_used, reason = "test helpers and the child fail loudly")]

use std::collections::{BTreeSet, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use mud_supervisor::gate::Gate;
use mud_supervisor::mind::{BoxFuture, Mind, MindError, MindReply, MudTools, Usage, WakeContext};
use mud_supervisor::runtime::{ObserverEvent, Supervisor, SupervisorConfig};
use mud_supervisor::scripted::{FallbackMind, ScriptedMind};
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
    let restored = game.take_restored().unwrap_or_default();
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
    assert_eq!(v, 1);
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
