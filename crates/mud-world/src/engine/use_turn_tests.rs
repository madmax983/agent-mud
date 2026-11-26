//! A turn in a fight: with `[combat] use_draws_strike`, a `use` that acts
//! beside a hostile draws the same mob round as `attack` and `defend`
//! (ADR 0017).

use std::sync::Arc;

use super::*;

fn crypt() -> World {
    World::new(Arc::new(WorldPack::crypt_v01().unwrap()), 42)
}

/// crypt-v01 with a free `use`, as a pack without the key has.
fn free_use() -> World {
    let mut pack = WorldPack::crypt_v01().unwrap();
    pack.combat.use_draws_strike = false;
    World::new(Arc::new(pack), 42)
}

fn ok(w: &mut World, who: &EntityId, line: &str) -> CommandResult {
    let r = w.execute(who, line, 1_000);
    assert!(r.ok, "{line:?} failed: {r:?}");
    r
}

fn place(w: &mut World, who: &EntityId, room: &str) {
    w.state.entities.get_mut(who).unwrap().room = RoomId::from(room);
}

fn give_item(w: &mut World, who: &EntityId, proto: &str) -> ItemId {
    let pack = Arc::clone(&w.pack);
    let item = w.state.new_item(&ProtoId::from(proto), &pack);
    w.state.attach(
        item,
        &Holder::Entity {
            entity: who.clone(),
        },
    );
    item
}

fn hurt(w: &mut World, who: &EntityId, hp: i32) {
    w.state.entities.get_mut(who).unwrap().pools.hp = hp;
}

/// The attacks in this command's events on `who`.
fn strikes_on(w: &World, r: &CommandResult, who: &EntityId) -> usize {
    w.logged()
        .filter(|e| r.events.contains(&e.seq))
        .filter(|e| e.kind == EventKind::Combat && e.verb.as_deref() == Some("attack"))
        .filter(|e| e.targets(who))
        .count()
}

/// The HP changes of this command on `who`, in order.
fn hp_deltas(r: &CommandResult, who: &EntityId) -> Vec<i32> {
    r.state_changes
        .iter()
        .filter_map(|c| match c {
            StateChange::Hp { entity, delta, .. } if entity == who => Some(*delta),
            _ => None,
        })
        .collect()
}

/// The kinds of this command's events, in order.
fn kinds(w: &World, r: &CommandResult) -> Vec<EventKind> {
    w.logged()
        .filter(|e| r.events.contains(&e.seq))
        .map(|e| e.kind)
        .collect()
}

/// Ada walks into the wine vault from the landing; the rat misses.
fn ada_beside_the_rat(w: &mut World) -> EntityId {
    let ada = w.login("ada", 1).unwrap();
    place(w, &ada, "cellar_landing");
    w.fix_dice([6, 6, 6]);
    let r = ok(w, &ada, "w");
    assert_eq!(strikes_on(w, &r, &ada), 1, "the strike on entering stays");
    ada
}

#[test]
fn crypt_sets_the_key_and_a_pack_without_it_keeps_a_free_use() {
    assert!(WorldPack::crypt_v01().unwrap().combat.use_draws_strike);
    assert!(!crate::pack::CombatRules::default().use_draws_strike);
    let rules: crate::pack::CombatRules = toml::from_str("retreat_free = true").unwrap();
    assert!(!rules.use_draws_strike, "unset means false");
}

#[test]
fn a_bandage_used_beside_a_hostile_rat_draws_its_strike() {
    let mut w = crypt();
    let ada = ada_beside_the_rat(&mut w);
    hurt(&mut w, &ada, 4);
    // 1d-1 on a 4 heals 3; first aid fails on an 18; the rat misses on an 18.
    w.fix_dice([4, 6, 6, 6, 6, 6, 6]);
    let r = ok(&mut w, &ada, "use bandage");
    assert_eq!(r.resolution["healed"], 3);
    assert_eq!(r.resolution["hp"], 7);
    assert_eq!(w.entity(&ada).unwrap().pools.hp, 7);
    assert_eq!(hp_deltas(&r, &ada), vec![3], "the heal lands");
    assert_eq!(strikes_on(&w, &r, &ada), 1, "the rat takes its turn");
    let kinds = kinds(&w, &r);
    let gear = kinds.iter().position(|k| *k == EventKind::Gear).unwrap();
    let bite = kinds.iter().position(|k| *k == EventKind::Combat).unwrap();
    assert!(gear < bite, "the heal comes before the strike: {kinds:?}");
    assert_eq!(r.resolution["attacks"].as_array().unwrap().len(), 1);
    assert!(
        r.narrative.starts_with("You use the linen bandage."),
        "{}",
        r.narrative
    );
    assert!(r.narrative.contains("giant rat"), "{}", r.narrative);
}

#[test]
fn with_no_hostile_in_the_room_a_bandage_draws_nothing() {
    let mut w = crypt();
    let ada = w.login("ada", 1).unwrap();
    place(&mut w, &ada, "cellar_landing");
    hurt(&mut w, &ada, 4);
    let r = ok(&mut w, &ada, "use bandage");
    assert_eq!(strikes_on(&w, &r, &ada), 0);
    assert_eq!(r.resolution["attacks"], json!([]));
    assert!(!r.narrative.contains('\n'), "{}", r.narrative);
}

#[test]
fn with_the_key_off_a_bandage_beside_the_rat_is_free() {
    let mut w = free_use();
    let ada = ada_beside_the_rat(&mut w);
    hurt(&mut w, &ada, 4);
    let r = ok(&mut w, &ada, "use bandage");
    assert_eq!(strikes_on(&w, &r, &ada), 0);
    assert!(r.resolution.get("attacks").is_none(), "{r:?}");
}

/// The draught heals first, then the rat bites: 2d of 6s heals the 8
/// missing HP, the rat hits on a 9 and Ada fails her defense on an 18.
#[test]
fn a_draught_heals_and_then_the_strike_lands() {
    let mut w = crypt();
    let ada = ada_beside_the_rat(&mut w);
    give_item(&mut w, &ada, "healing_draught");
    hurt(&mut w, &ada, 2);
    w.fix_dice([6, 6, 3, 3, 3, 6, 6, 6, 6]);
    let r = ok(&mut w, &ada, "use draught");
    let deltas = hp_deltas(&r, &ada);
    assert_eq!(deltas.len(), 2, "{deltas:?}");
    assert_eq!(deltas[0], 8, "the draught heals first");
    assert!(deltas[1] < 0, "then the bite lands: {deltas:?}");
    let hp = w.entity(&ada).unwrap().pools.hp;
    assert_eq!(hp, 10 + deltas[1]);
    assert_eq!(r.resolution["hp"], hp, "hp is the HP after the turn");
    // The room sees the same order.
    let kinds = kinds(&w, &r);
    let gear = kinds.iter().position(|k| *k == EventKind::Gear).unwrap();
    let bite = kinds.iter().position(|k| *k == EventKind::Combat).unwrap();
    assert!(gear < bite, "{kinds:?}");
    let lines: Vec<&str> = r.narrative.lines().collect();
    assert!(
        lines[0].starts_with("You use the healing draught."),
        "{lines:?}"
    );
    assert!(lines[1].contains("hits"), "{lines:?}");
}

#[test]
fn a_purse_opened_beside_the_rat_draws_its_strike() {
    let mut w = crypt();
    let ada = ada_beside_the_rat(&mut w);
    give_item(&mut w, &ada, "old_coin_pouch");
    let r = ok(&mut w, &ada, "use pouch");
    assert_eq!(strikes_on(&w, &r, &ada), 1);
}

#[test]
fn putting_on_gear_or_looking_in_a_box_beside_the_rat_stays_free() {
    let mut w = crypt();
    let ada = ada_beside_the_rat(&mut w);
    give_item(&mut w, &ada, "shortsword");
    let r = ok(&mut w, &ada, "use shortsword");
    assert_eq!(strikes_on(&w, &r, &ada), 0, "use on gear is equip");
    let pack = Arc::clone(&w.pack);
    let bx = w.state.new_item(&ProtoId::from("rusted_lockbox"), &pack);
    w.state.items.get_mut(&bx).unwrap().locked = false;
    w.state.attach(
        bx,
        &Holder::Room {
            room: RoomId::from("wine_vault"),
        },
    );
    let r = ok(&mut w, &ada, "use lockbox");
    assert!(r.narrative.contains("is empty"), "{}", r.narrative);
    assert_eq!(strikes_on(&w, &r, &ada), 0, "a look takes no turn");
}

#[test]
fn a_refused_use_beside_the_rat_takes_no_turn() {
    let mut w = crypt();
    let ada = ada_beside_the_rat(&mut w);
    let r = w.execute(&ada, "use bandage on nobody", 1_000);
    assert_eq!(r.error_code, Some(ErrorCode::NoSuchTarget));
    assert_eq!(strikes_on(&w, &r, &ada), 0);
}

/// Look, say and sheet never draw a strike, with the key on.
#[test]
fn look_say_and_sheet_beside_the_rat_stay_free() {
    let mut w = crypt();
    let ada = ada_beside_the_rat(&mut w);
    for line in ["look", "say easy now", "sheet", "inventory", "examine rat"] {
        let r = ok(&mut w, &ada, line);
        assert_eq!(strikes_on(&w, &r, &ada), 0, "{line}");
    }
}

#[test]
fn help_and_the_agent_prompt_say_use_takes_a_turn_only_when_it_does() {
    let turn = "With a hostile in the room, use takes your turn as attack and defend do";
    let mut w = crypt();
    let ada = w.login("ada", 1).unwrap();
    let help = ok(&mut w, &ada, "help").narrative;
    assert!(help.contains(turn), "{help}");
    assert!(help.contains("Looking into a container or putting on gear stays free"));
    assert!(w.pack.body_rules().contains(turn));
    let mut f = free_use();
    let bo = f.login("bo", 1).unwrap();
    let help = ok(&mut f, &bo, "help").narrative;
    assert!(!help.contains("use takes your turn"), "{help}");
    assert!(!f.pack.body_rules().contains("use takes your turn"));
}

/// crypt-v01 with the grate lock on the vault's way back east, so a key
/// has a lock to open beside the rat.
fn locked_vault() -> World {
    let mut pack = WorldPack::crypt_v01().unwrap();
    pack.rooms
        .get_mut(&RoomId::from("wine_vault"))
        .unwrap()
        .locks
        .insert(
            Direction::E,
            crate::pack::LockDef {
                lock: "alley_grate".into(),
                pick: 0,
                describe: None,
                relock_minutes: None,
            },
        );
    World::new(Arc::new(pack), 42)
}

#[test]
fn a_key_used_beside_the_rat_draws_its_strike() {
    let mut w = locked_vault();
    let ada = ada_beside_the_rat(&mut w);
    give_item(&mut w, &ada, "grate_key");
    let r = ok(&mut w, &ada, "use key");
    assert!(
        r.narrative
            .starts_with("The key turns. The way east is open."),
        "{}",
        r.narrative
    );
    assert_eq!(strikes_on(&w, &r, &ada), 1, "the rat takes its turn");
    assert_eq!(r.resolution["attacks"].as_array().unwrap().len(), 1);
}

#[test]
fn a_bandage_used_on_bo_beside_the_rat_draws_the_strike_on_ada() {
    let mut w = crypt();
    let ada = ada_beside_the_rat(&mut w);
    let bo = w.login("bo", 1).unwrap();
    place(&mut w, &bo, "wine_vault");
    hurt(&mut w, &bo, 4);
    // 1d-1 on a 4 heals 3; first aid fails on an 18; the rat misses on an 18.
    w.fix_dice([4, 6, 6, 6, 6, 6, 6]);
    let r = ok(&mut w, &ada, "use bandage on bo");
    assert_eq!(r.resolution["healed"], 3);
    assert_eq!(r.resolution["hp"], 7, "hp is Bo's");
    assert_eq!(strikes_on(&w, &r, &ada), 1, "the user's turn is taken");
    assert_eq!(strikes_on(&w, &r, &bo), 0, "the one healed is not struck");
    assert_eq!(r.resolution["attacks"].as_array().unwrap().len(), 1);
}

/// A heal on someone at full HP is refused before it rolls, so it keeps
/// the item and takes no turn.
#[test]
fn a_heal_on_someone_unhurt_is_refused_and_takes_no_turn() {
    let mut w = crypt();
    let ada = ada_beside_the_rat(&mut w);
    let bo = w.login("bo", 1).unwrap();
    place(&mut w, &bo, "wine_vault");
    let full = w.entity(&ada).unwrap().pools.hp_max;
    assert_eq!(w.entity(&ada).unwrap().pools.hp, full);
    let r = w.execute(&ada, "use bandage", 1_000);
    assert_eq!(r.error_code, Some(ErrorCode::CannotUse), "{r:?}");
    assert!(r.narrative.contains("you are not hurt"), "{}", r.narrative);
    assert_eq!(strikes_on(&w, &r, &ada), 0);
    let r = w.execute(&ada, "use bandage on bo", 1_000);
    assert_eq!(r.error_code, Some(ErrorCode::CannotUse), "{r:?}");
    assert!(
        r.narrative.to_lowercase().contains("bo is not hurt"),
        "{}",
        r.narrative
    );
    assert_eq!(strikes_on(&w, &r, &ada), 0);
    let r = ok(&mut w, &ada, "inventory");
    assert!(r.narrative.contains("linen bandage"), "{}", r.narrative);
}

/// The loop out of a fight and back: `e`, `use draught`, `w`. Walking
/// out is free and the heal outside is free, so the loop costs the strike
/// on walking back in, once, whatever the number of heals.
#[test]
fn healing_outside_and_walking_back_in_costs_the_strike_on_entering() {
    let mut w = crypt();
    let ada = ada_beside_the_rat(&mut w);
    let r = ok(&mut w, &ada, "e");
    assert_eq!(strikes_on(&w, &r, &ada), 0, "the retreat is free");
    give_item(&mut w, &ada, "healing_draught");
    hurt(&mut w, &ada, 2);
    w.fix_dice([3, 3]);
    let r = ok(&mut w, &ada, "use draught");
    assert_eq!(strikes_on(&w, &r, &ada), 0, "no hostile on the landing");
    w.fix_dice([6, 6, 6]);
    let r = ok(&mut w, &ada, "w");
    assert_eq!(strikes_on(&w, &r, &ada), 1, "the rat strikes on entering");
}

/// With `[combat] foe_memory_minutes`, hiding does not shake a hostile
/// you fought: it sees you walk back in and strikes. A stranger who walks
/// in hidden is not seen, and the memory fades after the window.
#[test]
fn a_hostile_you_fought_sees_through_hiding_on_your_way_back() {
    let mut w = crypt();
    assert_eq!(w.pack.combat.foe_memory_minutes, Some(5));
    let ada = ada_beside_the_rat(&mut w);
    ok(&mut w, &ada, "e");
    w.state.entities.get_mut(&ada).unwrap().hidden = true;
    w.fix_dice([6, 6, 6]);
    let r = ok(&mut w, &ada, "w");
    assert_eq!(strikes_on(&w, &r, &ada), 1, "the rat saw her leave");
    assert!(!w.entity(&ada).unwrap().hidden, "she is seen");
    assert!(
        r.narrative.starts_with("The giant rat saw you go"),
        "{}",
        r.narrative
    );
    assert!(
        kinds(&w, &r).contains(&EventKind::Enter),
        "the room sees her come in"
    );
    // A stranger the rat never fought slips in unseen.
    let bo = w.login("bo", 1).unwrap();
    place(&mut w, &bo, "cellar_landing");
    w.state.entities.get_mut(&bo).unwrap().hidden = true;
    let r = ok(&mut w, &bo, "w");
    assert_eq!(strikes_on(&w, &r, &bo), 0);
    assert!(w.entity(&bo).unwrap().hidden);
    // Past the window, the rat has forgotten Ada.
    ok(&mut w, &ada, "e");
    w.state.entities.get_mut(&ada).unwrap().hidden = true;
    let r = w.execute(&ada, "w", 1_000 + 5 * 60_000);
    assert!(r.ok, "{r:?}");
    assert_eq!(strikes_on(&w, &r, &ada), 0, "the memory fades");
}

#[test]
fn without_foe_memory_a_hidden_return_draws_no_strike() {
    let mut pack = WorldPack::crypt_v01().unwrap();
    pack.combat.foe_memory_minutes = None;
    let mut w = World::new(Arc::new(pack), 42);
    let ada = ada_beside_the_rat(&mut w);
    ok(&mut w, &ada, "e");
    w.state.entities.get_mut(&ada).unwrap().hidden = true;
    let r = ok(&mut w, &ada, "w");
    assert_eq!(strikes_on(&w, &r, &ada), 0);
    assert!(w.entity(&ada).unwrap().hidden);
    assert!(!w.pack.body_rules().contains("Hiding does not shake"));
}

#[test]
fn help_says_a_foe_you_fought_sees_through_hiding() {
    let mut w = crypt();
    let ada = w.login("ada", 1).unwrap();
    let help = ok(&mut w, &ada, "help").narrative;
    let line = "Hiding does not shake a hostile you fought in the last 5 minutes";
    assert!(help.contains(line), "{help}");
    assert!(w.pack.body_rules().contains(line));
}
