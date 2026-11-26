//! A respawned mob comes back with its kit (`[cast.x] respawn_kit`, ADR
//! 0012): made new, ledgered, and the old kit stays where it fell.

use std::sync::Arc;

use super::*;
use crate::ids::ShopId;
use crate::rewards::RESPAWN_POOL;

const MINUTE: u64 = 60_000;
const KILLED_AT: u64 = 10_000;

fn id(s: &str) -> EntityId {
    EntityId::from(s)
}

fn crypt() -> WorldPack {
    WorldPack::crypt_v01().unwrap()
}

fn proto_of(w: &World, item: ItemId) -> ProtoId {
    w.state.items[&item].proto.clone()
}

/// The items of `proto` that `who` carries.
fn carried(w: &World, who: &str, proto: &str) -> Vec<ItemId> {
    w.entity(&id(who))
        .unwrap()
        .inventory
        .iter()
        .copied()
        .filter(|i| proto_of(w, *i) == ProtoId::from(proto))
        .collect()
}

/// The items of `proto` on the floor of `room`.
fn on_floor(w: &World, room: &str, proto: &str) -> Vec<ItemId> {
    w.state.rooms[&RoomId::from(room)]
        .items
        .iter()
        .copied()
        .filter(|i| proto_of(w, *i) == ProtoId::from(proto))
        .collect()
}

/// A world where ada stands in `room` and has just killed `mob` there,
/// through `World::execute`: the mob is past saving, and the dice give a
/// critical hit.
fn killed(pack: WorldPack, mob: &str, room: &str) -> (World, EntityId) {
    let mut w = World::new(Arc::new(pack), 42);
    let ada = w.login("ada", 1).unwrap();
    kill(&mut w, &ada, mob, room, KILLED_AT);
    (w, ada)
}

/// `who` walks into `room` and kills `mob` there at `now`.
fn kill(w: &mut World, who: &EntityId, mob: &str, room: &str, now: u64) {
    w.state.entities.get_mut(who).unwrap().room = RoomId::from(room);
    let m = w.state.entities.get_mut(&id(mob)).unwrap();
    m.pools.hp = -6 * m.pools.hp_max;
    let name = m.aliases.first().cloned().unwrap_or_else(|| m.name.clone());
    w.fix_dice([1, 1, 1, 6, 6, 6]);
    let r = w.execute(who, &format!("attack {name}"), now);
    assert!(r.ok, "{r:?}");
    assert!(
        w.entity(&id(mob)).unwrap().dead_until_ms.is_some(),
        "{mob} dies"
    );
    w.drain_fresh();
    w.drain_audit();
}

/// The world `w` saved and loaded again under `pack`, with its humans
/// back online.
fn reload(w: &World, pack: WorldPack) -> World {
    let json = serde_json::to_string(&w.snapshot()).unwrap();
    let saved: WorldState = serde_json::from_str(&json).unwrap();
    let mut back = World::from_state(Arc::new(pack), saved, Vec::new());
    for e in back.state.entities.values_mut().filter(|e| e.human) {
        e.online = true;
    }
    back
}

/// The time just after `mob` is due back.
fn due(w: &World, mob: &str) -> u64 {
    w.entity(&id(mob)).unwrap().dead_until_ms.unwrap() + MINUTE
}

#[test]
fn the_respawned_warden_wields_a_new_blade_and_the_old_one_is_loot() {
    let (mut w, _) = killed(crypt(), "skeletal_warden", "ossuary_crypt");
    let money = w.state.total_currency(&w.pack);
    let warden = w.entity(&id("skeletal_warden")).unwrap();
    assert!(warden.inventory.is_empty() && warden.equipped.is_empty());
    let old = on_floor(&w, "ossuary_crypt", "warden_blade");
    assert_eq!(old.len(), 1, "the blade falls where the warden fell");
    // Not yet due: nothing comes back early.
    let early = w.tick(due(&w, "skeletal_warden") - 2 * MINUTE);
    assert!(
        !early
            .changes
            .iter()
            .any(|c| matches!(c, StateChange::Item { .. } | StateChange::Minted { .. }))
    );
    assert_eq!(w.fresh_mints(), []);
    let now = due(&w, "skeletal_warden");
    let out = w.tick(now);
    let warden = w.entity(&id("skeletal_warden")).unwrap();
    assert!(warden.can_act());
    assert_eq!(warden.room, RoomId::from("ossuary_crypt"));
    let blade = carried(&w, "skeletal_warden", "warden_blade");
    assert_eq!(blade.len(), 1, "one blade, made new");
    assert_ne!(blade[0], old[0]);
    assert_eq!(warden.equipped.get(&Slot::Weapon), Some(&blade[0]));
    assert_eq!(
        on_floor(&w, "ossuary_crypt", "warden_blade"),
        old,
        "the old blade stays where it fell"
    );
    // The mint is ledgered as a script's is: a change, an audit entry and
    // a ledger row named by the respawn line.
    let to = Holder::Entity {
        entity: id("skeletal_warden"),
    };
    assert!(out.changes.contains(&StateChange::Item {
        item: blade[0],
        proto: ProtoId::from("warden_blade"),
        from: None,
        to: Some(to),
    }));
    assert!(out.changes.contains(&StateChange::Equipped {
        entity: id("skeletal_warden"),
        slot: Slot::Weapon,
        item: Some(blade[0]),
    }));
    let line = w
        .drain_fresh()
        .into_iter()
        .find(|e| e.verb.as_deref() == Some(crate::event::VERB_RESPAWN))
        .unwrap();
    let rows = w.fresh_mints().to_vec();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.seq, line.seq);
    assert_eq!(row.pool, PoolId::from(RESPAWN_POOL));
    assert_eq!(row.entity, id("skeletal_warden"));
    assert_eq!((row.currency, row.period, row.at_ms), (0, 0, now));
    assert_eq!(row.items.len(), 1);
    assert_eq!(row.items[0].id, blade[0]);
    assert!(out.changes.contains(&StateChange::Minted {
        pool: PoolId::from(RESPAWN_POOL),
        entity: id("skeletal_warden"),
        currency: 0,
        items: vec![blade[0]],
    }));
    assert!(w.drain_audit().iter().any(|a| matches!(
        a,
        Audit::Minted { pool, items, .. }
            if pool.as_str() == RESPAWN_POOL && items == &[ProtoId::from("warden_blade")]
    )));
    assert_eq!(w.state.minted[&PoolId::from(RESPAWN_POOL)].items, 1);
    assert_eq!(w.state.check(), Vec::<String>::new());
    assert_eq!(w.state.check_rewards(&w.pack, &rows), Vec::<String>::new());
    assert_eq!(
        w.state.total_currency(&w.pack),
        money,
        "a kit makes no money"
    );
}

#[test]
fn a_second_kill_drops_a_second_blade_and_pays_one_kit_per_death() {
    let (mut w, ada) = killed(crypt(), "skeletal_warden", "ossuary_crypt");
    let mut ledger = Vec::new();
    let back = due(&w, "skeletal_warden");
    w.tick(back);
    ledger.extend(w.drain_mints());
    // Ticks with the warden alive mint nothing more.
    w.tick(back + 10 * MINUTE);
    assert_eq!(w.fresh_mints(), []);
    let warden = w.state.entities.get_mut(&id("skeletal_warden")).unwrap();
    warden.pools.hp = -6 * warden.pools.hp_max;
    w.fix_dice([1, 1, 1, 6, 6, 6]);
    let later = KILLED_AT + 200 * MINUTE;
    assert!(w.execute(&ada, "attack warden", later).ok);
    assert_eq!(on_floor(&w, "ossuary_crypt", "warden_blade").len(), 2);
    w.tick(due(&w, "skeletal_warden"));
    ledger.extend(w.drain_mints());
    assert_eq!(ledger.len(), 2, "one kit per death");
    assert_eq!(carried(&w, "skeletal_warden", "warden_blade").len(), 1);
    assert_eq!(w.state.minted[&PoolId::from(RESPAWN_POOL)].mints, 2);
    assert_eq!(w.state.check(), Vec::<String>::new());
    assert_eq!(
        w.state.check_rewards(&w.pack, &ledger),
        Vec::<String>::new()
    );
}

#[test]
fn only_the_pieces_that_fell_are_made_new() {
    // The warren rat drops its tail; its hidden teeth never fall.
    let (mut w, _) = killed(crypt(), "giant_rat_2", "rat_warren");
    let teeth = carried(&w, "giant_rat_2", "rat_teeth");
    assert_eq!(teeth.len(), 1);
    let tail = on_floor(&w, "rat_warren", "rat_tail");
    assert_eq!(tail.len(), 1);
    w.tick(due(&w, "giant_rat_2"));
    assert_eq!(carried(&w, "giant_rat_2", "rat_teeth"), teeth, "kept");
    let new_tail = carried(&w, "giant_rat_2", "rat_tail");
    assert_eq!(new_tail.len(), 1);
    assert_ne!(new_tail, tail);
    let rat = w.entity(&id("giant_rat_2")).unwrap();
    assert_eq!(rat.equipped.get(&Slot::Weapon), Some(&teeth[0]));
    let rows = w.fresh_mints();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].items.len(), 1, "only the tail");
    assert_eq!(w.state.check(), Vec::<String>::new());
}

#[test]
fn a_mob_with_nothing_to_restore_is_unchanged() {
    // The cellar rat carries only its hidden teeth, which never fall.
    let (mut w, _) = killed(crypt(), "giant_rat_1", "wine_vault");
    let before = w.entity(&id("giant_rat_1")).unwrap().clone();
    let next = w.state.next_item;
    w.tick(due(&w, "giant_rat_1"));
    let after = w.entity(&id("giant_rat_1")).unwrap();
    assert!(after.can_act());
    assert_eq!(
        (&after.inventory, &after.equipped),
        (&before.inventory, &before.equipped)
    );
    assert_eq!(w.state.next_item, next, "nothing is made");
    assert!(w.fresh_mints().is_empty() && w.state.minted.is_empty());

    // A mob with no kit at all.
    let mut pack = crypt();
    let rat = pack.cast.get_mut(&id("giant_rat_1")).unwrap();
    rat.inventory.clear();
    rat.equip.clear();
    let (mut w, _) = killed(pack, "giant_rat_1", "wine_vault");
    let next = w.state.next_item;
    let out = w.tick(due(&w, "giant_rat_1"));
    let rat = w.entity(&id("giant_rat_1")).unwrap();
    assert!(rat.can_act());
    assert!(rat.inventory.is_empty() && rat.equipped.is_empty());
    assert_eq!(w.state.next_item, next);
    assert!(
        !out.changes
            .iter()
            .any(|c| matches!(c, StateChange::Item { .. } | StateChange::Minted { .. }))
    );
    assert!(w.fresh_mints().is_empty() && w.state.minted.is_empty());
    assert_eq!(w.state.check(), Vec::<String>::new());
}

#[test]
fn respawn_kit_false_keeps_a_piece_one_off() {
    let mut pack = crypt();
    pack.cast
        .get_mut(&id("skeletal_warden"))
        .unwrap()
        .respawn_kit = Some(false);
    let (mut w, _) = killed(pack, "skeletal_warden", "ossuary_crypt");
    let next = w.state.next_item;
    w.tick(due(&w, "skeletal_warden"));
    let warden = w.entity(&id("skeletal_warden")).unwrap();
    assert!(warden.can_act());
    assert!(warden.inventory.is_empty(), "it fights bare-handed");
    assert_eq!(w.state.next_item, next);
    assert_eq!(w.fresh_mints(), []);
    assert_eq!(on_floor(&w, "ossuary_crypt", "warden_blade").len(), 1);
}

#[test]
fn a_restart_between_death_and_respawn_mints_the_same_kit() {
    let (mut w, _) = killed(crypt(), "skeletal_warden", "ossuary_crypt");
    let json = serde_json::to_string(&w.snapshot()).unwrap();
    let saved: WorldState = serde_json::from_str(&json).unwrap();
    let mut back = World::from_state(Arc::clone(&w.pack), saved, Vec::new());
    let now = due(&w, "skeletal_warden");
    let a = w.tick(now);
    let b = back.tick(now);
    let warden = |w: &World| w.entity(&id("skeletal_warden")).unwrap().clone();
    assert_eq!(warden(&w), warden(&back));
    assert_eq!(w.state.items, back.state.items);
    assert_eq!(w.state.rooms, back.state.rooms);
    assert_eq!(w.state.next_item, back.state.next_item);
    assert_eq!(w.state.minted, back.state.minted);
    assert_eq!(w.fresh_mints(), back.fresh_mints());
    let kit = |o: &TickOutcome| -> Vec<StateChange> {
        o.changes
            .iter()
            .filter(|c| {
                matches!(
                    c,
                    StateChange::Item { .. }
                        | StateChange::Equipped { .. }
                        | StateChange::Minted { .. }
                )
            })
            .cloned()
            .collect()
    };
    assert_eq!(kit(&a), kit(&b));
    assert_eq!(kit(&a).len(), 3, "made, wielded, ledgered");
    // The stored state holds the respawned warden and its ledger totals.
    let again: WorldState =
        serde_json::from_str(&serde_json::to_string(&back.snapshot()).unwrap()).unwrap();
    assert_eq!(again.minted[&PoolId::from(RESPAWN_POOL)].mints, 1);
    assert_eq!(again.check(), Vec::<String>::new());
}

#[test]
fn a_kit_never_makes_money_and_stock_is_not_part_of_it() {
    // A pack lists a purse in the warden's kit: a respawn never makes it.
    let mut pack = crypt();
    pack.cast
        .get_mut(&id("skeletal_warden"))
        .unwrap()
        .inventory
        .push(ProtoId::from("old_coin_pouch"));
    let (mut w, _) = killed(pack, "skeletal_warden", "ossuary_crypt");
    // The warden keeps a shop, and a blade it carries as merchandise is
    // not its own blade.
    w.state
        .entities
        .get_mut(&id("skeletal_warden"))
        .unwrap()
        .shop = Some(ShopId::from("vich"));
    let stock = w.state.new_item(&ProtoId::from("warden_blade"), &w.pack);
    w.state.attach(
        stock,
        &Holder::Entity {
            entity: id("skeletal_warden"),
        },
    );
    w.state.items.get_mut(&stock).unwrap().stock = true;
    let money = w.state.total_currency(&w.pack);
    let next = w.state.next_item;
    w.tick(due(&w, "skeletal_warden"));
    assert!(
        carried(&w, "skeletal_warden", "old_coin_pouch").is_empty(),
        "no purse is made"
    );
    let blades = carried(&w, "skeletal_warden", "warden_blade");
    assert_eq!(blades.len(), 2, "its stock and a blade made new");
    let made: Vec<ItemId> = blades.iter().copied().filter(|b| *b != stock).collect();
    assert_eq!(made.len(), 1);
    let warden = w.entity(&id("skeletal_warden")).unwrap();
    assert_eq!(
        warden.equipped.get(&Slot::Weapon),
        Some(&made[0]),
        "it wields its own blade, not its stock"
    );
    assert_eq!(w.state.next_item, next + 1, "one item is made");
    let rows = w.fresh_mints().to_vec();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].items.len(), 1);
    assert_eq!(rows[0].items[0].proto, ProtoId::from("warden_blade"));
    assert_eq!(w.state.total_currency(&w.pack), money, "no money is made");
    assert_eq!(w.state.check(), Vec::<String>::new());
    assert_eq!(w.state.check_rewards(&w.pack, &rows), Vec::<String>::new());
}

#[test]
fn a_pack_edit_to_respawn_minutes_reaches_a_saved_mob() {
    // The pack drops the rat's respawn while it lies dead: it comes back
    // on the timer it died with, with its kit, and stays dead next time.
    let (w, ada) = killed(crypt(), "giant_rat_2", "rat_warren");
    let back = due(&w, "giant_rat_2");
    let mut pack = crypt();
    pack.cast
        .get_mut(&id("giant_rat_2"))
        .unwrap()
        .respawn_minutes = None;
    let mut w = reload(&w, pack);
    assert_eq!(w.entity(&id("giant_rat_2")).unwrap().respawn_minutes, None);
    w.tick(back);
    assert!(w.entity(&id("giant_rat_2")).unwrap().can_act());
    assert_eq!(
        carried(&w, "giant_rat_2", "rat_tail").len(),
        1,
        "it comes back with its kit"
    );
    kill(&mut w, &ada, "giant_rat_2", "rat_warren", back + MINUTE);
    assert_eq!(
        w.entity(&id("giant_rat_2")).unwrap().dead_until_ms,
        Some(u64::MAX),
        "the pack says it does not respawn"
    );
    assert_eq!(w.state.check(), Vec::<String>::new());

    // A save from a pack where the rat did not respawn, loaded under one
    // where it does: the rat respawns, with its kit.
    let mut w = World::new(Arc::new(crypt()), 42);
    let ada = w.login("ada", 1).unwrap();
    w.state
        .entities
        .get_mut(&id("giant_rat_2"))
        .unwrap()
        .respawn_minutes = None;
    let mut w = reload(&w, crypt());
    assert_eq!(
        w.entity(&id("giant_rat_2")).unwrap().respawn_minutes,
        Some(30)
    );
    kill(&mut w, &ada, "giant_rat_2", "rat_warren", KILLED_AT);
    let until = w.entity(&id("giant_rat_2")).unwrap().dead_until_ms;
    assert_eq!(until, Some(KILLED_AT + 30 * MINUTE));
    w.tick(due(&w, "giant_rat_2"));
    assert!(w.entity(&id("giant_rat_2")).unwrap().can_act());
    assert_eq!(carried(&w, "giant_rat_2", "rat_tail").len(), 1);
    assert_eq!(w.state.check(), Vec::<String>::new());
}
