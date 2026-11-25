//! Booked wakes in the running supervisor (ADR 0008): who may book,
//! when a due booking is held, and what the observer feed shows.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use mud_world::{EntityId, RoomId};
use serde_json::{Value, json};

use super::persist_tests::{Disk, Run, T0, marta, quiet, start, torin};
use super::*;
use crate::mind::{BoxFuture, MindError};
use crate::wakes::BookingAction;

/// A mind that records the bookings each wake carries and, when asked,
/// books a wake itself.
#[derive(Debug, Default)]
struct Booker {
    seen: Mutex<Vec<Vec<u64>>>,
    /// What its `mud_wake_me` answered, when it books.
    answers: Mutex<Vec<Value>>,
    book: bool,
}

impl Booker {
    fn seen(&self) -> Vec<Vec<u64>> {
        self.seen.lock().unwrap().clone()
    }

    fn fired(&self) -> Vec<u64> {
        self.seen().into_iter().flatten().collect()
    }
}

impl Mind for Booker {
    fn kind(&self) -> &'static str {
        "booker"
    }

    fn wake(
        &self,
        ctx: WakeContext,
        tools: MudTools,
    ) -> BoxFuture<'_, Result<MindReply, MindError>> {
        Box::pin(async move {
            self.seen
                .lock()
                .unwrap()
                .push(ctx.booked.iter().map(|b| b.id).collect());
            if self.book {
                let r = tools
                    .call("mud_wake_me", &json!({ "minutes": 5, "note": "later" }))
                    .await;
                self.answers.lock().unwrap().push(r);
            }
            Ok(MindReply::default())
        })
    }
}

fn minds(who: &[(&EntityId, &Arc<Booker>)]) -> HashMap<EntityId, Arc<dyn Mind>> {
    who.iter()
        .map(|(e, m)| ((*e).clone(), Arc::clone(m) as Arc<dyn Mind>))
        .collect()
}

/// The booking events of the feed so far, as (entity, id, action).
fn bookings(r: &mut Run) -> Vec<(EntityId, u64, BookingAction)> {
    r.feed()
        .iter()
        .filter_map(|e| match &**e {
            ObserverEvent::Booking {
                entity, id, action, ..
            } => Some((entity.clone(), *id, *action)),
            _ => None,
        })
        .collect()
}

fn count(events: &[(EntityId, u64, BookingAction)], id: u64, action: BookingAction) -> usize {
    events
        .iter()
        .filter(|(_, i, a)| *i == id && *a == action)
        .count()
}

/// Runs `who`'s lines as commands, then lets the supervisor see them.
async fn act(r: &mut Run, who: &EntityId, lines: &[&str]) {
    for l in lines {
        assert!(r.game.execute(who, l).ok, "{who}: {l}");
    }
    r.run_for(0, 1).await;
}

fn turns(r: &Run, room: &str) -> u8 {
    lock(&r.sup.router).agent_only_turns(&RoomId::from(room))
}

#[tokio::test]
async fn two_self_booking_agents_in_an_empty_room_are_held() {
    let disk = Disk::new();
    let (t, m) = (Arc::new(Booker::default()), Arc::new(Booker::default()));
    let mut r = start(&disk, T0, quiet(), minds(&[(&torin(), &t), (&marta(), &m)]));
    // Torin walks into the Flagon, where Marta is; they talk with no
    // human there until the room's agent-only turns are spent.
    act(
        &mut r,
        &torin(),
        &["e", "e", "say evening", "say ore?", "say no?"],
    )
    .await;
    let max = r.sup.cfg.router.max_agent_only_turns;
    assert!(turns(&r, "rusty_flagon_taproom") >= max);
    r.run_for(60_000, 5_000).await;
    let before = (t.seen().len(), m.seen().len());
    let now = r.game.now_ms();
    let bt = r
        .sup
        .wakes()
        .book(&torin(), now, 1, "talk to marta")
        .unwrap();
    let bm = r
        .sup
        .wakes()
        .book(&marta(), now, 1, "answer torin")
        .unwrap();
    r.run_for(10 * 60_000, 30_000).await;
    assert!(t.fired().is_empty() && m.fired().is_empty(), "both held");
    assert_eq!((t.seen().len(), m.seen().len()), before, "no wake at all");
    assert_eq!(r.sup.wakes().pending(&torin()), vec![bt.clone()]);
    let feed = bookings(&mut r);
    assert_eq!(count(&feed, bt.id, BookingAction::Held), 1, "{feed:?}");
    assert_eq!(count(&feed, bm.id, BookingAction::Held), 1, "{feed:?}");
    // A macro tick resets the room: both go.
    assert!(matches!(
        r.sup.fire_macro_tick(&torin()),
        RouteNote::Queued { .. }
    ));
    r.run_for(10_000, 1_000).await;
    assert_eq!(t.fired(), vec![bt.id]);
    assert_eq!(m.fired(), vec![bm.id]);
    let feed = bookings(&mut r);
    assert_eq!(count(&feed, bt.id, BookingAction::Fired), 1, "{feed:?}");
    assert_eq!(count(&feed, bm.id, BookingAction::Fired), 1, "{feed:?}");
}

#[tokio::test]
async fn a_human_who_speaks_releases_a_chatter_hold() {
    let disk = Disk::new();
    let (t, m) = (Arc::new(Booker::default()), Arc::new(Booker::default()));
    let mut r = start(&disk, T0, quiet(), minds(&[(&torin(), &t), (&marta(), &m)]));
    act(&mut r, &torin(), &["e", "e", "say a", "say b", "say c"]).await;
    let now = r.game.now_ms();
    let bm = r.sup.wakes().book(&marta(), now, 1, "x").unwrap();
    r.run_for(5 * 60_000, 30_000).await;
    assert!(m.fired().is_empty(), "held");
    let ada = r.game.login("ada").unwrap();
    act(&mut r, &ada, &["e", "say hello"]).await;
    r.run_for(60_000, 5_000).await;
    assert_eq!(m.fired(), vec![bm.id], "released by a human");
}

#[tokio::test]
async fn a_lone_agent_booking_fires_at_chatter_max() {
    let disk = Disk::new();
    let t = Arc::new(Booker::default());
    let mut r = start(&disk, T0, quiet(), minds(&[(&torin(), &t)]));
    act(&mut r, &torin(), &["say one", "say two", "say three"]).await;
    let max = r.sup.cfg.router.max_agent_only_turns;
    assert!(turns(&r, "smithy") >= max, "the smithy's turns are spent");
    let now = r.game.now_ms();
    let b = r
        .sup
        .wakes()
        .book(&torin(), now, 1, "check the forge")
        .unwrap();
    r.run_for(2 * 60_000, 10_000).await;
    assert_eq!(t.fired(), vec![b.id], "alone, nobody to chat with");
}

#[tokio::test]
async fn an_afk_spell_holds_a_booking() {
    let disk = Disk::new();
    let t = Arc::new(Booker::default());
    let mut r = start(&disk, T0, quiet(), minds(&[(&torin(), &t)]));
    let b = r.sup.wakes().book(&torin(), T0, 1, "x").unwrap();
    let until = T0 + 10 * 60_000;
    lock(&r.sup.state).get_mut(&torin()).unwrap().afk_until = Some(until);
    r.run_for(8 * 60_000, 30_000).await;
    assert!(t.fired().is_empty(), "away: held");
    assert_eq!(r.sup.wakes().pending(&torin()), vec![b.clone()], "kept");
    r.run_for(4 * 60_000, 30_000).await;
    assert_eq!(t.fired(), vec![b.id], "fires once the spell is over");
}

#[tokio::test]
async fn a_held_booking_is_reported_once_per_hold() {
    let disk = Disk::new();
    let t = Arc::new(Booker::default());
    let mut r = start(&disk, T0, quiet(), minds(&[(&torin(), &t)]));
    let b = r.sup.wakes().book(&torin(), T0, 1, "x").unwrap();
    lock(&r.sup.state).get_mut(&torin()).unwrap().afk_until = Some(T0 + 60 * 60_000);
    r.clock.set(b.due_ms);
    for _ in 0..3 {
        r.run_for(0, 1_000).await;
    }
    let feed = bookings(&mut r);
    assert_eq!(count(&feed, b.id, BookingAction::Held), 1, "{feed:?}");
    // The spell ends: the booking is taken for a wake, but torin leaves
    // before it runs, so it comes back and is held again.
    lock(&r.sup.state).get_mut(&torin()).unwrap().afk_until = None;
    let now = r.game.now_ms();
    let (online, held) = (r.sup.online(), r.sup.held());
    r.sup.booked_wakes(now, &online, &held);
    assert!(r.sup.wakes().in_flight(b.id), "taken");
    r.game.take_offline(&torin());
    for _ in 0..3 {
        r.run_for(0, 1_000).await;
    }
    let feed = bookings(&mut r);
    assert_eq!(count(&feed, b.id, BookingAction::Requeued), 1, "{feed:?}");
    assert_eq!(
        count(&feed, b.id, BookingAction::Held),
        1,
        "a second hold, a second report: {feed:?}"
    );
    assert_eq!(t.fired(), Vec::<u64>::new());
}

#[tokio::test]
async fn fired_and_requeued_bookings_are_published() {
    let disk = Disk::new();
    let smith = Arc::new(Booker::default());
    let mut r = start(&disk, T0, quiet(), minds(&[(&torin(), &smith)]));
    let b = r.sup.wakes().book(&torin(), T0, 1, "x").unwrap();
    r.run_for(2 * 60_000, 30_000).await;
    assert_eq!(smith.fired(), vec![b.id]);
    let feed = bookings(&mut r);
    assert_eq!(count(&feed, b.id, BookingAction::Booked), 1, "{feed:?}");
    assert_eq!(count(&feed, b.id, BookingAction::Fired), 1, "{feed:?}");
    // An owner gone offline between the take and the wake: requeued.
    let now = r.game.now_ms();
    let c = r.sup.wakes().book(&torin(), now, 1, "y").unwrap();
    r.clock.set(c.due_ms);
    let (online, held) = (r.sup.online(), r.sup.held());
    r.sup.booked_wakes(c.due_ms, &online, &held);
    r.game.take_offline(&torin());
    r.run_for(0, 1_000).await;
    let feed = bookings(&mut r);
    assert_eq!(count(&feed, c.id, BookingAction::Requeued), 1, "{feed:?}");
    assert_eq!(count(&feed, c.id, BookingAction::Fired), 0, "{feed:?}");
    // A cancel and a late drop are published too; no note in any of it.
    let d = r.sup.wakes().book(&torin(), now, 30, "zq-note").unwrap();
    assert_eq!(r.sup.wakes().cancel(&torin(), d.id), Ok(true));
    r.clock.set(c.due_ms + r.sup.cfg.booking_max_late_ms + 1);
    r.run_for(0, 1_000).await;
    let feed = r.feed();
    let text = format!("{feed:?}");
    assert!(!text.contains("zq-note"), "the note stays out of the feed");
    let feed = feed
        .iter()
        .filter_map(|e| match &**e {
            ObserverEvent::Booking { id, action, .. } => Some((torin(), *id, *action)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(count(&feed, d.id, BookingAction::Cancelled), 1, "{feed:?}");
    assert_eq!(count(&feed, c.id, BookingAction::Dropped), 1, "{feed:?}");
    assert!(r.sup.wakes().pending(&torin()).is_empty(), "dropped live");
}

#[tokio::test]
async fn ambient_and_mindless_entities_cannot_book() {
    let disk = Disk::new();
    let guard = EntityId::from("guard_01");
    let kestrel = EntityId::from("pc_kestrel");
    let booking = || {
        Arc::new(Booker {
            book: true,
            ..Booker::default()
        })
    };
    let (smith, watch, pc) = (booking(), booking(), Arc::new(Booker::default()));
    let mut r = start(
        &disk,
        T0,
        quiet(),
        minds(&[(&torin(), &smith), (&guard, &watch), (&kestrel, &pc)]),
    );
    assert!(r.sup.can_book(&torin()), "persistent, with a mind");
    assert!(r.sup.can_book(&kestrel), "turing, with a mind");
    assert!(!r.sup.can_book(&guard), "ambient");
    assert!(!r.sup.can_book(&marta()), "persistent, but no mind here");
    assert!(!r.sup.can_book(&EntityId::from("giant_rat_1")), "a mob");
    let ada = r.game.login("ada").unwrap();
    assert!(!r.sup.can_book(&ada), "a human");
    for who in [&guard, &marta(), &ada] {
        let answer = r.sup.book_wake(who, 5, "x");
        assert_eq!(answer["error_code"], "no_wakes", "{who}: {answer}");
        assert_eq!(r.sup.cancel_wake(who, 1)["error_code"], "no_wakes");
    }
    assert_eq!(r.sup.book_wake(&torin(), 5, "x")["ok"], true);
    // In a wake, the guard's tools answer no_wakes; torin's book.
    r.sup.fire_macro_tick(&torin());
    lock(&r.sup.router).force(&guard, Priority::Immediate, "test", T0);
    r.run_for(2_000, 500).await;
    let ga = watch.answers.lock().unwrap().clone();
    assert!(!ga.is_empty(), "the guard woke");
    assert!(ga.iter().all(|a| a["error_code"] == "no_wakes"), "{ga:?}");
    let ta = smith.answers.lock().unwrap().clone();
    assert!(ta.iter().any(|a| a["ok"] == true), "{ta:?}");
    assert_eq!(r.sup.wakes().pending(&guard), Vec::new());
    assert!(r.sup.wakes().all().keys().all(|e| e == &torin()));
}
