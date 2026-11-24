//! Router behavior against a real world.

use std::collections::HashMap;
use std::sync::Arc;

use mud_world::{EntityId, World, WorldPack};

use super::*;

fn world() -> World {
    World::new(Arc::new(WorldPack::crypt_v01().unwrap()), 3)
}

fn id(s: &str) -> EntityId {
    EntityId::from(s)
}

/// Runs a command and feeds its events to the router.
fn run(
    w: &mut World,
    r: &mut Router,
    rt: &HashMap<EntityId, EntityRuntime>,
    who: &EntityId,
    line: &str,
    now: u64,
) -> Vec<RouteNote> {
    let res = w.execute(who, line, now);
    assert!(res.ok, "{line}: {res:?}");
    feed(w, r, rt, now)
}

fn feed(
    w: &mut World,
    r: &mut Router,
    rt: &HashMap<EntityId, EntityRuntime>,
    now: u64,
) -> Vec<RouteNote> {
    let evs = w.drain_fresh();
    evs.iter().flat_map(|e| r.observe(e, w, rt, now)).collect()
}

fn queued_for(notes: &[RouteNote], who: &str) -> Option<(Priority, u64)> {
    notes.iter().rev().find_map(|n| match n {
        RouteNote::Queued {
            entity,
            priority,
            due_ms,
            ..
        } if entity.as_str() == who => Some((*priority, *due_ms)),
        _ => None,
    })
}

fn dropped_for(notes: &[RouteNote], who: &str) -> Option<String> {
    notes.iter().find_map(|n| match n {
        RouteNote::Dropped { entity, why, .. } if entity.as_str() == who => Some(why.clone()),
        _ => None,
    })
}

#[test]
fn human_chatter_batches_in_a_rolling_window() {
    let mut w = world();
    let mut r = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let ada = w.login("ada", 0).unwrap();
    w.drain_fresh();
    run(&mut w, &mut r, &rt, &ada, "e", 0);
    let n = run(&mut w, &mut r, &rt, &ada, "say evening all", 1_000);
    assert_eq!(queued_for(&n, "marta"), Some((Priority::RoomBatch, 3_000)));
    let n = run(&mut w, &mut r, &rt, &ada, "say long day", 1_500);
    assert_eq!(
        queued_for(&n, "marta").unwrap().1,
        3_500,
        "pushed back by the new line"
    );
    let n = run(&mut w, &mut r, &rt, &ada, "say very long", 3_900);
    assert_eq!(
        queued_for(&n, "marta").unwrap().1,
        4_000,
        "capped at the arrival + 4 s"
    );
    assert!(r.due(3_999).iter().all(|w| w.entity != id("marta")));
    let wakes = r.due(4_000);
    let marta = wakes.iter().find(|w| w.entity == id("marta")).unwrap();
    assert!(marta.seqs.len() >= 3, "the enter and the lines coalesce");
}

#[test]
fn naming_or_telling_wakes_at_once() {
    let mut w = world();
    let mut r = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let ada = w.login("ada", 0).unwrap();
    run(&mut w, &mut r, &rt, &ada, "e", 0);
    r.due(10_000);
    r.finish(&id("marta"), 10_000);
    let n = run(
        &mut w,
        &mut r,
        &rt,
        &ada,
        "say Marta, an ale please",
        10_000,
    );
    let (p, due) = queued_for(&n, "marta").unwrap();
    assert_eq!(p, Priority::Immediate);
    assert!(due <= 10_500, "immediate ignores the cooldown");
    let n = run(&mut w, &mut r, &rt, &ada, "tell torin got ore", 10_100);
    assert_eq!(queued_for(&n, "torin").unwrap().0, Priority::Immediate);
}

#[test]
fn agents_alone_get_two_turns_then_silence() {
    let mut w = world();
    let mut r = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let vich = id("vich");
    let marta = id("marta");
    for d in ["w", "n", "e"] {
        run(&mut w, &mut r, &rt, &vich, d, 0);
    }
    let room = RoomId::from("rusty_flagon_taproom");
    r.rooms.insert(room.clone(), RoomChatter::default());
    let n = run(&mut w, &mut r, &rt, &vich, "say quiet night", 100);
    assert!(queued_for(&n, "marta").is_some());
    let n = run(&mut w, &mut r, &rt, &marta, "say it is", 200);
    assert!(queued_for(&n, "vich").is_some());
    let n = run(&mut w, &mut r, &rt, &vich, "say marta, pour one", 300);
    assert_eq!(
        dropped_for(&n, "marta").as_deref(),
        Some("anti-chatter: agent-only turns exhausted")
    );
    assert_eq!(r.agent_only_turns(&room), 3);
    let ada = w.login("ada", 400).unwrap();
    w.drain_fresh();
    run(&mut w, &mut r, &rt, &ada, "e", 500);
    assert_eq!(r.agent_only_turns(&room), 0, "a human resets the room");
    r.pending.clear();
    let n = run(&mut w, &mut r, &rt, &vich, "say evening", 600);
    assert_eq!(queued_for(&n, "marta").unwrap().0, Priority::Ambient);
}

#[test]
fn budgets_shed_wakes() {
    let mut w = world();
    let mut r = Router::new(RouterConfig::default());
    let mut rt = HashMap::new();
    rt.insert(
        id("marta"),
        EntityRuntime {
            budget: Some(BudgetLevel::Hard),
            afk_until_ms: None,
        },
    );
    let ada = w.login("ada", 0).unwrap();
    w.drain_fresh();
    let n = run(&mut w, &mut r, &rt, &ada, "e", 0);
    assert_eq!(
        dropped_for(&n, "marta").as_deref(),
        Some("budget: hard cap")
    );
    let n = run(&mut w, &mut r, &rt, &ada, "say marta?", 10);
    assert_eq!(
        queued_for(&n, "marta").unwrap().0,
        Priority::Immediate,
        "the fallback still answers"
    );
}

#[test]
fn soft_budget_drops_ambient_and_doubles_cooldown() {
    let mut w = world();
    let mut r = Router::new(RouterConfig::default());
    let mut rt = HashMap::new();
    rt.insert(
        id("marta"),
        EntityRuntime {
            budget: Some(BudgetLevel::Soft),
            afk_until_ms: None,
        },
    );
    let ada = w.login("ada", 0).unwrap();
    run(&mut w, &mut r, &rt, &ada, "e", 0);
    for d in ["w", "n", "e"] {
        run(&mut w, &mut r, &rt, &id("vich"), d, 10);
    }
    let n = run(&mut w, &mut r, &rt, &id("vich"), "emote counts coins", 20);
    assert_eq!(
        dropped_for(&n, "marta").as_deref(),
        Some("budget: soft cap disables ambient wakes")
    );
}

#[test]
fn macro_ticks_wait_for_an_empty_room() {
    let mut w = world();
    let mut r = Router::new(RouterConfig::default());
    let note = r.macro_tick(&id("torin"), &w, 5);
    assert!(matches!(
        note,
        RouteNote::Queued {
            priority: Priority::MacroTick,
            ..
        }
    ));
    let wakes = r.due(5);
    assert_eq!(wakes[0].priority, Priority::MacroTick);
    r.finish(&id("torin"), 6);
    let ada = w.login("ada", 7).unwrap();
    w.execute(&ada, "w", 8);
    let note = r.macro_tick(&id("torin"), &w, 9);
    assert!(matches!(note, RouteNote::Dropped { .. }));
}

#[test]
fn a_turing_pc_misses_some_chatter_but_not_its_goals() {
    let mut w = world();
    let mut r = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    w.bring_online(&id("kestrel"), 0);
    let ada = w.login("ada", 0).unwrap();
    w.drain_fresh();
    run(&mut w, &mut r, &rt, &ada, "e", 0);
    let mut heard = 0;
    let mut dropped = 0;
    for i in 0..60 {
        let n = run(
            &mut w,
            &mut r,
            &rt,
            &ada,
            &format!("say weather is grim {i}"),
            100 + i,
        );
        if dropped_for(&n, "kestrel").is_some() {
            dropped += 1;
        } else {
            heard += 1;
        }
        r.pending.clear();
    }
    assert!(
        dropped > 20 && heard > 5,
        "heard {heard}, dropped {dropped}"
    );
    for i in 0..10 {
        let n = run(
            &mut w,
            &mut r,
            &rt,
            &ada,
            &format!("say anyone for the cellar {i}"),
            1_000 + i,
        );
        assert!(
            queued_for(&n, "kestrel").is_some(),
            "goal keywords always land"
        );
        r.pending.clear();
    }
}

#[test]
fn afk_holds_routine_wakes_but_not_urgent_ones() {
    let mut w = world();
    let mut r = Router::new(RouterConfig::default());
    let mut rt = HashMap::new();
    rt.insert(
        id("kestrel"),
        EntityRuntime {
            budget: None,
            afk_until_ms: Some(120_000),
        },
    );
    w.bring_online(&id("kestrel"), 0);
    let ada = w.login("ada", 0).unwrap();
    w.drain_fresh();
    run(&mut w, &mut r, &rt, &ada, "e", 0);
    r.pending.clear();
    let n = run(&mut w, &mut r, &rt, &ada, "say cellar run anyone", 1_000);
    assert_eq!(queued_for(&n, "kestrel").unwrap().1, 120_000);
    r.pending.clear();
    let n = run(&mut w, &mut r, &rt, &ada, "tell kestrel you there?", 1_000);
    assert!(queued_for(&n, "kestrel").unwrap().1 <= 1_500);
}

#[test]
fn busy_entities_wait_and_cooldowns_apply() {
    let mut w = world();
    let mut r = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let ada = w.login("ada", 0).unwrap();
    run(&mut w, &mut r, &rt, &ada, "e", 0);
    let first = r.due(5_000);
    assert!(first.iter().any(|w| w.entity == id("marta")), "{first:?}");
    run(&mut w, &mut r, &rt, &ada, "say again", 5_000);
    assert!(r.due(60_000).is_empty(), "marta is still busy");
    r.finish(&id("marta"), 60_000);
    let n = run(&mut w, &mut r, &rt, &ada, "say and again", 60_000);
    let due = queued_for(&n, "marta").unwrap().1;
    assert!(due >= 63_000, "cooldown after the last wake: {due}");
    r.forget(&id("marta"));
    assert!(r.pending().iter().all(|(e, _, _)| e != &id("marta")));
}

#[test]
fn names_match_whole_words() {
    assert!(names("yo Kestrel, you in?", "kestrel", &[]));
    assert!(names("ask the barkeep", "Marta", &["barkeep".into()]));
    assert!(!names("kestrels nest", "kestrel", &[]));
    assert!(names("the giant rat bites", "giant rat", &[]));
}
