//! A fairer way out of a fight: the way you came in, a bare flee that
//! runs somewhere sensible, mobs that leave a body alone, and a grate that
//! locks itself again.

use std::sync::Arc;

use super::*;

const MINUTE: u64 = 60_000;

fn id(s: &str) -> EntityId {
    EntityId::from(s)
}

fn crypt() -> World {
    World::new(Arc::new(WorldPack::crypt_v01().unwrap()), 42)
}

fn strict() -> World {
    let mut pack = WorldPack::crypt_v01().unwrap();
    pack.combat.retreat_free = false;
    World::new(Arc::new(pack), 42)
}

fn ok(w: &mut World, who: &EntityId, line: &str) -> CommandResult {
    let r = w.execute(who, line, 1_000);
    assert!(r.ok, "{line:?} failed: {r:?}");
    r
}

fn err(w: &mut World, who: &EntityId, line: &str) -> ErrorCode {
    let r = w.execute(who, line, 1_000);
    assert!(!r.ok, "{line:?} should fail: {r:?}");
    r.error_code.unwrap()
}

fn place(w: &mut World, who: &EntityId, room: &str) {
    w.state.entities.get_mut(who).unwrap().room = RoomId::from(room);
}

fn room_of(w: &World, who: &EntityId) -> String {
    w.entity(who).unwrap().room.to_string()
}

/// The attacks in this command's events on `who`.
fn strikes_on(w: &World, r: &CommandResult, who: &EntityId) -> usize {
    w.logged()
        .filter(|e| r.events.contains(&e.seq))
        .filter(|e| e.kind == EventKind::Combat && e.verb.as_deref() == Some("attack"))
        .filter(|e| e.targets(who))
        .count()
}

/// Ada walks into the wine vault from the landing; the rat misses.
fn ada_in_the_vault(w: &mut World) -> EntityId {
    let ada = w.login("ada", 1).unwrap();
    place(w, &ada, "cellar_landing");
    w.fix_dice([6, 6, 6]);
    let r = ok(w, &ada, "w");
    assert_eq!(strikes_on(w, &r, &ada), 1, "the strike on entering stays");
    assert_eq!(
        w.entity(&ada).unwrap().came_from,
        Some(RoomId::from("cellar_landing"))
    );
    ada
}

#[test]
fn a_bare_flee_from_the_vault_after_arriving_from_the_landing_goes_back() {
    let mut w = crypt();
    let ada = ada_in_the_vault(&mut w);
    let r = ok(&mut w, &ada, "flee");
    assert_eq!(room_of(&w, &ada), "cellar_landing", "east, not south");
    assert!(r.narrative.starts_with("You fall back the way you came."));
    assert!(r.resolution.get("flee_roll").is_none(), "no roll");
    assert_eq!(strikes_on(&w, &r, &ada), 0, "no strike");
}

/// The owner's log: Ada comes in through the cistern and takes the
/// warden's strike in the ossuary, breaks away northwest into the wine
/// vault and takes the rat's. The way she came in is now the warden's
/// lair, so a bare flee runs east to the empty landing, not back south.
#[test]
fn the_logs_path_through_the_ossuary_flees_to_the_landing_not_the_warden() {
    let mut w = crypt();
    let ada = w.login("ada", 1).unwrap();
    place(&mut w, &ada, "flooded_cistern");
    w.fix_dice([6, 6, 6]);
    let r = ok(&mut w, &ada, "s");
    assert_eq!(strikes_on(&w, &r, &ada), 1, "the warden strikes on entry");
    // DX 10: 1+1+1 breaks away; the rat then misses with 6s.
    w.fix_dice([1, 1, 1, 6, 6, 6]);
    let r = ok(&mut w, &ada, "flee nw");
    assert_eq!(room_of(&w, &ada), "wine_vault");
    assert_eq!(strikes_on(&w, &r, &ada), 1, "the rat strikes on entry");
    assert_eq!(
        w.entity(&ada).unwrap().came_from,
        Some(RoomId::from("ossuary_crypt"))
    );
    // Walking east still needs flee, and the hint does not send her back
    // to the warden.
    let r = w.execute(&ada, "e", 1_000);
    assert_eq!(r.error_code, Some(ErrorCode::InCombat));
    assert!(!r.narrative.contains("the way you came"), "{r:?}");
    w.fix_dice([1, 1, 1]);
    let r = ok(&mut w, &ada, "flee");
    assert_eq!(room_of(&w, &ada), "cellar_landing", "east, not south");
    assert!(r.narrative.starts_with("You break away!"), "{r:?}");
    assert_eq!(strikes_on(&w, &r, &ada), 0);
}

/// A retreat into a lair does not make a bare flee bounce: Ada falls back
/// from the warden into the vault, and the next bare flee leaves the rat
/// for the empty landing, not for the warden she just left.
#[test]
fn a_bare_flee_never_bounces_between_two_lairs() {
    let mut w = crypt();
    let ada = w.login("ada", 1).unwrap();
    place(&mut w, &ada, "ossuary_crypt");
    w.state.entities.get_mut(&ada).unwrap().came_from = Some(RoomId::from("wine_vault"));
    w.fix_dice([6, 6, 6]);
    let r = ok(&mut w, &ada, "nw");
    assert_eq!(room_of(&w, &ada), "wine_vault");
    assert_eq!(strikes_on(&w, &r, &ada), 1, "the rat strikes on entry");
    w.fix_dice([1, 1, 1]);
    let r = ok(&mut w, &ada, "flee");
    assert_eq!(room_of(&w, &ada), "cellar_landing", "{r:?}");
    assert_eq!(strikes_on(&w, &r, &ada), 0);
}

#[test]
fn a_bare_flee_takes_the_way_back_into_a_lair_only_when_every_room_is_hostile() {
    let mut w = crypt();
    let ada = w.login("ada", 1).unwrap();
    place(&mut w, &ada, "wine_vault");
    w.state.entities.get_mut(&ada).unwrap().came_from = Some(RoomId::from("rat_warren"));
    place(&mut w, &id("giant_rat_2"), "rat_warren");
    let torin = id("torin");
    place(&mut w, &torin, "cellar_landing");
    w.state.entities.get_mut(&torin).unwrap().hostile = true;
    w.fix_dice([6, 6, 6]);
    let r = ok(&mut w, &ada, "flee");
    assert_eq!(
        room_of(&w, &ada),
        "rat_warren",
        "the way back, not the old order"
    );
    assert!(r.narrative.starts_with("You fall back the way you came."));
}

#[test]
fn walking_back_the_way_you_came_is_a_move_and_other_ways_need_flee() {
    let mut w = crypt();
    let ada = ada_in_the_vault(&mut w);
    assert_eq!(err(&mut w, &ada, "w"), ErrorCode::InCombat);
    let r = w.execute(&ada, "s", 1_000);
    assert_eq!(r.error_code, Some(ErrorCode::InCombat));
    assert!(
        r.narrative.contains("or go back east, the way you came"),
        "{r:?}"
    );
    let r = ok(&mut w, &ada, "e");
    assert_eq!(strikes_on(&w, &r, &ada), 0);
    assert_eq!(room_of(&w, &ada), "cellar_landing");
    assert_eq!(
        w.entity(&ada).unwrap().came_from,
        Some(RoomId::from("wine_vault"))
    );
}

#[test]
fn a_flee_by_another_way_still_rolls_and_draws_strikes_when_it_fails() {
    let mut w = crypt();
    let ada = ada_in_the_vault(&mut w);
    // DX 10: 6+6+5 fails; the rat then misses with 6s.
    w.fix_dice([6, 6, 5, 6, 6, 6]);
    let r = ok(&mut w, &ada, "flee s");
    assert!(r.narrative.contains("cut off"), "{r:?}");
    assert_eq!(strikes_on(&w, &r, &ada), 1);
    assert_eq!(room_of(&w, &ada), "wine_vault");
}

#[test]
fn without_retreat_free_a_bare_flee_still_runs_back_but_rolls() {
    let mut w = strict();
    let ada = ada_in_the_vault(&mut w);
    assert_eq!(err(&mut w, &ada, "e"), ErrorCode::InCombat);
    w.fix_dice([1, 1, 1]);
    let r = ok(&mut w, &ada, "flee");
    assert!(r.narrative.contains("break away"));
    assert!(r.resolution.get("flee_roll").is_some());
    assert_eq!(room_of(&w, &ada), "cellar_landing");
}

#[test]
fn a_bare_flee_with_no_way_back_prefers_a_room_with_no_hostile() {
    let mut w = crypt();
    let ada = w.login("ada", 1).unwrap();
    place(&mut w, &ada, "wine_vault");
    assert_eq!(w.entity(&ada).unwrap().came_from, None);
    // South is first in exit order, and the warden is there.
    w.fix_dice([1, 1, 1]);
    ok(&mut w, &ada, "flee");
    assert_eq!(room_of(&w, &ada), "cellar_landing");
}

#[test]
fn a_bare_flee_falls_back_to_the_first_open_exit_when_every_room_is_hostile() {
    let mut w = crypt();
    let ada = w.login("ada", 1).unwrap();
    place(&mut w, &ada, "wine_vault");
    place(&mut w, &id("giant_rat_2"), "cellar_landing");
    let torin = id("torin");
    place(&mut w, &torin, "rat_warren");
    w.state.entities.get_mut(&torin).unwrap().hostile = true;
    w.fix_dice([1, 1, 1]);
    ok(&mut w, &ada, "flee");
    assert_eq!(room_of(&w, &ada), "ossuary_crypt", "the old order: south");
}

#[test]
fn a_locked_exit_is_never_a_way_to_flee() {
    let mut w = crypt();
    let ada = w.login("ada", 1).unwrap();
    // Ada came down through the grate, and it locked behind her.
    place(&mut w, &ada, "dark_alley");
    w.state.entities.get_mut(&ada).unwrap().came_from = Some(RoomId::from("cellar_landing"));
    let r = ok(&mut w, &ada, "flee");
    assert_eq!(room_of(&w, &ada), "town_square", "{r:?}");
}

#[test]
fn the_second_mob_of_a_round_leaves_a_collapsed_body_alone() {
    let mut w = crypt();
    let ada = w.login("ada", 1).unwrap();
    place(&mut w, &ada, "cellar_landing");
    place(&mut w, &id("giant_rat_2"), "wine_vault");
    w.state.entities.get_mut(&ada).unwrap().pools.hp = 1;
    // The first rat: attack 6 hits, Ada's dodge 18 fails, damage 6-2 = 4
    // cut = 6 injury, to -5; her consciousness roll of 18 fails.
    w.fix_dice([2, 2, 2, 6, 6, 6, 6, 6, 6, 6]);
    let r = ok(&mut w, &ada, "w");
    let e = w.entity(&ada).unwrap();
    assert!(e.unconscious, "{r:?}");
    assert_eq!(e.pools.hp, -5);
    assert_eq!(r.resolution["ambush"].as_array().map(Vec::len), Some(1));
    assert_eq!(strikes_on(&w, &r, &ada), 1, "one strike, not two");
}

#[test]
fn came_from_round_trips_and_an_older_save_has_none() {
    let mut w = crypt();
    let ada = ada_in_the_vault(&mut w);
    let snap = w.snapshot();
    let json = serde_json::to_value(&snap).unwrap();
    assert_eq!(json["entities"]["pc_ada"]["came_from"], "cellar_landing");
    let back: WorldState = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(back, snap);
    let mut old = json;
    old["entities"]["pc_ada"]
        .as_object_mut()
        .unwrap()
        .remove("came_from");
    let back: WorldState = serde_json::from_value(old).unwrap();
    assert_eq!(back.entities[&ada].came_from, None);
}

#[test]
fn a_came_from_the_pack_no_longer_has_is_forgotten_on_load() {
    let mut w = crypt();
    let ada = ada_in_the_vault(&mut w);
    let mut snap = w.snapshot();
    snap.entities.get_mut(&ada).unwrap().came_from = Some(RoomId::from("old_cellar"));
    let w = World::from_state(Arc::clone(&w.pack), snap, Vec::new());
    assert_eq!(w.entity(&ada).unwrap().came_from, None);
    assert_eq!(room_of(&w, &ada), "wine_vault");
}

#[test]
fn came_from_never_shows_in_a_view() {
    let mut w = crypt();
    let ada = ada_in_the_vault(&mut w);
    let views = [
        w.look_view(&ada).to_string(),
        w.sheet_view(&ada).to_string(),
        w.who().1.to_string(),
    ];
    for v in views {
        assert!(!v.contains("came_from"), "{v}");
    }
}

#[test]
fn a_death_respawn_forgets_where_you_came_from() {
    let mut w = crypt();
    let ada = ada_in_the_vault(&mut w);
    let mut ctx = Ctx {
        now: 1_000,
        ..Ctx::default()
    };
    w.wound(&mut ctx, &ada, 1_000, &id("giant_rat_1"));
    assert_eq!(room_of(&w, &ada), "town_square");
    assert_eq!(w.entity(&ada).unwrap().came_from, None);
}

#[test]
fn the_grate_locks_itself_again_after_its_minutes() {
    let mut w = crypt();
    let ada = w.login("ada", 1).unwrap();
    let pack = Arc::clone(&w.pack);
    let key = w.state.new_item(&ProtoId::from("grate_key"), &pack);
    w.state.attach(
        key,
        &Holder::Entity {
            entity: ada.clone(),
        },
    );
    ok(&mut w, &ada, "s");
    let opened_at = 5 * MINUTE;
    let r = w.execute(&ada, "use key", opened_at);
    assert!(r.ok, "{r:?}");
    let alley = RoomId::from("dark_alley");
    assert_eq!(
        w.state.rooms[&alley].unlocked_at.get(&Direction::D),
        Some(&opened_at)
    );
    let out = w.tick(opened_at + 29 * MINUTE);
    assert!(
        !out.changes
            .iter()
            .any(|c| matches!(c, StateChange::Locked { .. }))
    );
    assert!(
        !w.look_view(&ada)["exits"]
            .to_string()
            .contains("\"locked\":true")
    );
    let out = w.tick(opened_at + 30 * MINUTE);
    assert!(out.changes.contains(&StateChange::Locked {
        room: alley.clone(),
        dir: Direction::D
    }));
    let line = w
        .logged()
        .find(|e| e.tags.iter().any(|t| t == "relock"))
        .unwrap();
    assert_eq!(
        line.narrative,
        "The way down swings shut, and its lock catches."
    );
    assert!(line.recipients.contains(&ada));
    assert!(w.state.rooms[&alley].unlocked_at.is_empty());
    assert!(
        w.look_view(&ada)["exits"]
            .to_string()
            .contains("\"locked\":true")
    );
    // From below, the way up stays open.
    place(&mut w, &ada, "cellar_landing");
    ok(&mut w, &ada, "u");
}

#[test]
fn an_exit_opened_in_an_older_save_starts_its_clock_at_the_first_tick() {
    let mut w = crypt();
    let alley = RoomId::from("dark_alley");
    w.state
        .rooms
        .get_mut(&alley)
        .unwrap()
        .unlocked
        .insert(Direction::D);
    w.tick(10 * MINUTE);
    assert_eq!(
        w.state.rooms[&alley].unlocked_at.get(&Direction::D),
        Some(&(10 * MINUTE))
    );
    w.tick(39 * MINUTE);
    assert!(w.state.rooms[&alley].unlocked.contains(&Direction::D));
    w.tick(40 * MINUTE);
    assert!(!w.state.rooms[&alley].unlocked.contains(&Direction::D));
}

#[test]
fn a_lock_with_no_relock_stays_open() {
    let mut w = {
        let mut pack = WorldPack::crypt_v01().unwrap();
        for lock in pack
            .rooms
            .get_mut(&RoomId::from("dark_alley"))
            .unwrap()
            .locks
            .values_mut()
        {
            lock.relock_minutes = None;
        }
        World::new(Arc::new(pack), 42)
    };
    let alley = RoomId::from("dark_alley");
    w.state
        .rooms
        .get_mut(&alley)
        .unwrap()
        .unlocked
        .insert(Direction::D);
    w.tick(500 * MINUTE);
    assert!(w.state.rooms[&alley].unlocked.contains(&Direction::D));
}

#[test]
fn help_says_how_to_leave_a_fight() {
    let mut w = crypt();
    let ada = w.login("ada", 1).unwrap();
    let help = ok(&mut w, &ada, "help").narrative;
    assert!(
        help.contains(
            "fighting: With a hostile in the room, you may walk back out the way you came in"
        ),
        "{help}"
    );
    assert!(
        w.pack
            .body_rules()
            .contains("A bare flee runs back the way you came when no hostile waits there")
    );
    let mut s = strict();
    let bo = s.login("bo", 1).unwrap();
    let help = ok(&mut s, &bo, "help").narrative;
    assert!(help.contains("every way out needs flee"), "{help}");
}
