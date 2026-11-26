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
    let r = ok(&mut w, &ada, "use bandage");
    assert!(r.resolution["healed"].as_i64().unwrap() >= 0);
    assert_eq!(strikes_on(&w, &r, &ada), 1, "the rat takes its turn");
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
    let deltas: Vec<i32> = r
        .state_changes
        .iter()
        .filter_map(|c| match c {
            StateChange::Hp { entity, delta, .. } if *entity == ada => Some(*delta),
            _ => None,
        })
        .collect();
    assert_eq!(deltas.len(), 2, "{deltas:?}");
    assert_eq!(deltas[0], 8, "the draught heals first");
    assert!(deltas[1] < 0, "then the bite lands: {deltas:?}");
    let hp = w.entity(&ada).unwrap().pools.hp;
    assert_eq!(hp, 10 + deltas[1]);
    // The room sees the same order.
    let kinds: Vec<EventKind> = w
        .logged()
        .filter(|e| r.events.contains(&e.seq))
        .map(|e| e.kind)
        .collect();
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
