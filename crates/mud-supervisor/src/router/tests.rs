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

/// Vich walked to the taproom, with the room's counter fresh.
fn vich_in_the_taproom(
    w: &mut World,
    r: &mut Router,
    rt: &HashMap<EntityId, EntityRuntime>,
) -> RoomId {
    for d in ["w", "n", "e"] {
        run(w, r, rt, &id("vich"), d, 0);
    }
    let room = RoomId::from("rusty_flagon_taproom");
    r.rooms.insert(room.clone(), RoomChatter::default());
    r.pending.clear();
    room
}

#[test]
fn npcs_treat_an_agent_pc_as_they_treat_a_human() {
    // Regression: kestrel (an agent-played PC) counted as an agent, so Hale
    // and Marta fell silent after two exchanges with it — a tell a human
    // would never see.
    let mut w = world();
    let mut r = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let room = vich_in_the_taproom(&mut w, &mut r, &rt);
    let kestrel = id("pc_kestrel");
    w.bring_online(&kestrel, 50);
    w.drain_fresh();
    for (i, line) in [
        "say evening",
        "say vich, got a key?",
        "say well?",
        "say hello?",
    ]
    .iter()
    .enumerate()
    {
        let t = 1_000 * (u64::try_from(i).unwrap() + 1);
        let n = run(&mut w, &mut r, &rt, &kestrel, line, t);
        assert!(
            dropped_for(&n, "vich").is_none(),
            "{line}: a player's speech is never chatter-capped: {n:?}"
        );
        assert_eq!(r.agent_only_turns(&room), 0, "a player resets the room");
        r.pending.clear();
        // And the NPCs answering it, with a player present, are not capped.
        let n = run(&mut w, &mut r, &rt, &id("marta"), "say mm", t + 100);
        assert!(dropped_for(&n, "vich").is_none(), "{n:?}");
        let n = run(&mut w, &mut r, &rt, &id("vich"), "say aye", t + 200);
        assert!(dropped_for(&n, "marta").is_none(), "{n:?}");
        r.pending.clear();
    }
}

#[test]
fn an_agent_pcs_chatter_batches_like_a_humans() {
    let mut w = world();
    let mut r = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    vich_in_the_taproom(&mut w, &mut r, &rt);
    let kestrel = id("pc_kestrel");
    w.bring_online(&kestrel, 50);
    w.drain_fresh();
    let n = run(&mut w, &mut r, &rt, &kestrel, "say quiet in here", 1_000);
    assert_eq!(queued_for(&n, "vich").unwrap().0, Priority::RoomBatch);
}

#[test]
fn macro_ticks_wait_while_an_agent_pc_is_in_the_room() {
    let mut w = world();
    let mut r = Router::new(RouterConfig::default());
    let kestrel = id("pc_kestrel");
    w.bring_online(&kestrel, 1);
    let note = r.macro_tick(&id("marta"), &w, 5, false);
    assert!(
        matches!(note, RouteNote::Dropped { ref why, .. } if why.contains("player")),
        "{note:?}"
    );
}

/// The owner's run of 2026-11-26, after a restart: kestrel (an agent PC,
/// online) and torin stand in the flooded cistern, with the humans who
/// logged in as `humans` moved there too, and nothing has happened there.
fn the_cistern_after_a_restart(humans: &[&str]) -> (World, Vec<EntityId>) {
    let mut w = world();
    let logged: Vec<EntityId> = humans.iter().map(|n| w.login(n, 0).unwrap()).collect();
    let mut st = w.snapshot();
    for who in [id("pc_kestrel"), id("torin")].iter().chain(&logged) {
        let e = st.entities.get_mut(who).unwrap();
        e.room = RoomId::from("flooded_cistern");
        e.online = true;
    }
    let mut w = World::from_state(Arc::new(WorldPack::crypt_v01().unwrap()), st, Vec::new());
    // A restart takes humans out of the world: they log in again.
    for name in humans {
        w.login(name, 1).unwrap();
    }
    w.drain_fresh();
    (w, logged)
}

fn tick(r: &mut Router, w: &World, who: &str, now: u64) -> RouteNote {
    r.macro_tick(&id(who), w, now, false)
}

#[test]
fn an_idle_agent_pc_does_not_hold_an_npcs_macro_tick_back() {
    // Regression (the owner's run of 2026-11-26): torin's tick was
    // "deferred: a player is present" every few minutes, forever, because
    // kestrel stood in the cistern and never acted.
    let (w, _) = the_cistern_after_a_restart(&[]);
    let mut r = Router::new(RouterConfig::default());
    let now = 50_000_000;
    let note = tick(&mut r, &w, "torin", now);
    assert!(matches!(note, RouteNote::Queued { .. }), "{note:?}");
    let note = tick(&mut r, &w, "pc_kestrel", now);
    assert!(matches!(note, RouteNote::Queued { .. }), "{note:?}");
    let due = r.due(now);
    assert!(due.iter().all(|wake| wake.macro_tick), "{due:?}");
    assert_eq!(due.len(), 2);
}

#[test]
fn a_player_who_acts_defers_the_tick_for_the_window_only() {
    let (mut w, _) = the_cistern_after_a_restart(&[]);
    let mut r = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let t = 50_000_000;
    run(
        &mut w,
        &mut r,
        &rt,
        &id("pc_kestrel"),
        "say the water is cold",
        t,
    );
    r.pending.clear();
    for at in [t, t + 1, t + 599_999] {
        let note = tick(&mut r, &w, "torin", at);
        assert!(
            matches!(note, RouteNote::Dropped { ref why, .. } if why.contains("acted")),
            "{at}: {note:?}"
        );
    }
    let note = tick(&mut r, &w, "torin", t + 600_000);
    assert!(matches!(note, RouteNote::Queued { .. }), "{note:?}");
    // Its own act never defers kestrel's tick.
    let note = tick(&mut r, &w, "pc_kestrel", t + 1);
    assert!(matches!(note, RouteNote::Queued { .. }), "{note:?}");
}

#[test]
fn a_human_player_defers_the_tick_exactly_as_an_agent_pc_does() {
    // NPCs treat agent PCs as humans: the rule reads who acted and when,
    // never who plays them. Kestrel is offline here; ada is the player.
    let (mut w, humans) = the_cistern_after_a_restart(&["ada"]);
    let ada = humans[0].clone();
    let kestrel = id("pc_kestrel");
    w.take_offline(&kestrel, 1);
    w.drain_fresh();
    let mut r = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let t = 50_000_000;
    let note = tick(&mut r, &w, "torin", t);
    assert!(
        matches!(note, RouteNote::Queued { .. }),
        "idle ada: {note:?}"
    );
    r.pending.clear();
    run(&mut w, &mut r, &rt, &ada, "say hello?", t);
    r.pending.clear();
    let note = tick(&mut r, &w, "torin", t + 599_999);
    assert!(matches!(note, RouteNote::Dropped { .. }), "{note:?}");
    let note = tick(&mut r, &w, "torin", t + 600_000);
    assert!(matches!(note, RouteNote::Queued { .. }), "{note:?}");
}

#[test]
fn a_window_of_zero_never_defers_and_an_absent_actor_never_counts() {
    let (mut w, _) = the_cistern_after_a_restart(&[]);
    let rt = HashMap::new();
    let mut r = Router::new(RouterConfig {
        macro_defer_ms: 0,
        ..RouterConfig::default()
    });
    let t = 50_000_000;
    run(&mut w, &mut r, &rt, &id("pc_kestrel"), "say hm", t);
    r.pending.clear();
    assert!(matches!(
        tick(&mut r, &w, "torin", t),
        RouteNote::Queued { .. }
    ));
    // A player who acted, then left, defers nothing.
    let mut r = Router::new(RouterConfig::default());
    let kestrel = id("pc_kestrel");
    run(&mut w, &mut r, &rt, &kestrel, "say back soon", t + 1);
    w.take_offline(&kestrel, t + 2);
    w.drain_fresh();
    r.pending.clear();
    assert!(matches!(
        tick(&mut r, &w, "torin", t + 3),
        RouteNote::Queued { .. }
    ));
}

#[test]
fn a_hidden_pc_alone_gets_its_macro_tick() {
    // Regression (log 7:59:08): kestrel's own tick was always "deferred:
    // a player is present", the player being kestrel.
    let mut w = world();
    let mut r = Router::new(RouterConfig::default());
    let kestrel = id("pc_kestrel");
    w.bring_online(&kestrel, 1);
    let note = r.macro_tick(&kestrel, &w, 5, false);
    assert!(matches!(note, RouteNote::Queued { .. }), "{note:?}");
    assert!(
        r.due(5)
            .iter()
            .any(|wake| wake.entity == kestrel && wake.macro_tick)
    );
    let ada = w.login("ada", 6).unwrap();
    w.drain_fresh();
    run(&mut w, &mut r, &HashMap::new(), &ada, "e", 7);
    r.finish(&kestrel, 7);
    r.pending.clear();
    let note = r.macro_tick(&kestrel, &w, 8, false);
    assert!(
        matches!(note, RouteNote::Dropped { ref why, .. } if why.contains("player")),
        "another player still defers it: {note:?}"
    );
}

/// The engine's line about `who`'s body, as combat emits it: `who` is its
/// actor, with `verb`, in its room, perceived by the room.
fn body_line(w: &World, who: &str, verb: &str, kind: EventKind) -> Event {
    let e = w.entity(&id(who)).unwrap();
    let room = e.room.clone();
    Event {
        seq: Seq(w.last_seq().0 + 1),
        at_ms: 0,
        room: Some(room.clone()),
        kind,
        tags: vec!["combat".into()],
        actor: Some(mud_world::ActorRef {
            id: e.id.clone(),
            name: e.name.clone(),
        }),
        verb: Some(verb.into()),
        target: None,
        untrusted_text: None,
        narrative: format!("{} collapses.", e.name),
        data: serde_json::Value::Null,
        recipients: w.present_in(&room).map(|p| p.id.clone()).collect(),
        human_actor: e.human,
        player_actor: e.kind == EntityKind::Player,
        party_rooms: Vec::new(),
    }
}

#[test]
fn martas_collapse_spends_no_agent_turn_in_the_wine_vault() {
    let mut w = world();
    let mut r = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let vault = RoomId::from("wine_vault");
    let mut s = w.snapshot();
    for who in ["marta", "vich"] {
        if let Some(e) = s.entities.get_mut(&id(who)) {
            e.room = vault.clone();
        }
    }
    let recent: Vec<Event> = w.logged().cloned().collect();
    w = World::from_state(Arc::clone(w.pack()), s, recent);
    r.rooms.insert(
        vault.clone(),
        RoomChatter {
            agent_only_turns: 1,
        },
    );
    for (verb, kind) in [
        (mud_world::VERB_STAGGER, EventKind::Combat),
        (mud_world::VERB_COLLAPSE, EventKind::Combat),
        (mud_world::VERB_DIE, EventKind::Death),
    ] {
        let ev = body_line(&w, "marta", verb, kind);
        assert!(ev.is_body_line());
        let notes = r.observe(&ev, &w, &rt, 100);
        assert_eq!(r.agent_only_turns(&vault), 1, "{verb}: no turn spent");
        assert!(
            dropped_for(&notes, "vich").is_none(),
            "{verb}: never dropped as chatter"
        );
    }
    // Her own say is a turn; the room holds at the bound after it.
    let mut say = body_line(&w, "marta", "say", EventKind::Speech);
    say.untrusted_text = Some("help".into());
    r.observe(&say, &w, &rt, 200);
    assert_eq!(r.agent_only_turns(&vault), 2, "an act still counts");
    let ev = body_line(&w, "marta", mud_world::VERB_COLLAPSE, EventKind::Combat);
    let notes = r.observe(&ev, &w, &rt, 300);
    assert!(
        queued_for(&notes, "vich").is_some(),
        "a held room still lets the world's line through: {notes:?}"
    );
}

#[test]
fn a_pcs_collapse_does_not_reset_the_room_as_a_human_would() {
    let mut w = world();
    let mut r = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let kestrel = id("pc_kestrel");
    w.bring_online(&kestrel, 1);
    w.drain_fresh();
    let room = w.entity(&kestrel).unwrap().room.clone();
    r.rooms.insert(
        room.clone(),
        RoomChatter {
            agent_only_turns: 2,
        },
    );
    let ev = body_line(
        &w,
        "pc_kestrel",
        mud_world::VERB_COLLAPSE,
        EventKind::Combat,
    );
    r.observe(&ev, &w, &rt, 10);
    assert_eq!(r.agent_only_turns(&room), 2, "kestrel did not act");
    let respawn = body_line(&w, "pc_kestrel", mud_world::VERB_RESPAWN, EventKind::Enter);
    r.observe(&respawn, &w, &rt, 20);
    assert_eq!(r.agent_only_turns(&room), 2);
    run(&mut w, &mut r, &rt, &kestrel, "say back on my feet", 30);
    assert_eq!(r.agent_only_turns(&room), 0, "kestrel's own say does");
}

#[test]
fn agent_tells_count_as_agent_only_turns() {
    // Regression: a tell has no room, so agent-to-agent tells skipped the
    // anti-chatter counter and could ping-pong forever.
    let mut w = world();
    let mut r = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let (vich, marta) = (id("vich"), id("marta"));
    let n = run(&mut w, &mut r, &rt, &vich, "tell marta psst", 100);
    assert!(queued_for(&n, "marta").is_some(), "{n:?}");
    let n = run(&mut w, &mut r, &rt, &marta, "tell vich what", 200);
    assert!(queued_for(&n, "vich").is_some(), "{n:?}");
    let n = run(&mut w, &mut r, &rt, &vich, "tell marta psst again", 300);
    assert_eq!(
        dropped_for(&n, "marta").as_deref(),
        Some("anti-chatter: agent-only turns exhausted"),
        "{n:?}"
    );
    let n = run(&mut w, &mut r, &rt, &marta, "tell vich still here", 400);
    assert!(dropped_for(&n, "vich").is_some(), "{n:?}");
    // A human next to either party keeps tells flowing.
    let ada = w.login("ada", 500).unwrap();
    w.drain_fresh();
    run(&mut w, &mut r, &rt, &ada, "e", 600);
    r.pending.clear();
    for i in 0..5 {
        let n = run(
            &mut w,
            &mut r,
            &rt,
            &vich,
            &format!("tell marta hi {i}"),
            700 + i,
        );
        assert!(queued_for(&n, "marta").is_some(), "{n:?}");
    }
    // A human's tell is never held back.
    let n = run(&mut w, &mut r, &rt, &ada, "tell vich hey", 800);
    assert!(queued_for(&n, "vich").is_some(), "{n:?}");
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
fn macro_ticks_wait_while_a_player_acts_in_the_room() {
    let mut w = world();
    let mut r = Router::new(RouterConfig::default());
    let note = r.macro_tick(&id("torin"), &w, 5, false);
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
    let note = r.macro_tick(&id("torin"), &w, 9, false);
    assert!(matches!(note, RouteNote::Dropped { .. }));
}

#[test]
fn a_retried_macro_tick_does_not_reopen_a_held_room() {
    // A tick whose wake never reached the mind comes again in minutes
    // (`tick_retry_ms`). The tick it retries reset the room already; the
    // retry must not hand the other agents a fresh set of turns each time.
    let w = world();
    let mut r = Router::new(RouterConfig::default());
    let smithy = RoomId::from("smithy");
    let max = r.config().max_agent_only_turns;
    r.rooms.insert(
        smithy.clone(),
        RoomChatter {
            agent_only_turns: max,
        },
    );
    let note = r.macro_tick(&id("torin"), &w, 5, true);
    assert!(matches!(note, RouteNote::Queued { .. }), "{note:?}");
    assert!(r.due(5).iter().any(|wake| wake.macro_tick), "still a tick");
    assert_eq!(r.agent_only_turns(&smithy), max, "the room stays held");
    r.finish(&id("torin"), 6);
    r.macro_tick(&id("torin"), &w, 7, false);
    assert_eq!(r.agent_only_turns(&smithy), 0, "a scheduled tick resets it");
}

#[test]
fn a_macro_tick_keeps_its_identity_inside_a_merged_wake() {
    let w = world();
    let mut r = Router::new(RouterConfig::default());
    r.force(&id("torin"), Priority::RoomBatch, "leave by ada", 0);
    r.macro_tick(&id("torin"), &w, 5, false);
    let wakes = r.due(5);
    assert_eq!(wakes[0].priority, Priority::RoomBatch);
    assert!(wakes[0].macro_tick, "{wakes:?}");
    r.finish(&id("torin"), 6);
    r.force(&id("torin"), Priority::Immediate, "login", 10);
    assert!(!r.due(10)[0].macro_tick, "an ordinary wake is not a tick");
}

#[test]
fn a_turing_pc_misses_some_chatter_but_not_its_goals() {
    let mut w = world();
    let mut r = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    w.bring_online(&id("pc_kestrel"), 0);
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
        if dropped_for(&n, "pc_kestrel").is_some() {
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
            queued_for(&n, "pc_kestrel").is_some(),
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
        id("pc_kestrel"),
        EntityRuntime {
            budget: None,
            afk_until_ms: Some(120_000),
        },
    );
    w.bring_online(&id("pc_kestrel"), 0);
    let ada = w.login("ada", 0).unwrap();
    w.drain_fresh();
    run(&mut w, &mut r, &rt, &ada, "e", 0);
    r.pending.clear();
    let n = run(&mut w, &mut r, &rt, &ada, "say cellar run anyone", 1_000);
    assert_eq!(queued_for(&n, "pc_kestrel").unwrap().1, 120_000);
    r.pending.clear();
    let n = run(&mut w, &mut r, &rt, &ada, "tell kestrel you there?", 1_000);
    assert!(queued_for(&n, "pc_kestrel").unwrap().1 <= 1_500);
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
fn a_wake_queued_while_busy_waits_out_the_cooldown() {
    // Regression: the floor was computed when the event came in, from the
    // wake before, so a wake queued mid-wake fired right after finish().
    let mut w = world();
    let mut r = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let ada = w.login("ada", 0).unwrap();
    run(&mut w, &mut r, &rt, &ada, "e", 0);
    assert!(r.due(5_000).iter().any(|w| w.entity == id("marta")));
    run(&mut w, &mut r, &rt, &ada, "say again", 5_000);
    r.finish(&id("marta"), 60_000);
    assert!(
        r.due(60_001).iter().all(|w| w.entity != id("marta")),
        "the cooldown starts when the wake ends"
    );
    let due = r
        .pending()
        .into_iter()
        .find(|(e, _, _)| e == &id("marta"))
        .unwrap()
        .2;
    assert!((63_000..=68_000).contains(&due), "{due}");
    assert!(r.due(due).iter().any(|w| w.entity == id("marta")));
}

#[test]
fn an_urgent_wake_queued_while_busy_still_goes_at_once() {
    let mut w = world();
    let mut r = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let ada = w.login("ada", 0).unwrap();
    run(&mut w, &mut r, &rt, &ada, "e", 0);
    assert!(r.due(5_000).iter().any(|w| w.entity == id("marta")));
    run(&mut w, &mut r, &rt, &ada, "say marta, an ale", 5_000);
    r.finish(&id("marta"), 60_000);
    assert!(r.due(60_000).iter().any(|w| w.entity == id("marta")));
}

#[test]
fn script_narration_never_wakes_anyone() {
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let mut sources = mud_script::ScriptSources {
        manifest: Some("[[verbs]]\nname = \"wave\"\nusage = \"wave\"\nfile = \"w.rhai\"\n".into()),
        ..mud_script::ScriptSources::default()
    };
    sources.files.insert(
        "w.rhai".into(),
        "fn on_wave(ctx) { [narrate(\"Hale, Marta: someone waves at you!\")] }".into(),
    );
    let host = Arc::new(mud_script::ScriptHost::from_sources(&sources, &pack).unwrap());
    let mut w = World::new(pack, 3);
    w.set_hooks(Some(host), 0, mud_world::HooksChange::Start);
    let mut r = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let ada = w.login("ada", 0).unwrap();
    w.drain_fresh();
    let res = w.execute(&ada, "wave", 1_000);
    assert!(res.ok, "{res:?}");
    let evs = w.drain_fresh();
    assert_eq!(evs.len(), 1);
    assert!(evs[0].reaches(&id("guard_01")));
    let notes: Vec<RouteNote> = evs
        .iter()
        .flat_map(|e| r.observe(e, &w, &rt, 1_000))
        .collect();
    assert!(
        notes.iter().all(|n| !matches!(n, RouteNote::Queued { .. })),
        "{notes:?}"
    );
}

#[test]
fn names_match_whole_words() {
    assert!(names("yo Kestrel, you in?", "kestrel", &[]));
    assert!(names("ask the barkeep", "Marta", &["barkeep".into()]));
    assert!(!names("kestrels nest", "kestrel", &[]));
    assert!(names("the giant rat bites", "giant rat", &[]));
}

// ---- boot derivations (ADR 0011, phase 4) ----

/// The world as a restart reloads it: the stored state and its log (every
/// human offline), with the log cut to the events from `from` on.
fn reloaded(w: &World, from: u64) -> World {
    let recent: Vec<Event> = w.logged().filter(|e| e.seq.0 >= from).cloned().collect();
    World::from_state(Arc::clone(w.pack()), w.snapshot(), recent)
}

fn seqs_of(w: &World, kind: EventKind) -> Vec<Seq> {
    w.logged()
        .filter(|e| e.kind == kind)
        .map(|e| e.seq)
        .collect()
}

#[test]
fn replay_queues_only_immediate_wakes() {
    let mut w = world();
    let mut live = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let ada = w.login("ada", 0).unwrap();
    run(&mut w, &mut live, &rt, &ada, "e", 1_000);
    run(&mut w, &mut live, &rt, &ada, "say evening all", 2_000);
    run(
        &mut w,
        &mut live,
        &rt,
        &ada,
        "say Marta, an ale please",
        3_000,
    );
    run(&mut w, &mut live, &rt, &ada, "tell torin got ore", 4_000);
    let w = reloaded(&w, 0);
    let mut r = Router::new(RouterConfig::default());
    let notes = r.replay(&w, &rt, 5_000, |_| 0, 120_000);
    assert_eq!(queued_for(&notes, "marta").unwrap().0, Priority::Immediate);
    assert_eq!(queued_for(&notes, "torin").unwrap().0, Priority::Immediate);
    let wakes = r.due(5_500);
    let marta = wakes.iter().find(|x| x.entity == id("marta")).unwrap();
    let speech = seqs_of(&w, EventKind::Speech);
    assert_eq!(
        marta.seqs,
        [speech[1]],
        "the line naming her, not the room batch nor the arrival"
    );
    assert!(marta.reason.starts_with("replayed speech by"), "{marta:?}");
    let torin = wakes.iter().find(|x| x.entity == id("torin")).unwrap();
    assert_eq!(torin.seqs, seqs_of(&w, EventKind::Tell));
    assert_eq!(wakes.len(), 2, "no room batch, no ambient: {wakes:?}");
}

#[test]
fn replay_skips_what_a_wake_consumed_and_what_is_too_old() {
    let mut w = world();
    let mut live = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let ada = w.login("ada", 0).unwrap();
    run(&mut w, &mut live, &rt, &ada, "tell torin one", 10_000);
    run(&mut w, &mut live, &rt, &ada, "tell torin two", 20_000);
    let tells = seqs_of(&w, EventKind::Tell);
    let w = reloaded(&w, 0);
    // Torin's committed wake consumed the first tell.
    let mut r = Router::new(RouterConfig::default());
    r.replay(&w, &rt, 30_000, |_| tells[0].0, 120_000);
    assert_eq!(r.due(31_000)[0].seqs, [tells[1]], "only the second");
    // At the edge of the window it still goes; one ms later it does not.
    let mut r = Router::new(RouterConfig::default());
    r.replay(&w, &rt, 140_000, |_| 0, 120_000);
    assert_eq!(r.due(141_000)[0].seqs, [tells[1]]);
    let mut r = Router::new(RouterConfig::default());
    let notes = r.replay(&w, &rt, 140_001, |_| 0, 120_000);
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(r.due(u64::MAX), Vec::<Wake>::new());
}

#[test]
fn chatter_counters_are_rebuilt_from_the_event_tail() {
    let mut w = world();
    let mut live = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let (vich, marta) = (id("vich"), id("marta"));
    for d in ["w", "n", "e"] {
        run(&mut w, &mut live, &rt, &vich, d, 0);
    }
    run(&mut w, &mut live, &rt, &vich, "say quiet night", 100);
    run(&mut w, &mut live, &rt, &marta, "say it is", 200);
    let n = run(&mut w, &mut live, &rt, &vich, "tell marta pour one", 300);
    assert!(dropped_for(&n, "marta").is_some(), "{n:?}");
    let room = RoomId::from("rusty_flagon_taproom");
    let counted = live.agent_only_turns(&room);
    assert!(counted >= 2, "{counted}");
    let w = reloaded(&w, 0);
    let mut r = Router::new(RouterConfig::default());
    let notes = r.replay(&w, &rt, 400, |_| 0, 120_000);
    for (room, c) in &live.rooms {
        assert_eq!(r.agent_only_turns(room), c.agent_only_turns, "{room}");
    }
    // The tell the live router held back is held back again.
    assert_eq!(
        dropped_for(&notes, "marta").as_deref(),
        Some("anti-chatter: agent-only turns exhausted"),
        "{notes:?}"
    );
    assert!(queued_for(&notes, "marta").is_none(), "{notes:?}");
}

#[test]
fn the_fold_reads_presence_from_the_recipients() {
    // At boot no human is online, so the world shows nobody: the fold
    // asks whether a player perceived the agent's event.
    let mut w = world();
    let mut live = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let ada = w.login("ada", 0).unwrap();
    run(&mut w, &mut live, &rt, &ada, "e", 100);
    run(&mut w, &mut live, &rt, &id("marta"), "say evening", 200);
    run(&mut w, &mut live, &rt, &id("marta"), "say long day", 300);
    run(&mut w, &mut live, &rt, &id("marta"), "say quiet", 400);
    let room = RoomId::from("rusty_flagon_taproom");
    assert_eq!(live.agent_only_turns(&room), 0, "ada was there");
    let w = reloaded(&w, 0);
    assert!(!w.player_present(&room), "ada is offline after the restart");
    let mut r = Router::new(RouterConfig::default());
    r.replay(&w, &rt, 500, |_| 0, 120_000);
    assert_eq!(r.agent_only_turns(&room), 0, "ada perceived each line");
}

#[test]
fn a_history_the_log_does_not_reach_starts_held() {
    let mut w = world();
    let mut live = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let ada = w.login("ada", 0).unwrap();
    run(&mut w, &mut live, &rt, &ada, "say one", 100);
    run(&mut w, &mut live, &rt, &ada, "e", 200);
    let max = RouterConfig::default().max_agent_only_turns;
    // The log keeps only the arrival in the taproom: the square's past is
    // unknown, and so is every room the log never shows.
    let arrival = seqs_of(&w, EventKind::Enter).last().copied().unwrap();
    let w = reloaded(&w, arrival.0);
    assert_eq!(w.oldest_logged(), Some(arrival));
    let mut r = Router::new(RouterConfig::default());
    r.replay(&w, &rt, 300, |_| 0, 120_000);
    let taproom = RoomId::from("rusty_flagon_taproom");
    assert_eq!(r.agent_only_turns(&taproom), 0, "a human event resets");
    assert_eq!(
        r.agent_only_turns(&RoomId::from("dark_alley")),
        max,
        "an unseen room starts held"
    );
    // A whole history starts every room at 0.
    let mut r = Router::new(RouterConfig::default());
    r.replay(&world(), &rt, 0, |_| 0, 120_000);
    assert_eq!(r.agent_only_turns(&taproom), 0);
    assert_eq!(r.agent_only_turns(&RoomId::from("dark_alley")), 0);
}

/// Asserts that the boot router holds at least as much as the live one in
/// every room either of them counts.
fn holds_at_least(live: &Router, boot: &Router) {
    let max = live.cfg.max_agent_only_turns;
    for room in live.rooms.keys().chain(boot.rooms.keys()) {
        let (l, b) = (live.agent_only_turns(room), boot.agent_only_turns(room));
        assert!(b.min(max) >= l.min(max), "{room}: live {l} boot {b}");
    }
}

#[test]
fn a_tell_counts_where_its_parties_stood_when_it_was_sent() {
    // The guard tells marta from the square, then walks into the taproom.
    // The fold counts the tell in the square, as the live counter did, not
    // where the guard stands at boot.
    let mut w = world();
    let mut live = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let guard = id("guard_01");
    let square = RoomId::from("town_square");
    assert_eq!(
        w.entity(&guard).map(|g| g.room.clone()),
        Some(square.clone())
    );
    run(&mut w, &mut live, &rt, &guard, "tell marta psst", 100);
    run(&mut w, &mut live, &rt, &guard, "e", 200);
    assert_eq!(live.agent_only_turns(&square), 2, "the tell and the leave");
    let w = reloaded(&w, 0);
    let mut r = Router::new(RouterConfig::default());
    r.replay(&w, &rt, 300, |_| u64::MAX, 120_000);
    for (room, c) in &live.rooms {
        assert_eq!(r.agent_only_turns(room), c.agent_only_turns, "{room}");
    }
    holds_at_least(&live, &r);
}

#[test]
fn a_room_held_by_a_tell_stays_held_after_the_teller_moves() {
    let mut w = world();
    let mut live = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let vich = id("vich");
    let taproom = RoomId::from("rusty_flagon_taproom");
    for d in ["w", "n", "e"] {
        run(&mut w, &mut live, &rt, &vich, d, 10);
    }
    let ada = w.login("ada", 20).unwrap();
    run(&mut w, &mut live, &rt, &ada, "e", 30);
    run(&mut w, &mut live, &rt, &ada, "say hi", 40);
    run(&mut w, &mut live, &rt, &ada, "w", 50);
    assert_eq!(live.agent_only_turns(&taproom), 0, "the human reset it");
    run(&mut w, &mut live, &rt, &vich, "tell torin hello", 60);
    assert_eq!(live.agent_only_turns(&taproom), 1);
    run(&mut w, &mut live, &rt, &vich, "w", 70);
    assert_eq!(live.agent_only_turns(&taproom), 2, "held");
    let w = reloaded(&w, 0);
    let mut r = Router::new(RouterConfig::default());
    r.replay(&w, &rt, 80, |_| u64::MAX, 120_000);
    assert_eq!(r.agent_only_turns(&taproom), 2, "still held after a boot");
    holds_at_least(&live, &r);
}

#[test]
fn a_tell_from_an_older_log_holds_every_room() {
    // A log written before the engine recorded the parties' rooms: the
    // fold cannot know where the tell counted, so every room holds.
    let mut w = world();
    let mut live = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let guard = id("guard_01");
    run(&mut w, &mut live, &rt, &guard, "tell marta psst", 100);
    run(&mut w, &mut live, &rt, &guard, "e", 200);
    let recent: Vec<Event> = w
        .logged()
        .cloned()
        .map(|mut e| {
            e.party_rooms.clear();
            e
        })
        .collect();
    let old = World::from_state(Arc::clone(w.pack()), w.snapshot(), recent);
    let mut r = Router::new(RouterConfig::default());
    r.replay(&old, &rt, 300, |_| u64::MAX, 120_000);
    holds_at_least(&live, &r);
    let max = RouterConfig::default().max_agent_only_turns;
    for room in ["town_square", "rusty_flagon_taproom", "dark_alley"] {
        let held = r.agent_only_turns(&RoomId::from(room));
        assert!(held >= max, "{room}: {held}");
    }
    // A tell a player perceived keeps the count, as it did live.
    let mut w = world();
    let mut live = Router::new(RouterConfig::default());
    let ada = w.login("ada", 0).unwrap();
    run(&mut w, &mut live, &rt, &guard, "tell ada psst", 100);
    let recent: Vec<Event> = w
        .logged()
        .cloned()
        .map(|mut e| {
            e.party_rooms.clear();
            e
        })
        .collect();
    let old = World::from_state(Arc::clone(w.pack()), w.snapshot(), recent);
    let mut r = Router::new(RouterConfig::default());
    r.replay(&old, &rt, 300, |_| u64::MAX, 120_000);
    holds_at_least(&live, &r);
    assert_eq!(r.agent_only_turns(&RoomId::from("dark_alley")), 0);
    assert!(w.entity(&ada).is_some());
}

// ------------------------------------------------------------ downed

/// `w` with `who` out cold at `hp` in its room.
fn knocked_out(w: &World, who: &str, hp: i32) -> World {
    let mut s = w.snapshot();
    if let Some(e) = s.entities.get_mut(&id(who)) {
        e.pools.hp = hp;
        e.unconscious = true;
    }
    let recent: Vec<Event> = w.logged().cloned().collect();
    let mut out = World::from_state(Arc::clone(w.pack()), s, recent);
    // A reload takes humans offline; bring them back.
    for h in w.state().entities.values().filter(|e| e.human && e.online) {
        out.bring_online(&h.id, 0);
    }
    out.drain_fresh();
    out
}

/// Takes and finishes every wake waiting.
fn clear(r: &mut Router) {
    for wake in r.due(u64::MAX) {
        r.finish(&wake.entity, 0);
    }
}

/// One recovery step a minute until someone comes to; their report.
fn tick_until_came_to(w: &mut World, from: u64) -> (u64, Vec<(EntityId, Seq)>) {
    (1..=20)
        .map(|m| from + m * 60_000)
        .find_map(|t| {
            let out = w.tick(t);
            (!out.came_to.is_empty()).then_some((t, out.came_to))
        })
        .unwrap()
}

#[test]
fn a_tell_to_an_entity_out_cold_is_held_and_rides_on_its_come_to_wake() {
    let mut w = world();
    let mut r = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let ada = w.login("ada", 0).unwrap();
    w.drain_fresh();
    let mut w = knocked_out(&w, "marta", -3);
    let n = run(&mut w, &mut r, &rt, &ada, "tell marta you there?", 1_000);
    assert_eq!(
        queued_for(&n, "marta").map(|q| q.0),
        Some(Priority::Immediate)
    );
    let out_cold = |e: &EntityId| e != &id("marta");
    assert!(
        r.due_where(10_000, out_cold).is_empty(),
        "held while out cold"
    );
    assert_eq!(r.pending().len(), 1, "kept, not dropped");
    let (t, came) = tick_until_came_to(&mut w, 1_000);
    let notes: Vec<RouteNote> = came.iter().map(|(e, s)| r.came_to(e, *s, t)).collect();
    assert!(matches!(
        notes[..],
        [RouteNote::Queued {
            priority: Priority::Immediate,
            ..
        }]
    ));
    // The line itself comes through the event feed too: no second wake.
    let n = feed(&mut w, &mut r, &rt, t);
    assert!(
        dropped_for(&n, "marta").is_some_and(|why| why.contains("already woken")),
        "{n:?}"
    );
    let wakes = r.due_where(t, |_| true);
    assert_eq!(wakes.len(), 1, "{wakes:?}");
    assert_eq!(wakes[0].reason, CAME_TO);
    assert_eq!(wakes[0].priority, Priority::Immediate);
    let tell = seqs_of(&w, EventKind::Tell)[0];
    assert_eq!(
        wakes[0].seqs,
        [tell, came[0].1],
        "the tell and the come-to line"
    );
    assert!(r.due(u64::MAX).is_empty(), "once");
}

#[test]
fn a_come_to_line_seen_first_wakes_the_entity_once() {
    // A heal in a command: the router learns of it from the line alone.
    let mut w = world();
    let mut r = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let ada = w.login("ada", 0).unwrap();
    run(&mut w, &mut r, &rt, &ada, "e", 0);
    clear(&mut r);
    let mut w = knocked_out(&w, "marta", 0);
    let (t, came) = tick_until_came_to(&mut w, 0);
    let n = feed(&mut w, &mut r, &rt, t);
    assert_eq!(
        queued_for(&n, "marta").map(|q| q.0),
        Some(Priority::Immediate)
    );
    assert!(matches!(
        r.came_to(&came[0].0, came[0].1, t),
        RouteNote::Dropped { .. }
    ));
    let wakes = r.due(t);
    assert_eq!(wakes.len(), 1);
    assert_eq!(
        (wakes[0].reason.as_str(), &wakes[0].seqs[..]),
        (CAME_TO, &[came[0].1][..])
    );
}

#[test]
fn bystanders_notice_someone_stir_and_a_heal_on_an_agent_wakes_it() {
    let mut w = world();
    let mut r = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let ada = w.login("ada", 0).unwrap();
    run(&mut w, &mut r, &rt, &ada, "e", 0);
    clear(&mut r);
    // Torin lies out cold in the Flagon, where Marta can see him stir.
    let mut snap = w.snapshot();
    let torin = snap.entities.get_mut(&id("torin")).unwrap();
    torin.room = RoomId::from("rusty_flagon_taproom");
    torin.pools.hp = 0;
    torin.unconscious = true;
    let bandage = snap.new_item(&mud_world::ProtoId::from("bandage"), w.pack());
    snap.attach(
        bandage,
        &mud_world::state::Holder::Entity {
            entity: id("torin"),
        },
    );
    let mut w = World::from_state(Arc::clone(w.pack()), snap, Vec::new());
    w.bring_online(&ada, 0);
    w.drain_fresh();
    let (t, _) = tick_until_came_to(&mut w, 0);
    let n = feed(&mut w, &mut r, &rt, t);
    assert_eq!(
        queued_for(&n, "marta").map(|q| q.0),
        Some(Priority::Ambient)
    );
    assert_eq!(
        queued_for(&n, "torin").map(|q| q.0),
        Some(Priority::Immediate)
    );
    clear(&mut r);
    // An agent's bandage on Marta (a Gear event that targets her) is a
    // room batch for her, not nothing.
    let n = run(
        &mut w,
        &mut r,
        &rt,
        &id("torin"),
        "use bandage on marta",
        t + 1_000,
    );
    assert_eq!(
        queued_for(&n, "marta").map(|q| q.0),
        Some(Priority::RoomBatch)
    );
}

#[test]
fn a_held_wake_given_back_merges_with_what_came_meanwhile() {
    let mut r = Router::new(RouterConfig::default());
    let marta = id("marta");
    r.force(&marta, Priority::Booked, "booked wake", 0);
    let mut taken = r.due(0);
    assert!(r.is_busy(&marta));
    taken[0].seqs = vec![Seq(4)];
    taken[0].macro_tick = true;
    r.force(&marta, Priority::Immediate, "combat by giant rat", 1);
    r.hold(taken.remove(0));
    assert!(!r.is_busy(&marta));
    let w = r.due(2);
    assert_eq!(w.len(), 1);
    assert_eq!(w[0].priority, Priority::Immediate);
    assert_eq!(w[0].reason, "combat by giant rat");
    assert!(w[0].macro_tick);
    assert_eq!(w[0].seqs, [Seq(4)]);
    // A resume given back waits as a resume.
    r.finish(&marta, 3);
    r.resume(
        &marta,
        Resume {
            wake_id: "w".into(),
            lines: vec!["say hi".into()],
            cursor: 0,
        },
    );
    let mut res = r.resumes_due(|_| true);
    r.hold(res.remove(0));
    assert_eq!(r.resumes_due(|_| true).len(), 1);
}

#[test]
fn replay_waives_the_age_limit_for_an_entity_held_out_cold() {
    let mut w = world();
    let mut live = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let ada = w.login("ada", 0).unwrap();
    run(&mut w, &mut live, &rt, &ada, "tell marta old news", 1_000);
    run(&mut w, &mut live, &rt, &ada, "tell torin old news", 1_000);
    let w = knocked_out(&reloaded(&w, 0), "marta", -5);
    let mut r = Router::new(RouterConfig::default());
    let notes = r.replay(&w, &rt, 10 * 60_000, |_| 0, 120_000);
    assert!(queued_for(&notes, "marta").is_some(), "held: still owed");
    assert!(queued_for(&notes, "torin").is_none(), "awake and too old");
}

fn booking(id: u64) -> crate::wakes::Booking {
    crate::wakes::Booking {
        id,
        due_ms: 0,
        note: format!("n{id}"),
    }
}

#[test]
fn a_booked_wake_does_not_reset_anti_chatter() {
    let mut w = world();
    let mut r = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let marta = id("marta");
    let room = RoomId::from("rusty_flagon_taproom");
    for (i, line) in ["say one", "say two", "say three"].iter().enumerate() {
        run(&mut w, &mut r, &rt, &marta, line, 1_000 * (i as u64 + 1));
    }
    let max = r.config().max_agent_only_turns;
    let spent = r.agent_only_turns(&room);
    assert!(
        spent >= max,
        "the room's agent-only turns are spent: {spent}"
    );
    r.deliver_booked(&marta, vec![booking(1)], 5_000);
    r.deliver_booked(&id("torin"), vec![booking(2)], 5_000);
    assert_eq!(
        r.agent_only_turns(&room),
        spent,
        "a booked wake is no human and no macro tick"
    );
    let taken = r.due(5_000);
    assert_eq!(taken.len(), 2);
    assert!(taken.iter().all(|wk| wk.priority == Priority::Booked));
    assert_eq!(r.agent_only_turns(&room), spent, "nor is taking it");
}

#[test]
fn a_booking_merges_into_a_pending_room_batch() {
    let mut w = world();
    let mut r = Router::new(RouterConfig::default());
    let rt = HashMap::new();
    let ada = w.login("ada", 0).unwrap();
    w.drain_fresh();
    run(&mut w, &mut r, &rt, &ada, "e", 0);
    let n = run(&mut w, &mut r, &rt, &ada, "say evening all", 1_000);
    let (priority, _) = queued_for(&n, "marta").unwrap();
    assert_eq!(priority, Priority::RoomBatch);
    let note = r.deliver_booked(&id("marta"), vec![booking(9)], 1_200);
    assert!(
        matches!(
            note,
            RouteNote::Queued {
                priority: Priority::RoomBatch,
                due_ms: 1_200,
                ..
            }
        ),
        "one wake, the room batch's, due now: {note:?}"
    );
    let taken = r.due(1_200);
    assert_eq!(taken.len(), 1, "one wake for marta");
    assert_eq!(taken[0].priority, Priority::RoomBatch);
    assert_eq!(taken[0].booked, vec![booking(9)], "it carries the booking");
    assert!(!taken[0].seqs.is_empty(), "and the speech");
    r.finish(&id("marta"), 1_300);
    assert!(
        r.pending().iter().all(|(e, _, _)| e != &id("marta")),
        "nothing else waits for marta"
    );
}

#[test]
fn a_claim_holds_only_an_idle_entity_and_starts_no_cooldown() {
    let mut r = Router::new(RouterConfig::default());
    let torin = id("torin");
    assert!(r.claim(&torin), "idle: claimed");
    assert!(r.is_busy(&torin));
    assert!(!r.claim(&torin), "claimed already");
    // A wake queued meanwhile waits for the release.
    r.force(&torin, Priority::Immediate, "test", 10);
    assert!(r.due_where(10, |_| true).is_empty(), "busy: nothing due");
    r.release(&torin);
    assert!(!r.is_busy(&torin));
    assert!(!r.claim(&torin), "a waiting wake: not idle");
    let due = r.due_where(10, |_| true);
    assert_eq!(due.len(), 1, "no cooldown from the claim");
    r.finish(&torin, 20);
    assert!(r.claim(&torin));
    r.release(&torin);
    assert!(r.is_idle(&torin));
}
