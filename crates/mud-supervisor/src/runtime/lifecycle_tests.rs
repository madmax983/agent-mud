//! The Turing wake lifecycle (cause 7 of the downed plan): a wake's bound
//! leaves out the time its PC spends typing, the next wake carries only
//! what the mind did not read, an interrupted line does not stack a second
//! wake on a running one, and spend that comes in after a wake's line
//! still reaches the feed. The shape of the owner's log: every kestrel
//! wake ended "FAILED · 0 tok · timed out" two minutes after it started.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mud_world::{ActorRef, EntityId, Event, EventKind, Game, ManualClock, Seq, World, WorldPack};
use tokio::sync::Notify;

use super::*;
use crate::mind::{BoxFuture, InFlight, MindError};

fn kestrel() -> EntityId {
    EntityId::from("pc_kestrel")
}

fn torin() -> EntityId {
    EntityId::from("torin")
}

struct Rig {
    sup: Arc<Supervisor>,
    game: Arc<Game>,
    clock: Arc<ManualClock>,
}

/// A supervisor whose only minds are `minds`, with a gate scaled by
/// `typing`.
fn rig(cfg: SupervisorConfig, typing: f64, minds: HashMap<EntityId, Arc<dyn Mind>>) -> Rig {
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let clock = Arc::new(ManualClock::new(20 * 3_600_000));
    let game = Game::new(World::new(Arc::clone(&pack), 5), None, clock.clone());
    let typist = Gate::new(Arc::clone(&game), 5, typing);
    let fallback = Arc::new(FallbackMind::from_cast(&pack.cast));
    let sup = Supervisor::new(Arc::clone(&game), typist, minds, fallback, cfg);
    Rig { sup, game, clock }
}

fn quiet() -> SupervisorConfig {
    SupervisorConfig {
        first_macro_tick_secs: [3_600, 3_600],
        ..SupervisorConfig::default()
    }
}

/// Queues an Immediate wake for `who` and runs the step that takes it.
fn wake_now(r: &Rig, who: &EntityId) -> Vec<JoinHandle<()>> {
    lock(&r.sup.router).force(who, Priority::Immediate, "test", r.game.now_ms());
    r.sup.step()
}

fn finished(obs: &mut broadcast::Receiver<Arc<ObserverEvent>>) -> Vec<(bool, Option<String>)> {
    std::iter::from_fn(|| obs.try_recv().ok())
        .filter_map(|e| match &*e {
            ObserverEvent::WakeFinished { ok, note, .. } => Some((*ok, note.clone())),
            _ => None,
        })
        .collect()
}

// ---- the bound leaves out typing ----

/// Runs one line through the gate, then answers.
#[derive(Debug)]
struct Types(&'static str);

impl Mind for Types {
    fn kind(&self) -> &'static str {
        "types"
    }

    fn wake(
        &self,
        _ctx: WakeContext,
        tools: MudTools,
    ) -> BoxFuture<'_, Result<MindReply, MindError>> {
        Box::pin(async move {
            let r = tools.command(self.0).await;
            Ok(MindReply {
                text: Some(format!("{:?}", r.ok)),
                ..MindReply::default()
            })
        })
    }
}

/// Thinks for `ms` without typing.
#[derive(Debug)]
struct Thinks(u64);

impl Mind for Thinks {
    fn kind(&self) -> &'static str {
        "thinks"
    }

    fn wake(
        &self,
        _ctx: WakeContext,
        _tools: MudTools,
    ) -> BoxFuture<'_, Result<MindReply, MindError>> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(self.0)).await;
            Ok(MindReply::default())
        })
    }
}

const NOTICE: &str = "post still need a second for the ossuary warden. warden hits hard, \
                      don't come alone. four ore chunks + bone charm, even split. kestrel.";

#[tokio::test(start_paused = true)]
async fn a_turing_wake_with_a_45_s_board_post_hold_ends_ok() {
    // The gate holds the post about 46 s; the Turing bound is 30 s.
    let probe = rig(quiet(), 1.0, HashMap::new());
    let raw = probe.sup.gate().hold_ms(&kestrel(), NOTICE, 0);
    assert!(raw > 0, "a notice is typed");
    #[expect(clippy::cast_precision_loss, reason = "a few seconds of ms")]
    let scale = 46_000.0 / raw as f64;
    let cfg = SupervisorConfig {
        wake_timeout_ms: 120_000,
        wake_timeout_ms_by_tier: TierTimeouts {
            turing: Some(30_000),
            ..TierTimeouts::default()
        },
        ..quiet()
    };
    let mind: Arc<dyn Mind> = Arc::new(Types(NOTICE));
    let r = rig(cfg, scale, HashMap::from([(kestrel(), mind)]));
    r.game.bring_online(&kestrel());
    for d in ["w", "n"] {
        assert!(r.game.execute(&kestrel(), d).ok, "{d}");
    }
    let hold = r.sup.gate().hold_ms(&kestrel(), NOTICE, 0);
    assert!((45_000..=47_000).contains(&hold), "{hold}");
    let mut obs = r.sup.subscribe();
    let started = tokio::time::Instant::now();
    for h in wake_now(&r, &kestrel()) {
        h.await.unwrap();
    }
    assert!(started.elapsed() >= Duration::from_millis(hold));
    assert_eq!(
        finished(&mut obs),
        [(true, Some("true".to_owned()))],
        "the wake outlived its 30 s bound by typing, not by hanging"
    );
    let board = r.game.read(|w| {
        w.state().rooms[&mud_world::RoomId::from("notice_market")]
            .board
            .len()
    });
    assert_eq!(board, 1, "the notice went up");
}

#[tokio::test(start_paused = true)]
async fn a_mind_that_hangs_still_times_out_on_its_tiers_bound() {
    let cfg = SupervisorConfig {
        wake_timeout_ms: 120_000,
        wake_timeout_ms_by_tier: TierTimeouts {
            turing: Some(30_000),
            ..TierTimeouts::default()
        },
        ..quiet()
    };
    let minds: HashMap<EntityId, Arc<dyn Mind>> = HashMap::from([
        (kestrel(), Arc::new(Thinks(31_000)) as Arc<dyn Mind>),
        (torin(), Arc::new(Thinks(31_000)) as Arc<dyn Mind>),
    ]);
    let r = rig(cfg, 1.0, minds);
    r.game.bring_online(&kestrel());
    let mut obs = r.sup.subscribe();
    let started = tokio::time::Instant::now();
    for h in wake_now(&r, &kestrel()) {
        h.await.unwrap();
    }
    let took = started.elapsed();
    assert!(
        (Duration::from_secs(30)..Duration::from_secs(31)).contains(&took),
        "{took:?}"
    );
    assert_eq!(finished(&mut obs), [(false, Some("timed out".to_owned()))]);
    for h in wake_now(&r, &torin()) {
        h.await.unwrap();
    }
    assert_eq!(
        finished(&mut obs),
        [(true, None)],
        "a persistent agent keeps the default 120 s bound"
    );
}

// ---- the next wake carries only what the mind did not read ----

/// How the first wake of a [`Reader`] ends, once it has read its events.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum End {
    /// It answers.
    #[default]
    Answers,
    /// The mind fails (a socket error, a wait for an approval).
    Fails,
    /// littleguy's `no_answer`: a provider error or a refusal after the
    /// model already called `mud_events`.
    NoAnswer,
}

/// Torin's first wake waits for the test, reads `mud_events`, waits again,
/// and ends as `end` says; every wake records the seqs it carried.
#[derive(Debug, Default)]
struct Reader {
    started: Notify,
    read: Notify,
    go: Notify,
    done: Notify,
    end: End,
    wakes: Mutex<Vec<(String, Vec<Seq>)>>,
}

impl Reader {
    fn ending(end: End) -> Self {
        Self {
            end,
            ..Self::default()
        }
    }
}

impl Mind for Reader {
    fn kind(&self) -> &'static str {
        "reader"
    }

    fn wake(
        &self,
        ctx: WakeContext,
        tools: MudTools,
    ) -> BoxFuture<'_, Result<MindReply, MindError>> {
        Box::pin(async move {
            let first = {
                let mut w = self.wakes.lock().unwrap();
                w.push((
                    ctx.reason.clone(),
                    ctx.events.iter().map(|e| e.seq).collect(),
                ));
                w.len() == 1
            };
            if first {
                self.started.notify_one();
                self.read.notified().await;
                let _ = tools
                    .call("mud_events", &serde_json::json!({ "since_seq": 0 }))
                    .await;
                self.done.notify_one();
                self.go.notified().await;
                match self.end {
                    End::Answers => {}
                    End::Fails => return Err(MindError::Failed("the socket closed".into())),
                    End::NoAnswer => {
                        return Ok(MindReply {
                            text: Some("no answer: provider error".into()),
                            unanswered: true,
                            ..MindReply::default()
                        });
                    }
                }
            }
            Ok(MindReply::default())
        })
    }
}

/// The seq of the newest event torin perceived.
fn last_for_torin(game: &Game) -> Seq {
    game.read(|w| {
        w.events_since(&torin(), Seq(0), usize::MAX)
            .last()
            .map_or(Seq(0), |e| e.seq)
    })
}

#[tokio::test]
async fn a_tell_the_mind_read_is_not_carried_again_but_an_unread_one_is() {
    let reader = Arc::new(Reader::default());
    let r = rig(
        quiet(),
        0.0,
        HashMap::from([(torin(), Arc::clone(&reader) as Arc<dyn Mind>)]),
    );
    let mut rx = r.game.subscribe();
    let ada = r.game.login("ada").unwrap();
    let feed = |rx: &mut broadcast::Receiver<Arc<Event>>| {
        while let Ok(ev) = rx.try_recv() {
            r.sup.observe(&ev);
        }
    };
    feed(&mut rx);
    let one = wake_now(&r, &torin());
    reader.started.notified().await;
    // While the wake runs: a tell it will read, then one it will not.
    assert!(r.game.execute(&ada, "tell torin ore at the smithy?").ok);
    let read_tell = last_for_torin(&r.game);
    feed(&mut rx);
    reader.read.notify_one();
    reader.done.notified().await;
    assert_eq!(r.game.read_mark(&torin()), read_tell);
    assert!(r.game.execute(&ada, "tell torin and the warden?").ok);
    let unread_tell = last_for_torin(&r.game);
    feed(&mut rx);
    assert!(lock(&r.sup.router).is_busy(&torin()));
    reader.go.notify_one();
    for h in one {
        h.await.unwrap();
    }
    assert_eq!(
        r.sup.seen_seq(&torin()),
        Some(read_tell),
        "what it read is seen, for a boot replay too"
    );
    // Past the Immediate jitter of the unread tell.
    r.clock.advance(1_000);
    for h in r.sup.step() {
        h.await.unwrap();
    }
    let wakes = reader.wakes.lock().unwrap().clone();
    assert_eq!(wakes.len(), 2, "{wakes:?}");
    assert_eq!(
        wakes[1],
        ("tell by ada".to_owned(), vec![unread_tell]),
        "only the tell it never read"
    );
}

#[tokio::test]
async fn a_wake_of_only_read_events_is_dropped() {
    let reader = Arc::new(Reader::default());
    let r = rig(
        quiet(),
        0.0,
        HashMap::from([(torin(), Arc::clone(&reader) as Arc<dyn Mind>)]),
    );
    let mut rx = r.game.subscribe();
    let ada = r.game.login("ada").unwrap();
    while let Ok(ev) = rx.try_recv() {
        r.sup.observe(&ev);
    }
    let one = wake_now(&r, &torin());
    reader.started.notified().await;
    assert!(r.game.execute(&ada, "tell torin ore at the smithy?").ok);
    while let Ok(ev) = rx.try_recv() {
        r.sup.observe(&ev);
    }
    assert!(
        lock(&r.sup.router)
            .pending()
            .iter()
            .any(|(e, ..)| e == &torin())
    );
    reader.read.notify_one();
    reader.done.notified().await;
    reader.go.notify_one();
    for h in one {
        h.await.unwrap();
    }
    assert!(
        lock(&r.sup.router).is_idle(&torin()),
        "the tell was read and answered in the wake: nothing is owed"
    );
    r.clock.advance(1_000);
    assert!(r.sup.step().is_empty());
    assert_eq!(reader.wakes.lock().unwrap().len(), 1);
}

/// Runs torin's first wake to its end around one tell: the tell comes
/// while the wake runs, the mind reads it with `mud_events`, and the wake
/// ends as the mind's [`End`] says. Returns the rig and the tell's seq.
async fn a_tell_read_in_a_wake(mind: Arc<Reader>) -> (Rig, Seq) {
    let r = rig(
        quiet(),
        0.0,
        HashMap::from([(torin(), Arc::clone(&mind) as Arc<dyn Mind>)]),
    );
    let mut rx = r.game.subscribe();
    let ada = r.game.login("ada").unwrap();
    while let Ok(ev) = rx.try_recv() {
        r.sup.observe(&ev);
    }
    let one = wake_now(&r, &torin());
    mind.started.notified().await;
    assert!(r.game.execute(&ada, "tell torin meet at the smithy").ok);
    let tell = last_for_torin(&r.game);
    while let Ok(ev) = rx.try_recv() {
        r.sup.observe(&ev);
    }
    mind.read.notify_one();
    mind.done.notified().await;
    assert_eq!(r.game.read_mark(&torin()), tell, "the mind read the tell");
    mind.go.notify_one();
    for h in one {
        h.await.unwrap();
    }
    (r, tell)
}

#[tokio::test]
async fn a_tell_read_by_a_wake_that_failed_still_wakes_the_mind() {
    // Reading an event is not answering it: a wake that failed, or ended
    // with littleguy's `no_answer`, after `mud_events` owes the tell still.
    for end in [End::Fails, End::NoAnswer] {
        let mind = Arc::new(Reader::ending(end));
        let (r, tell) = a_tell_read_in_a_wake(Arc::clone(&mind)).await;
        assert!(
            r.sup.seen_seq(&torin()) < Some(tell),
            "{end:?}: the floor stays below the tell"
        );
        r.clock.advance(1_000);
        for h in r.sup.step() {
            h.await.unwrap();
        }
        let wakes = mind.wakes.lock().unwrap().clone();
        assert_eq!(wakes.len(), 2, "{end:?}: {wakes:?}");
        assert_eq!(
            wakes[1],
            ("tell by ada".to_owned(), vec![tell]),
            "{end:?}: the tell wakes it"
        );
    }
}

/// The signals between a [`ReadsLate`] op and the test.
#[derive(Debug, Default)]
struct Signals {
    started: Notify,
    read: Notify,
    go: Notify,
    done: Notify,
}

/// A littleguy-like mind whose first op outlives its wake: the op reads
/// its events after the wake timed out, waits for the test, and answers
/// late (or does not), as `lg serve` does after a timeout.
#[derive(Debug, Default)]
struct ReadsLate {
    in_flight: InFlight,
    answers: bool,
    signals: Arc<Signals>,
    wakes: Mutex<Vec<Vec<Seq>>>,
}

impl Mind for ReadsLate {
    fn kind(&self) -> &'static str {
        "reads-late"
    }

    fn wake(
        &self,
        ctx: WakeContext,
        tools: MudTools,
    ) -> BoxFuture<'_, Result<MindReply, MindError>> {
        let first = {
            let mut w = self.wakes.lock().unwrap();
            w.push(ctx.events.iter().map(|e| e.seq).collect());
            w.len() == 1
        };
        if !first {
            return Box::pin(async { Ok(MindReply::default()) });
        }
        let guard = self.in_flight.start(&ctx.entity);
        let (signals, answers) = (Arc::clone(&self.signals), self.answers);
        // The op, on a task of its own: the wake's timeout drops only the
        // wake's future.
        tokio::spawn(async move {
            signals.started.notify_one();
            signals.read.notified().await;
            let _ = tools
                .call("mud_events", &serde_json::json!({ "since_seq": 0 }))
                .await;
            signals.done.notify_one();
            signals.go.notified().await;
            if answers {
                tools.usage_sink().answered_late(&ctx.entity);
            }
            drop(guard);
        });
        Box::pin(std::future::pending())
    }

    fn settled(&self, entity: &EntityId) -> BoxFuture<'_, ()> {
        self.in_flight.settled(entity)
    }
}

#[tokio::test(start_paused = true)]
async fn a_timed_out_wake_drops_what_it_read_only_if_its_op_answered_late() {
    for answers in [true, false] {
        let cfg = SupervisorConfig {
            wake_timeout_ms: 50,
            ..quiet()
        };
        let mind = Arc::new(ReadsLate {
            answers,
            ..ReadsLate::default()
        });
        let r = rig(
            cfg,
            0.0,
            HashMap::from([(torin(), Arc::clone(&mind) as Arc<dyn Mind>)]),
        );
        let mut rx = r.game.subscribe();
        let ada = r.game.login("ada").unwrap();
        while let Ok(ev) = rx.try_recv() {
            r.sup.observe(&ev);
        }
        let one = wake_now(&r, &torin());
        let signals = Arc::clone(&mind.signals);
        signals.started.notified().await;
        // Past the bound: the wake times out, its op runs on.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(lock(&r.sup.router).is_busy(&torin()), "{answers}");
        assert!(r.game.execute(&ada, "tell torin meet at the smithy").ok);
        let tell = last_for_torin(&r.game);
        while let Ok(ev) = rx.try_recv() {
            r.sup.observe(&ev);
        }
        signals.read.notify_one();
        signals.done.notified().await;
        signals.go.notify_one();
        for h in one {
            h.await.unwrap();
        }
        r.clock.advance(1_000);
        for h in r.sup.step() {
            h.await.unwrap();
        }
        let wakes = mind.wakes.lock().unwrap().clone();
        if answers {
            assert_eq!(wakes.len(), 1, "answered late: nothing owed: {wakes:?}");
            assert_eq!(r.sup.seen_seq(&torin()), Some(tell));
        } else {
            assert_eq!(wakes.len(), 2, "no late answer: {wakes:?}");
            assert_eq!(wakes[1], vec![tell], "the tell still wakes it");
        }
    }
}

#[test]
fn a_forced_cause_survives_the_drop_of_read_events() {
    // A macro tick that merged into a wake of read events still goes out,
    // as the tick it is.
    let r = rig(quiet(), 0.0, HashMap::new());
    let ada = r.game.login("ada").unwrap();
    let mut router = Router::new(RouterConfig::default());
    let now = r.game.now_ms();
    assert!(r.game.execute(&ada, "tell torin hello").ok);
    let tell = r.game.read(World::last_seq);
    let ev = r.game.read(|w| w.logged().last().cloned()).unwrap();
    let rt = HashMap::new();
    r.game.read(|w| router.observe(&ev, w, &rt, now));
    // Torin is away from any player: the tick queues.
    let tick = r.game.read(|w| router.macro_tick(&torin(), w, now, false));
    assert!(matches!(tick, RouteNote::Queued { .. }), "{tick:?}");
    router.finish_seen(&torin(), now, tell);
    let left = router.due(now + 60_000);
    assert_eq!(left.len(), 1);
    assert_eq!(
        left[0].priority,
        Priority::MacroTick,
        "the tell's urgency went with it"
    );
    assert_eq!(left[0].reason, "macro-tick");
    assert!(left[0].macro_tick);
    assert_eq!(left[0].seqs, Vec::<Seq>::new());
}

// ---- an interrupted line does not stack a wake on a running one ----

#[tokio::test(start_paused = true)]
async fn combat_during_a_running_wake_interrupts_the_line_without_a_second_wake() {
    let mind: Arc<dyn Mind> = Arc::new(Types("say this is a long sentence to type out slowly"));
    let r = rig(quiet(), 1.0, HashMap::from([(kestrel(), mind)]));
    r.game.bring_online(&kestrel());
    let mut obs = r.sup.subscribe();
    let wake = wake_now(&r, &kestrel());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(r.sup.gate().is_holding(&kestrel()));
    let mut ev = r.game.read(|w| w.logged().last().cloned()).unwrap();
    ev.seq = Seq(ev.seq.0 + 1);
    ev.kind = EventKind::Combat;
    ev.actor = Some(ActorRef {
        id: EntityId::from("giant_rat_1"),
        name: "giant rat".into(),
    });
    ev.verb = Some("attack".into());
    ev.player_actor = false;
    ev.human_actor = false;
    ev.recipients = vec![kestrel()];
    r.sup.observe(&ev);
    assert!(!r.sup.gate().is_holding(&kestrel()), "the line was dropped");
    for h in wake {
        h.await.unwrap();
    }
    let seen: Vec<Arc<ObserverEvent>> = std::iter::from_fn(|| obs.try_recv().ok()).collect();
    assert!(
        seen.iter()
            .any(|e| matches!(&**e, ObserverEvent::Interrupted { entity } if entity == &kestrel()))
    );
    assert!(
        seen.iter().any(|e| matches!(
            &**e,
            ObserverEvent::WakeFinished { note: Some(n), .. } if n == "false"
        )),
        "the running wake read INTERRUPTED and ended"
    );
    let pending = lock(&r.sup.router).pending();
    assert!(
        pending
            .iter()
            .all(|(e, p, _)| e != &kestrel() || *p < Priority::Immediate),
        "no forced Immediate wake behind the running one: {pending:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn combat_interrupting_a_line_outside_a_wake_still_wakes_the_pc() {
    let r = rig(quiet(), 1.0, HashMap::new());
    r.game.bring_online(&kestrel());
    let gate = Arc::clone(r.sup.gate());
    let typing = tokio::spawn(async move {
        gate.run(&kestrel(), "say typed over the MCP between wakes")
            .await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut ev = r.game.read(|w| w.logged().last().cloned()).unwrap();
    ev.seq = Seq(ev.seq.0 + 1);
    ev.kind = EventKind::Combat;
    ev.actor = None;
    ev.player_actor = false;
    ev.recipients = vec![kestrel()];
    r.sup.observe(&ev);
    assert!(!typing.await.unwrap().ok);
    let pending = lock(&r.sup.router).pending();
    assert!(
        pending
            .iter()
            .any(|(e, p, _)| e == &kestrel() && *p == Priority::Immediate),
        "{pending:?}"
    );
}

// ---- late spend reaches the feed ----

/// A littleguy-like mind: its op outlives the wake and charges its spend
/// to the sink once read, before it settles.
#[derive(Debug, Default)]
struct OutlivesItsWake {
    in_flight: InFlight,
}

impl Mind for OutlivesItsWake {
    fn kind(&self) -> &'static str {
        "outlives"
    }

    fn wake(
        &self,
        ctx: WakeContext,
        tools: MudTools,
    ) -> BoxFuture<'_, Result<MindReply, MindError>> {
        let guard = self.in_flight.start(&ctx.entity);
        Box::pin(async move {
            let (sink, who) = (Arc::clone(tools.usage_sink()), ctx.entity.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                sink.add(
                    &who,
                    Usage {
                        calls: 3,
                        input_tokens: 41_000,
                        output_tokens: 900,
                        cost_usd_micros: 0,
                    },
                );
                drop(guard);
            });
            std::future::pending::<()>().await;
            Ok(MindReply::default())
        })
    }

    fn settled(&self, entity: &EntityId) -> BoxFuture<'_, ()> {
        self.in_flight.settled(entity)
    }
}

#[tokio::test]
async fn a_timed_out_wake_shows_its_late_spend_in_the_feed() {
    let cfg = SupervisorConfig {
        wake_timeout_ms: 50,
        ..quiet()
    };
    let mind: Arc<dyn Mind> = Arc::new(OutlivesItsWake::default());
    let r = rig(cfg, 0.0, HashMap::from([(torin(), mind)]));
    let mut obs = r.sup.subscribe();
    for h in wake_now(&r, &torin()) {
        h.await.unwrap();
    }
    let seen: Vec<Arc<ObserverEvent>> = std::iter::from_fn(|| obs.try_recv().ok()).collect();
    let done = seen
        .iter()
        .position(|e| matches!(&**e, ObserverEvent::WakeFinished { ok: false, usage, .. } if usage.is_zero()))
        .expect("the wake's own line: timed out, nothing yet");
    let late = seen
        .iter()
        .position(|e| {
            matches!(
                &**e,
                ObserverEvent::LateSpend { entity, usage, budget }
                    if entity == &torin() && usage.tokens() == 41_900 && *budget == BudgetLevel::Normal
            )
        })
        .expect("the op's spend, when it was read");
    assert!(done < late);
    let (_, day) = r.sup.budget_of(&torin()).unwrap();
    assert_eq!(day.local.tokens, 41_900, "charged once");
    drop(r.sup.step());
    assert!(
        std::iter::from_fn(|| obs.try_recv().ok())
            .all(|e| !matches!(&*e, ObserverEvent::LateSpend { .. })),
        "nothing is charged, or shown, twice"
    );
}
