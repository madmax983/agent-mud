//! Downed, not limbo: an agent out cold is not woken, and the wake it gets
//! when it comes to carries everything that waited for it, across a
//! restart too. The shape of the owner's log: Marta and Vich at -3 HP in
//! the wine vault, with wakes booked 15 and 20 minutes out.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mud_world::pack::RecoveryRules;
use mud_world::store::Store;
use mud_world::{EntityId, Game, ManualClock, RoomId, Seq, World, WorldPack};
use serde_json::Value;

use super::persist_tests::{Disk, T0, marta, quiet, start};
use super::*;
use crate::admit::CAME_TO;
use crate::mind::{BoxFuture, MindError};

const MINUTE: u64 = 60_000;

fn vich() -> EntityId {
    EntityId::from("vich")
}

/// One wake as a mind saw it.
#[derive(Debug, Clone)]
struct Seen {
    at_ms: u64,
    reason: String,
    priority: Priority,
    seqs: Vec<Seq>,
    booked: Vec<String>,
    you: Value,
    message: String,
}

/// Remembers every wake and does nothing.
#[derive(Debug, Default)]
struct Recorder {
    seen: Mutex<Vec<Seen>>,
}

impl Recorder {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

impl Mind for Recorder {
    fn kind(&self) -> &'static str {
        "recorder"
    }

    fn wake(
        &self,
        ctx: WakeContext,
        _tools: MudTools,
    ) -> BoxFuture<'_, Result<MindReply, MindError>> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(Seen {
                at_ms: ctx.now_ms,
                reason: ctx.reason.clone(),
                priority: ctx.priority,
                seqs: ctx.events.iter().map(|e| e.seq).collect(),
                booked: ctx.booked.iter().map(|b| b.note.clone()).collect(),
                you: ctx.you.clone(),
                message: crate::mind::wake_message(&ctx),
            });
            Ok(MindReply::default())
        })
    }
}

/// Lays each of `who` out cold at `hp` in the wine vault.
fn downed(pack: &Arc<WorldPack>, who: &[(&EntityId, i32)]) -> mud_world::state::WorldState {
    let mut s = World::new(Arc::clone(pack), 5).snapshot();
    for (id, hp) in who {
        let e = s.entities.get_mut(*id).unwrap();
        e.room = RoomId::from("wine_vault");
        e.pools.hp = *hp;
        e.unconscious = true;
    }
    s
}

/// Writes `state` as the world on `disk`, as a world saved before a boot.
fn seed(disk: &Disk, state: &mud_world::state::WorldState) {
    let mut store = Store::open(&disk.db(), 5, T0).unwrap();
    store
        .save(&disk.pack.world.name, state, &[], &[], T0)
        .unwrap();
}

fn minds(of: &[(&EntityId, &Arc<Recorder>)]) -> HashMap<EntityId, Arc<dyn Mind>> {
    of.iter()
        .map(|(id, m)| ((*id).clone(), Arc::clone(m) as Arc<dyn Mind>))
        .collect()
}

/// When `who` came to, from the world's log.
fn came_to_at(game: &Game, who: &EntityId) -> Option<(u64, Seq)> {
    game.read(|w| {
        w.logged()
            .find(|e| e.tags.iter().any(|t| t == mud_world::CAME_TO_TAG) && e.targets(who))
            .map(|e| (e.at_ms, e.seq))
    })
}

#[tokio::test]
async fn the_logs_marta_and_vich_come_to_within_minutes_and_are_woken_once() {
    let disk = Disk::new();
    seed(&disk, &downed(&disk.pack, &[(&marta(), -3), (&vich(), -3)]));
    let (m, v) = (Arc::new(Recorder::default()), Arc::new(Recorder::default()));
    let mut run = start(&disk, T0, quiet(), minds(&[(&marta(), &m), (&vich(), &v)]));
    run.sup
        .wakes()
        .book(&marta(), T0, 15, "see whether I came to")
        .unwrap();
    run.sup
        .wakes()
        .book(&vich(), T0, 20, "crawl east once I am up")
        .unwrap();
    // A human sends a tell to each while they lie there, and a macro tick
    // falls due for Marta: all of it waits.
    let ada = run.game.login("ada").unwrap();
    let tell = run
        .game
        .execute(&ada, "tell marta are you alive down there?");
    assert!(tell.ok, "{tell:?}");
    let tell_seq = tell.events[0];
    run.game.execute(&ada, "tell vich the rat is still there");
    run.sup.fire_macro_tick(&marta());
    run.run_for(4 * MINUTE, 250).await;
    for (who, mind) in [(marta(), &m), (vich(), &v)] {
        let (up_at, up_seq) =
            came_to_at(&run.game, &who).unwrap_or_else(|| panic!("{who} never came to"));
        assert!(up_at <= T0 + 4 * MINUTE, "{who} came to at {}", up_at - T0);
        let seen = mind.seen();
        assert!(!seen.is_empty(), "{who} was never woken");
        for s in &seen {
            assert!(
                s.at_ms >= up_at,
                "{who} was woken out cold at {}",
                s.at_ms - T0
            );
            assert_eq!(s.you["unconscious"], false, "{who}: {:?}", s.you);
        }
        let first = &seen[0];
        assert_eq!(first.reason, CAME_TO, "{who}: {first:?}");
        assert_eq!(first.priority, Priority::Immediate);
        assert!(
            first.seqs.contains(&up_seq),
            "{who}: the come-to line rides along"
        );
        assert!(first.message.contains("You come to"), "{}", first.message);
        assert!(first.message.contains("\nyou: {"), "{}", first.message);
        assert!(first.booked.is_empty(), "{who}: the booking is not due yet");
        assert!(
            first.at_ms - up_at <= 1_000,
            "{who} woken {} ms after coming to",
            first.at_ms - up_at
        );
    }
    let first = &m.seen()[0];
    assert!(
        first.seqs.contains(&tell_seq),
        "the held tell went out with it"
    );
    assert!(
        first.message.contains("are you alive down there?"),
        "{}",
        first.message
    );
    // The bookings still come, at their own times, once each.
    run.run_for(17 * MINUTE, 1_000).await;
    let booked = |r: &Recorder| -> Vec<Vec<String>> {
        r.seen()
            .into_iter()
            .filter(|s| !s.booked.is_empty())
            .map(|s| s.booked)
            .collect()
    };
    assert_eq!(booked(&m), [vec!["see whether I came to".to_owned()]]);
    assert_eq!(booked(&v), [vec!["crawl east once I am up".to_owned()]]);
}

#[tokio::test]
async fn zero_wakes_while_she_is_out_cold_whatever_happens_around_her() {
    // The 7:52-7:54:53 stretch of the log: seven wakes while unconscious,
    // about 190k tokens. Now there are none until she comes to.
    let mut pack = WorldPack::crypt_v01().unwrap();
    pack.recovery = RecoveryRules::default();
    let disk = Disk::with(pack);
    seed(&disk, &downed(&disk.pack, &[(&marta(), -8)]));
    let m = Arc::new(Recorder::default());
    let mut run = start(&disk, T0, quiet(), minds(&[(&marta(), &m)]));
    run.sup
        .wakes()
        .book(&marta(), T0, 1, "check myself")
        .unwrap();
    let ada = run.game.login("ada").unwrap();
    for minute in 0..7 {
        run.game
            .execute(&ada, &format!("tell marta minute {minute}, still there?"));
        run.sup.fire_macro_tick(&marta());
        run.run_for(MINUTE - 250, 250).await;
    }
    assert!(
        m.seen().is_empty(),
        "no wake while out cold: {:?}",
        m.seen()
    );
    assert_eq!(
        run.sup.wakes().pending(&marta()).len(),
        1,
        "the due booking is kept, not lost"
    );
    run.run_for(3 * MINUTE, 250).await;
    let seen = m.seen();
    assert_eq!(seen.len(), 1, "one wake carries it all: {seen:?}");
    assert_eq!(seen[0].reason, CAME_TO);
    assert_eq!(seen[0].booked, ["check myself"]);
    let tells = seen[0].message.matches("still there?").count();
    assert_eq!(tells, 7, "every held tell: {}", seen[0].message);
    assert!(
        run.sup.wakes().pending(&marta()).is_empty(),
        "consumed once"
    );
}

#[tokio::test]
async fn a_dead_entity_keeps_its_booking_until_it_is_back() {
    let disk = Disk::new();
    let mut state = downed(&disk.pack, &[]);
    state.entities.get_mut(&marta()).unwrap().dead_until_ms = Some(T0 + 3 * MINUTE);
    seed(&disk, &state);
    let m = Arc::new(Recorder::default());
    let mut run = start(&disk, T0, quiet(), minds(&[(&marta(), &m)]));
    run.sup
        .wakes()
        .book(&marta(), T0, 1, "after the dark")
        .unwrap();
    run.run_for(2 * MINUTE + 30_000, 1_000).await;
    assert!(m.seen().is_empty(), "no wake while dead");
    assert_eq!(
        run.sup.wakes().pending(&marta()).len(),
        1,
        "deferred, not lost"
    );
    run.run_for(2 * MINUTE, 1_000).await;
    let seen = m.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].booked, ["after the dark"]);
}

#[tokio::test]
async fn a_held_tell_and_booking_survive_a_restart_and_wake_her_when_she_comes_to() {
    let mut pack = WorldPack::crypt_v01().unwrap();
    pack.recovery = RecoveryRules::default();
    let disk = Disk::with(pack);
    seed(&disk, &downed(&disk.pack, &[(&marta(), -8)]));
    let m = Arc::new(Recorder::default());
    {
        let mut run = start(&disk, T0, quiet(), minds(&[(&marta(), &m)]));
        run.sup
            .wakes()
            .book(&marta(), T0, 1, "before the restart")
            .unwrap();
        let ada = run.game.login("ada").unwrap();
        run.game.execute(&ada, "tell marta hold on");
        run.run_for(4 * MINUTE, 1_000).await;
        assert!(m.seen().is_empty(), "no wake while out cold");
    }
    // The process stops and comes back later; the tell is past the replay
    // window, but she was held, not idle.
    let m2 = Arc::new(Recorder::default());
    let mut run = start(&disk, T0 + 5 * MINUTE, quiet(), minds(&[(&marta(), &m2)]));
    assert!(
        run.game.read(|w| w.entity(&marta()).unwrap().unconscious),
        "still out cold after the restart"
    );
    run.run_for(5 * MINUTE, 1_000).await;
    let seen = m2.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].reason, CAME_TO);
    assert_eq!(seen[0].booked, ["before the restart"]);
    assert!(seen[0].message.contains("hold on"), "{}", seen[0].message);
}

#[tokio::test]
async fn a_wake_whose_entity_collapsed_before_it_started_goes_back_held() {
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let state = downed(&pack, &[(&marta(), -3)]);
    let clock = Arc::new(ManualClock::new(T0));
    let game = Game::new(
        World::from_state(Arc::clone(&pack), state, Vec::new()),
        None,
        clock,
    );
    let m = Arc::new(Recorder::default());
    let typing = Gate::new(Arc::clone(&game), 5, 0.0);
    let sup = Supervisor::new(
        Arc::clone(&game),
        typing,
        minds(&[(&marta(), &m)]),
        Arc::new(FallbackMind::from_cast(&pack.cast)),
        quiet(),
    );
    // The router took the wake as due (`due` counts everyone as able to
    // act: as if she collapsed in between), and a booking rode on it.
    let booking = sup.wakes().book(&marta(), T0, 1, "later").unwrap();
    lock(&sup.router).force(&marta(), Priority::RoomBatch, "x", T0);
    lock(&sup.router).deliver_booked(&marta(), vec![booking.clone()], T0);
    let wakes = lock(&sup.router).due(T0);
    assert_eq!(wakes.len(), 1);
    Arc::clone(&sup).run_wake(wakes[0].clone()).await;
    assert!(m.seen().is_empty(), "no mind ran");
    let mut router = lock(&sup.router);
    assert!(!router.is_busy(&marta()), "not busy");
    assert_eq!(
        router
            .pending()
            .iter()
            .map(|(e, p, _)| (e.clone(), *p))
            .collect::<Vec<_>>(),
        [(marta(), Priority::RoomBatch)],
        "held, not dropped"
    );
    assert!(router.due_where(T0, |_| false).is_empty(), "still held");
    let back = router.due(T0);
    drop(router);
    assert_eq!(back[0].booked, [booking], "the booking rides along still");
}

#[tokio::test]
async fn a_draught_from_another_wakes_the_npc_it_brings_round() {
    // No come-to roll: only the draught can bring him round this soon.
    let mut pack = WorldPack::crypt_v01().unwrap();
    pack.recovery = RecoveryRules::default();
    let disk = Disk::with(pack);
    let mut state = downed(&disk.pack, &[(&vich(), -3)]);
    let torin = EntityId::from("torin");
    let vial = state.new_item(&mud_world::ProtoId::from("healing_draught"), &disk.pack);
    state.attach(
        vial,
        &mud_world::state::Holder::Entity {
            entity: torin.clone(),
        },
    );
    state.entities.get_mut(&torin).unwrap().room = RoomId::from("wine_vault");
    seed(&disk, &state);
    let v = Arc::new(Recorder::default());
    let mut run = start(&disk, T0 + 30_000, quiet(), minds(&[(&vich(), &v)]));
    // The first recovery step comes with the first tick; then Torin acts.
    run.run_for(1_000, 250).await;
    let r = run.game.execute(&torin, "use draught on vich");
    assert!(r.ok, "{r:?}");
    assert!(run.game.read(|w| !w.entity(&vich()).unwrap().unconscious));
    run.run_for(2_000, 250).await;
    let seen = v.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].reason, CAME_TO);
    let (_, line) = came_to_at(&run.game, &vich()).unwrap();
    assert!(seen[0].seqs.contains(&line));
    assert!(
        seen[0]
            .message
            .contains("Torin uses the healing draught on Vich.")
    );
}
