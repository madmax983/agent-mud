//! Downed, not limbo: coming to, the come-to roll, and what death costs.

use std::sync::Arc;

use serde_json::Value;

use super::*;
use crate::pack::{DeathRules, RecoveryRules};

const MINUTE: u64 = 60_000;

fn id(s: &str) -> EntityId {
    EntityId::from(s)
}

/// crypt-v01 with its own `[recovery]` and `[death]` tables replaced.
fn world_with(recovery: RecoveryRules, death: Option<DeathRules>) -> World {
    let mut pack = WorldPack::crypt_v01().unwrap();
    pack.recovery = recovery;
    if let Some(d) = death {
        pack.death = d;
    }
    World::new(Arc::new(pack), 42)
}

/// A world that comes to by regeneration alone (no come-to roll).
fn regen_only() -> World {
    world_with(RecoveryRules::default(), None)
}

fn crypt() -> World {
    World::new(Arc::new(WorldPack::crypt_v01().unwrap()), 42)
}

/// Lays `who` out cold at `hp` in `room`.
fn knock_out(w: &mut World, who: &EntityId, room: &str, hp: i32) {
    let e = w.state.entities.get_mut(who).unwrap();
    e.room = RoomId::from(room);
    e.pools.hp = hp;
    e.unconscious = true;
}

fn status_changes(out: &TickOutcome) -> Vec<&StateChange> {
    out.changes
        .iter()
        .filter(|c| matches!(c, StateChange::Status { .. }))
        .collect()
}

#[test]
fn marta_at_minus_three_comes_to_once_regen_lifts_her_past_zero() {
    let mut w = regen_only();
    let ada = w.login("ada", 1).unwrap();
    w.state.entities.get_mut(&ada).unwrap().room = RoomId::from("wine_vault");
    let marta = id("marta");
    knock_out(&mut w, &marta, "wine_vault", -3);
    w.drain_fresh();
    for minute in 1..=3 {
        let out = w.tick(minute * MINUTE);
        assert!(
            out.came_to.is_empty(),
            "minute {minute}: still at or below 0"
        );
        assert!(status_changes(&out).is_empty(), "no status change yet");
        assert!(w.entity(&marta).unwrap().unconscious);
    }
    let out = w.tick(4 * MINUTE);
    let e = w.entity(&marta).unwrap();
    assert_eq!((e.pools.hp, e.unconscious), (1, false));
    assert_eq!(out.came_to.len(), 1, "{out:?}");
    assert_eq!(out.came_to[0].0, marta);
    assert_eq!(
        status_changes(&out),
        vec![&StateChange::Status {
            entity: marta.clone(),
            status: "unconscious".into(),
            value: false,
        }]
    );
    let line = w
        .logged()
        .find(|e| e.seq == out.came_to[0].1)
        .unwrap()
        .clone();
    assert_eq!(line.narrative, "Marta stirs and comes to.");
    assert!(line.actor.is_none(), "no actor: the router never skips her");
    assert!(line.targets(&marta));
    assert!(line.tags.iter().any(|t| t == CAME_TO_TAG));
    assert!(line.recipients.contains(&ada), "a bystander sees it");
    assert!(out.seqs.contains(&line.seq));
    let later = w.tick(5 * MINUTE);
    assert!(later.came_to.is_empty(), "she comes to once");
}

#[test]
fn an_offline_entity_gets_no_regen_and_does_not_come_to() {
    let mut w = regen_only();
    let k = id("pc_kestrel");
    knock_out(&mut w, &k, "wine_vault", 0);
    let out = w.tick(MINUTE);
    assert!(out.came_to.is_empty(), "offline: no come-to");
    let e = w.entity(&k).unwrap();
    assert_eq!((e.pools.hp, e.unconscious), (0, true));
}

#[test]
fn an_ht_success_brings_her_to_while_still_below_zero() {
    let mut w = crypt();
    let marta = id("marta");
    knock_out(&mut w, &marta, "wine_vault", -3);
    // HT 11: 3+3+3 = 9 succeeds.
    w.fix_dice([3, 3, 3]);
    let out = w.tick(MINUTE);
    let e = w.entity(&marta).unwrap();
    assert_eq!(
        (e.pools.hp, e.unconscious),
        (-2, false),
        "regen first, then the roll"
    );
    assert_eq!(out.came_to.len(), 1);
    let rolls: Vec<Audit> = w
        .drain_audit()
        .into_iter()
        .filter(|a| matches!(a, Audit::ComeToRoll { .. }))
        .collect();
    assert!(
        matches!(&rolls[..], [Audit::ComeToRoll { attr, hp: -2, came_to: true, .. }] if attr == "ht"),
        "{rolls:?}"
    );
}

#[test]
fn a_failed_roll_keeps_her_down_and_at_minus_hp_no_roll_is_made() {
    let mut w = crypt();
    let marta = id("marta");
    knock_out(&mut w, &marta, "wine_vault", -5);
    w.fix_dice([6, 6, 6]);
    assert!(w.tick(MINUTE).came_to.is_empty(), "a failed roll at -4");
    assert!(w.entity(&marta).unwrap().unconscious);
    // At -11 of 10, the step's regen leaves her at -10: -HP, no roll.
    knock_out(&mut w, &marta, "wine_vault", -11);
    w.fix_dice([1, 1, 1]);
    assert!(w.tick(2 * MINUTE).came_to.is_empty(), "no roll at -HP");
    assert!(w.entity(&marta).unwrap().unconscious);
    assert_eq!(w.entity(&marta).unwrap().pools.hp, -10);
    assert!(
        !w.drain_audit()
            .iter()
            .any(|a| matches!(a, Audit::ComeToRoll { hp, .. } if *hp == -10)),
        "no come-to roll at -HP"
    );
}

#[test]
fn the_same_seed_brings_her_to_in_the_same_minute() {
    let minute_of = |seed: u64| {
        let mut w = World::new(Arc::new(WorldPack::crypt_v01().unwrap()), seed);
        knock_out(&mut w, &id("marta"), "wine_vault", -8);
        (1..=20)
            .find(|m| !w.tick(m * MINUTE).came_to.is_empty())
            .unwrap()
    };
    assert_eq!(minute_of(7), minute_of(7));
    let minutes: std::collections::BTreeSet<u64> = (0..40).map(minute_of).collect();
    assert!(
        minutes.iter().all(|m| *m <= 9),
        "regen alone needs 9: {minutes:?}"
    );
    assert!(minutes.len() > 1, "the roll varies the minute: {minutes:?}");
}

#[test]
fn the_recovery_step_follows_the_pack_period() {
    let mut w = world_with(
        RecoveryRules {
            come_to_roll: None,
            every_secs: 120,
        },
        None,
    );
    let marta = id("marta");
    knock_out(&mut w, &marta, "wine_vault", 0);
    assert!(w.tick(MINUTE).came_to.is_empty(), "a minute is not a step");
    assert_eq!(w.tick(2 * MINUTE).came_to.len(), 1);
}

#[test]
fn a_draught_on_an_unconscious_npc_brings_it_to() {
    let mut w = regen_only();
    let ada = w.login("ada", 1).unwrap();
    w.state.entities.get_mut(&ada).unwrap().room = RoomId::from("wine_vault");
    let pack = Arc::clone(&w.pack);
    let vial = w.state.new_item(&ProtoId::from("healing_draught"), &pack);
    w.state.attach(
        vial,
        &Holder::Entity {
            entity: ada.clone(),
        },
    );
    let vich = id("vich");
    knock_out(&mut w, &vich, "wine_vault", -3);
    // 2d: 4 + 4 = 8, so Vich is at 5.
    w.fix_dice([4, 4]);
    let r = w.execute(&ada, "use draught on vich", 1_000);
    assert!(r.ok, "{r:?}");
    let e = w.entity(&vich).unwrap();
    assert_eq!((e.pools.hp, e.unconscious), (5, false));
    assert!(r.state_changes.contains(&StateChange::Status {
        entity: vich.clone(),
        status: "unconscious".into(),
        value: false,
    }));
    let came: Vec<&Event> = w
        .logged()
        .filter(|e| e.tags.iter().any(|t| t == CAME_TO_TAG))
        .collect();
    assert_eq!(came.len(), 1);
    assert!(came[0].targets(&vich) && came[0].actor.is_none());
    assert!(r.events.contains(&came[0].seq));
}

#[test]
fn a_heal_that_leaves_hp_below_zero_does_not_bring_anyone_to() {
    let mut w = regen_only();
    let ada = w.login("ada", 1).unwrap();
    w.state.entities.get_mut(&ada).unwrap().room = RoomId::from("wine_vault");
    let vich = id("vich");
    knock_out(&mut w, &vich, "wine_vault", -9);
    w.fix_dice([1, 1, 6, 6, 6]);
    let r = w.execute(&ada, "use bandage on vich", 1_000);
    assert!(r.ok, "{r:?}");
    assert!(w.entity(&vich).unwrap().unconscious);
    assert!(!w.logged().any(|e| e.tags.iter().any(|t| t == CAME_TO_TAG)));
}

#[test]
fn out_cold_an_entity_reads_itself_and_the_room_and_hears_the_rule() {
    let mut w = crypt();
    let ada = w.login("ada", 1).unwrap();
    knock_out(&mut w, &ada, "wine_vault", -3);
    let r = w.execute(&ada, "say help", 1_000);
    assert_eq!(r.error_code, Some(ErrorCode::Incapacitated));
    for want in [
        "sheet, help, who, look",
        "1 HP every minute",
        "HT roll",
        "above -HP",
    ] {
        assert!(r.narrative.contains(want), "{want:?} in {:?}", r.narrative);
    }
    let r = w.execute(&ada, "wake", 1_000);
    assert_eq!(
        r.error_code,
        Some(ErrorCode::Incapacitated),
        "an unknown word while out cold gets the hint: {r:?}"
    );
    assert!(r.narrative.contains("Still works"));
    let r = w.execute(&ada, "look", 1_000);
    assert!(r.ok, "look works out cold: {r:?}");
    assert!(
        r.read_only && r.state_changes.is_empty() && r.events.is_empty(),
        "look is a view"
    );
    assert!(w.execute(&ada, "sheet", 1_000).ok);
    let awake = w.login("bo", 1).unwrap();
    assert_eq!(
        w.execute(&awake, "wake", 1_000).error_code,
        Some(ErrorCode::UnknownVerb),
        "awake, an unknown word stays unknown"
    );
}

#[test]
fn help_gives_the_recovery_and_death_rules() {
    let mut w = crypt();
    let ada = w.login("ada", 1).unwrap();
    let r = w.execute(&ada, "help", 1_000);
    for want in [
        "recovery:",
        "1 HP every minute",
        "HT roll",
        "about 34% of your HP",
        "purse stays where you fell",
    ] {
        assert!(r.narrative.contains(want), "{want:?} in {:?}", r.narrative);
    }
    let plain = World::new(
        Arc::new({
            let mut p = WorldPack::crypt_v01().unwrap();
            p.recovery = RecoveryRules::default();
            p.death = DeathRules::default();
            p
        }),
        1,
    );
    assert!(!plain.recovery_rule().contains("roll"));
    assert!(plain.death_rule().contains("whole"));
}

#[test]
fn death_drops_the_purse_where_she_fell_and_she_wakes_at_a_third() {
    let mut w = crypt();
    let ada = w.login("ada", 1).unwrap();
    let bo = w.login("bo", 1).unwrap();
    for who in [&ada, &bo] {
        w.state.entities.get_mut(who).unwrap().room = RoomId::from("wine_vault");
    }
    let money = w.state.total_currency(&w.pack);
    let mut ctx = Ctx {
        now: 5_000,
        ..Ctx::default()
    };
    w.wound(&mut ctx, &ada, 60, &id("giant_rat_1"));
    let e = w.entity(&ada).unwrap();
    assert_eq!(e.room.as_str(), "town_square");
    assert_eq!(e.pools.hp, 3, "about a third of 10");
    assert_eq!(e.pools.fp, e.pools.fp_max);
    assert_eq!(e.currency, 0);
    assert!(!e.unconscious);
    let floor: Vec<&crate::state::Item> = w.state.rooms[&RoomId::from("wine_vault")]
        .items
        .iter()
        .map(|i| &w.state.items[i])
        .collect();
    let purse = floor
        .iter()
        .find(|i| i.proto.as_str() == "dropped_purse")
        .unwrap();
    assert_eq!(purse.currency, Some(25));
    assert_eq!(w.state.total_currency(&w.pack), money, "the coins moved");
    assert!(w.state.check().is_empty(), "{:?}", w.state.check());
    let spill = w.logged().find(|e| e.kind == EventKind::ItemDrop).unwrap();
    assert_eq!(spill.narrative, "ada's dropped purse falls where ada fell.");
    assert!(spill.recipients.contains(&bo));
    // Bo lights a torch and loots it, and the money is still all there.
    assert!(w.execute(&bo, "equip torch", 6_000).ok);
    let r = w.execute(&bo, "take purse", 6_000);
    assert!(r.ok, "{r:?}");
    let r = w.execute(&bo, "use purse", 6_000);
    assert!(r.ok, "{r:?}");
    assert_eq!(w.entity(&bo).unwrap().currency, 25 + 25);
    assert_eq!(w.state.total_currency(&w.pack), money);
    // The purse round-trips through a snapshot, as an older one loads.
    let saved: WorldState =
        serde_json::from_value(serde_json::to_value(w.snapshot()).unwrap()).unwrap();
    assert_eq!(saved.items, w.state.items);
}

#[test]
fn an_empty_purse_is_not_dropped_and_mobs_keep_their_respawn() {
    let mut w = crypt();
    let ada = w.login("ada", 1).unwrap();
    w.state.entities.get_mut(&ada).unwrap().currency = 0;
    w.state.entities.get_mut(&ada).unwrap().room = RoomId::from("wine_vault");
    let mut ctx = Ctx::default();
    w.wound(&mut ctx, &ada, 60, &id("giant_rat_1"));
    assert!(
        !w.state
            .items
            .values()
            .any(|i| i.proto.as_str() == "dropped_purse"),
        "nothing to drop"
    );
    let rat = id("giant_rat_1");
    w.wound(&mut ctx, &rat, 200, &ada);
    let e = w.entity(&rat).unwrap();
    assert!(e.dead_until_ms.is_some(), "a mob still waits to respawn");
    assert!(e.incapacitated());
}

#[test]
fn without_a_death_table_the_dead_wake_whole_with_their_purse() {
    let mut w = world_with(RecoveryRules::default(), Some(DeathRules::default()));
    let ada = w.login("ada", 1).unwrap();
    let mut ctx = Ctx::default();
    w.wound(&mut ctx, &ada, 60, &id("giant_rat_1"));
    let e = w.entity(&ada).unwrap();
    assert_eq!((e.pools.hp, e.currency), (e.pools.hp_max, 25));
}

#[test]
fn a_bad_recovery_or_death_table_fails_the_pack() {
    let src = |world: &str| {
        let base = include_str!("../../../../worlds/crypt-v01/world.toml");
        let text = base.replace(
            "[recovery]\ncome_to_roll = \"ht\"\nevery_secs = 60\n",
            world,
        );
        WorldPack::from_sources(crate::pack::PackSources {
            world: &text,
            skills: include_str!("../../../../worlds/crypt-v01/skills.toml"),
            items: include_str!("../../../../worlds/crypt-v01/items.toml"),
            economy: include_str!("../../../../worlds/crypt-v01/economy.toml"),
            cast: include_str!("../../../../worlds/crypt-v01/cast.toml"),
        })
    };
    let err = |world: &str| src(world).unwrap_err().to_string();
    assert!(err("[recovery]\ncome_to_roll = \"luck\"\n").contains("unknown attribute \"luck\""));
    assert!(err("[recovery]\nevery_secs = 0\n").contains("recovery.every_secs"));
    let bad_death = |death: &str| {
        let base = include_str!("../../../../worlds/crypt-v01/world.toml");
        let start = base.find("[death]").unwrap();
        let end = base[start..].find("\n\n").unwrap() + start;
        let text = format!("{}{death}{}", &base[..start], &base[end..]);
        WorldPack::from_sources(crate::pack::PackSources {
            world: &text,
            skills: include_str!("../../../../worlds/crypt-v01/skills.toml"),
            items: include_str!("../../../../worlds/crypt-v01/items.toml"),
            economy: include_str!("../../../../worlds/crypt-v01/economy.toml"),
            cast: include_str!("../../../../worlds/crypt-v01/cast.toml"),
        })
        .unwrap_err()
        .to_string()
    };
    assert!(bad_death("[death]\nrespawn_hp_frac = 0.0").contains("respawn_hp_frac"));
    assert!(bad_death("[death]\ndrop_purse = true").contains("needs death.purse_item"));
    assert!(bad_death("[death]\npurse_item = \"ghost\"").contains("unknown item \"ghost\""));
    assert!(bad_death("[death]\npurse_item = \"dagger\"").contains("must carry currency"));
    // A misspelt key is an error, not a silent default: a typo must not
    // undo a free full heal's cost or the come-to roll.
    let typo = err("[recovery]\ncome_to = \"ht\"\n");
    assert!(
        typo.contains("unknown field") && typo.contains("come_to"),
        "{typo}"
    );
    let typo = bad_death(
        "[death]\nrespawn_hp_fraction = 0.34\ndrop_purse = true\npurse_item = \"dropped_purse\"",
    );
    assert!(
        typo.contains("unknown field") && typo.contains("respawn_hp_fraction"),
        "{typo}"
    );
    let pack = src("").unwrap();
    assert_eq!(pack.recovery, RecoveryRules::default(), "no table: no roll");
}

#[test]
fn respawn_hp_rounds_and_stays_in_range() {
    let d = |f: f64| DeathRules {
        respawn_hp_frac: f,
        ..DeathRules::default()
    };
    assert_eq!(d(0.34).respawn_hp(10), 3);
    assert_eq!(d(0.34).respawn_hp(12), 4);
    assert_eq!(d(0.01).respawn_hp(10), 1, "never below 1");
    assert_eq!(d(1.0).respawn_hp(10), 10);
    assert_eq!(d(1.0).respawn_hp(0), 1);
}

#[test]
fn the_sheet_says_how_hurt_and_whether_out_cold() {
    let mut w = crypt();
    let ada = w.login("ada", 1).unwrap();
    knock_out(&mut w, &ada, "wine_vault", -3);
    let v: Value = w.sheet_view(&ada);
    assert_eq!(v["pools"]["hp"], -3);
    assert_eq!(v["unconscious"], true);
    assert_eq!(v["wound_state"], "reeling");
}
