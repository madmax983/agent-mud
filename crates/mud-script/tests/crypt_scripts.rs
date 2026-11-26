//! The shipped crypt-v01 scripts, played through the real engine.
#![expect(clippy::unwrap_used, reason = "test helpers fail loudly")]

use std::path::Path;
use std::sync::Arc;

use mud_script::{ScriptHost, ScriptSources};
use mud_world::hooks::{FlagValue, WorldHooks};
use mud_world::state::{Holder, WorldState};
use mud_world::{
    EntityId, ErrorCode, EventKind, HooksChange, PoolId, ProtoId, RoomId, StateChange, World,
    WorldPack,
};

fn pack() -> Arc<WorldPack> {
    Arc::new(WorldPack::crypt_v01().unwrap())
}

fn pack_dir() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../worlds/crypt-v01"
    ))
}

/// 2023-11-14 at `hour`:00 UTC.
const fn at(hour: u64) -> u64 {
    1_699_920_000_000 + hour * 3_600_000
}

/// A world with the shipped scripts and players `names` logged in at
/// `rooms`; `edit` changes the state first (placing gear, killing mobs).
fn world_with(
    names: &[(&str, &str)],
    edit: impl FnOnce(&mut WorldState, &WorldPack),
) -> (World, Vec<EntityId>) {
    let pack = pack();
    let mut w = World::new(Arc::clone(&pack), 42);
    let ids: Vec<EntityId> = names
        .iter()
        .map(|(n, _)| w.login(n, at(1)).unwrap())
        .collect();
    let mut state = w.snapshot();
    for ((_, room), id) in names.iter().zip(&ids) {
        state.entities.get_mut(id).unwrap().room = RoomId::from(*room);
    }
    edit(&mut state, &pack);
    let mut w = World::from_state(Arc::clone(&pack), state, Vec::new());
    let host = Arc::new(ScriptHost::bundled_crypt_v01(&pack).unwrap());
    w.set_hooks(Some(host as Arc<dyn WorldHooks>), at(1), HooksChange::Start);
    for id in &ids {
        w.bring_online(id, at(1));
    }
    w.drain_fresh();
    (w, ids)
}

fn give(state: &mut WorldState, pack: &WorldPack, who: &str, proto: &str) {
    let item = state.new_item(&ProtoId::from(proto), pack);
    state.attach(
        item,
        &Holder::Entity {
            entity: EntityId::from(who),
        },
    );
}

fn kill(state: &mut WorldState, who: &str) {
    let e = state.entities.get_mut(&EntityId::from(who)).unwrap();
    e.dead_until_ms = Some(u64::MAX);
    e.deaths += 1;
}

/// The world again, after the warden came back and was put down once more.
fn warden_dies_again(w: &World, ids: &[EntityId]) -> World {
    let pack = pack();
    let mut state = w.snapshot();
    kill(&mut state, "skeletal_warden");
    let mut again = World::from_state(Arc::clone(&pack), state, Vec::new());
    let host = Arc::new(ScriptHost::bundled_crypt_v01(&pack).unwrap());
    again.set_hooks(Some(host as Arc<dyn WorldHooks>), at(1), HooksChange::Start);
    for id in ids {
        again.bring_online(id, at(1));
    }
    again.drain_fresh();
    again
}

fn narrations(w: &mut World) -> Vec<String> {
    w.drain_fresh()
        .into_iter()
        .filter(|e| e.kind == EventKind::System)
        .map(|e| e.narrative)
        .collect()
}

#[test]
fn the_shipped_scripts_pass_check_and_dry_run() {
    let pack = pack();
    let host = ScriptHost::load(pack_dir(), &pack).unwrap();
    assert_eq!(host.dry_run(&pack), Vec::<String>::new());
    let summary = host.summary();
    assert!(
        summary.ends_with("(7 files: 1 mind, 2 rooms, 2 items, 3 verbs)"),
        "{summary}"
    );
    let bundled = ScriptHost::bundled_crypt_v01(&pack).unwrap();
    assert_eq!(
        bundled.version(),
        host.version(),
        "the bundled copy is the folder"
    );
    assert!(!bundled.reloadable() && host.reloadable());
    assert_eq!(
        host.rhai_runtime_minds().into_iter().collect::<Vec<_>>(),
        [EntityId::from("guard_01")]
    );
    assert_eq!(host.mind_memory_schema(&EntityId::from("guard_01")), 1);
    assert_eq!(host.hook_keys().len(), 7);
}

#[test]
fn the_bundled_script_list_matches_the_pack_folder() {
    let read = ScriptSources::read_dir(pack_dir()).unwrap();
    assert_eq!(read.warnings, Vec::<String>::new(), "every file is used");
    assert_eq!(read.problems, Vec::<String>::new());
    let bundled = mud_script::crypt_v01_sources();
    assert_eq!(
        read.files.keys().collect::<Vec<_>>(),
        bundled.files.keys().collect::<Vec<_>>()
    );
    assert_eq!(read.version(), bundled.version());
}

#[test]
fn the_ossuary_marks_its_first_visitor_once() {
    // Both put their torches away: the warden waits in the dark.
    let (mut w, ids) = world_with(
        &[("ada", "flooded_cistern"), ("bo", "flooded_cistern")],
        |state, _| {
            for e in state.entities.values_mut().filter(|e| e.human) {
                e.equipped
                    .retain(|slot, _| *slot != mud_world::pack::Slot::Light);
            }
        },
    );
    let (ada, bo) = (&ids[0], &ids[1]);
    w.fix_dice([6; 60]);
    let r = w.execute(ada, "s", at(1));
    assert!(r.ok, "{r:?}");
    assert!(
        r.narrative.contains("bone scrapes on stone"),
        "the warden is here, in the dark: {}",
        r.narrative
    );
    // A narration skips the actor, who is usually alone down here: the
    // first visitor reads the welcome in their own narrative.
    assert!(
        r.narrative.contains("Dust sifts from the vault"),
        "the first visitor sees the welcome: {}",
        r.narrative
    );
    assert_eq!(
        narrations(&mut w),
        ["Dust sifts from the vault. Nothing living has stood here in a long time."]
    );
    assert!(w.execute(bo, "s", at(1)).ok);
    assert!(narrations(&mut w).is_empty(), "only the first is welcomed");
    assert_eq!(
        w.state().flags["ossuary.first_visitor"],
        FlagValue::Text("ada".into())
    );
}

#[test]
fn pray_heals_once_a_day_and_never_while_the_warden_stands() {
    let (mut w, ids) = world_with(&[("ada", "ossuary_crypt")], |_, _| {});
    let ada = &ids[0];
    let r = w.execute(ada, "pray", at(2));
    assert_eq!(r.error_code, Some(ErrorCode::Refused));
    assert!(
        r.narrative.contains("Not while it stands"),
        "{}",
        r.narrative
    );
    assert_eq!(
        w.execute(ada, "listen", at(2)).error_code,
        Some(ErrorCode::NotHere)
    );

    let (mut w, ids) = world_with(&[("ada", "ossuary_crypt")], |s, _| {
        kill(s, "skeletal_warden");
        s.entities
            .get_mut(&EntityId::from("pc_ada"))
            .unwrap()
            .pools
            .hp = 4;
    });
    let ada = &ids[0];
    w.fix_dice([1, 1, 1, 3]);
    let r = w.execute(ada, "pray", at(2));
    assert!(r.ok, "{r:?}");
    assert!(r.narrative.contains("A cold calm"), "{}", r.narrative);
    assert_eq!(w.entity(ada).unwrap().pools.hp, 7);
    assert_eq!(
        narrations(&mut w),
        ["ada kneels by the cracked sarcophagus."]
    );
    let r = w.execute(ada, "pray", at(3));
    assert!(r.narrative.contains("enough today"), "{}", r.narrative);
    assert_eq!(w.entity(ada).unwrap().pools.hp, 7);
    // The next day, the dead listen again; a plain failure says so.
    w.fix_dice([5, 5, 5]);
    let r = w.execute(ada, "pray", at(26));
    assert!(r.narrative.contains("Nothing answers"), "{}", r.narrative);
}

#[test]
fn listen_reports_rats_and_warden_until_they_die() {
    let (mut w, ids) = world_with(&[("ada", "dark_alley")], |_, _| {});
    let ada = &ids[0];
    w.fix_dice([1, 1, 1]);
    let r = w.execute(ada, "listen", at(2));
    assert!(r.ok, "{r:?}");
    assert_eq!(r.resolution["script"], "verbs.listen");
    assert_eq!(
        r.narrative,
        "You hold still and listen: claws skittering over broken glass, and far below, something dragging metal over stone."
    );
    w.fix_dice([6, 6, 6]);
    assert_eq!(
        w.execute(ada, "listen", at(2)).narrative,
        "Water, and your own breath. Nothing else."
    );
    let (mut w, ids) = world_with(&[("ada", "cellar_landing")], |s, _| {
        kill(s, "giant_rat_1");
        kill(s, "giant_rat_2");
        kill(s, "skeletal_warden");
    });
    w.fix_dice([1, 1, 1]);
    let r = w.execute(&ids[0], "listen", at(2));
    assert!(r.narrative.starts_with("Only water"), "{}", r.narrative);
}

#[test]
fn saying_vich_in_the_alley_twitches_the_curtain_without_quoting() {
    let (mut w, ids) = world_with(&[("ada", "dark_alley")], |_, _| {});
    let ada = &ids[0];
    let r = w.execute(ada, "say hello?", at(2));
    assert!(r.ok);
    assert_eq!(narrations(&mut w), Vec::<String>::new());
    let r = w.execute(ada, "say Vich, you there? zq-secret", at(2));
    assert!(r.ok);
    // The speaker, alone in the alley, sees the curtain too.
    assert!(
        r.narrative.contains("the curtain twitches"),
        "the speaker sees the curtain: {}",
        r.narrative
    );
    let evs = w.drain_fresh();
    let curtain: Vec<_> = evs.iter().filter(|e| e.kind == EventKind::System).collect();
    assert_eq!(curtain.len(), 1);
    assert_eq!(
        curtain[0].narrative,
        "The curtain over the east door twitches, then hangs still."
    );
    assert!(curtain[0].untrusted_text.is_none());
    assert!(!curtain[0].narrative.contains("zq"));
    let (mut w, ids) = world_with(&[("ada", "dark_alley")], |s, _| {
        s.entities.get_mut(&EntityId::from("vich")).unwrap().hidden = true;
    });
    let r = w.execute(&ids[0], "say vich", at(2));
    assert!(r.ok);
    assert!(!r.narrative.contains("curtain"), "{}", r.narrative);
    assert!(
        narrations(&mut w).is_empty(),
        "a hidden fence does not stir"
    );
}

#[test]
fn taking_ore_under_the_warden_can_cost_blood() {
    let (mut w, ids) = world_with(&[("ada", "ossuary_crypt")], |_, _| {});
    let ada = &ids[0];
    assert!(w.execute(ada, "equip torch", at(2)).ok);
    w.fix_dice([6, 6, 6, 6]);
    let before = w.entity(ada).unwrap().pools.hp;
    let r = w.execute(ada, "take ore", at(2));
    assert!(r.ok, "{r:?}");
    assert!(r.narrative.contains("bites your wrist"), "{}", r.narrative);
    assert_eq!(w.entity(ada).unwrap().pools.hp, before - 5);
    assert_eq!(
        narrations(&mut w),
        ["The warden's blade flicks at ada's hand."]
    );
    let (mut w, ids) = world_with(&[("ada", "ossuary_crypt")], |s, p| {
        give(s, p, "pc_ada", "lantern");
    });
    assert!(w.execute(&ids[0], "equip lantern", at(2)).ok);
    w.fix_dice([1, 1, 1]);
    let r = w.execute(&ids[0], "take ore", at(2));
    assert!(r.narrative.contains("whistles past"), "{}", r.narrative);
}

#[test]
fn the_bone_charm_hums_near_the_warden_and_answers_use_on_it() {
    let (mut w, ids) = world_with(
        &[("ada", "ossuary_crypt"), ("bo", "town_square")],
        |s, p| {
            give(s, p, "pc_ada", "bone_charm");
            give(s, p, "pc_bo", "bone_charm");
        },
    );
    let (ada, bo) = (&ids[0], &ids[1]);
    let r = w.execute(ada, "use charm on warden", at(2));
    assert!(r.narrative.contains("hears a name"), "{r:?}");
    let r = w.execute(ada, "use charm", at(2));
    assert!(r.narrative.contains("trapped wasp"), "{}", r.narrative);
    assert_eq!(
        narrations(&mut w),
        ["ada's fist rattles with something small and angry."]
    );
    assert!(
        w.execute(bo, "use charm", at(2))
            .narrative
            .contains("faintly warm")
    );
    let (mut w, ids) = world_with(&[("bo", "town_square")], |s, p| {
        give(s, p, "pc_bo", "bone_charm");
        kill(s, "skeletal_warden");
    });
    assert!(
        w.execute(&ids[0], "use charm", at(2))
            .narrative
            .contains("cold and quiet")
    );
}

fn carried(w: &World, who: &EntityId, proto: &str) -> usize {
    w.entity(who)
        .unwrap()
        .inventory
        .iter()
        .filter(|i| w.state().items[*i].proto.as_str() == proto)
        .count()
}

#[test]
fn hale_pays_a_ledgered_bounty_for_the_wardens_blade() {
    // With the warden walking, no bounty; without the blade, none either.
    let (mut w, ids) = world_with(&[("ada", "town_square")], |s, p| {
        give(s, p, "pc_ada", "warden_blade");
    });
    let r = w.execute(&ids[0], "report", at(2));
    assert!(r.narrative.contains("walks again"), "{r:?}");
    let (mut w, ids) = world_with(&[("ada", "town_square")], |s, _| {
        kill(s, "skeletal_warden");
    });
    let r = w.execute(&ids[0], "report", at(2));
    assert!(r.narrative.contains("Bring me proof"), "{r:?}");
    assert!(w.execute(&ids[0], "s", at(2)).ok);
    assert_eq!(
        w.execute(&ids[0], "report", at(2)).error_code,
        Some(ErrorCode::NotHere)
    );
    assert_eq!(w.fresh_mints(), []);

    // The ossuary is quiet, and ada brings back the one grave-blade.
    let names = [
        ("ada", "town_square"),
        ("bo", "town_square"),
        ("cy", "town_square"),
        ("di", "town_square"),
    ];
    let (mut w, ids) = world_with(&names, |s, p| {
        kill(s, "skeletal_warden");
        give(s, p, "pc_ada", "warden_blade");
    });
    let money = w.state().total_currency(w.pack());
    let purse = w.entity(&ids[0]).unwrap().currency;
    let r = w.execute(&ids[0], "report", at(2));
    assert!(r.ok, "{r:?}");
    assert!(
        r.narrative.contains("The watch pays for that"),
        "{}",
        r.narrative
    );
    assert!(
        r.narrative
            .ends_with("You receive 20s and a healing draught."),
        "{}",
        r.narrative
    );
    assert_eq!(w.entity(&ids[0]).unwrap().currency, purse + 20);
    assert_eq!(carried(&w, &ids[0], "healing_draught"), 1);
    assert_eq!(carried(&w, &ids[0], "warden_blade"), 1, "he keeps no proof");
    assert!(r.state_changes.iter().any(|c| matches!(
        c,
        StateChange::Minted { pool, currency: 20, .. } if *pool == PoolId::from("warden_bounty")
    )));
    let mint = w.fresh_mints()[0].clone();
    assert_eq!(
        (mint.hook.as_str(), &mint.entity, mint.currency),
        ("verbs.report", &ids[0], 20)
    );
    // The others in the square see silver counted, not the ledger.
    let seen: Vec<String> = w
        .drain_fresh()
        .into_iter()
        .filter(|e| e.reaches(&ids[1]))
        .map(|e| e.narrative)
        .collect();
    assert_eq!(seen, ["Hale counts silver into ada's hand."]);
    let r = w.execute(&ids[0], "report", at(3));
    assert!(r.narrative.contains("already paid"), "{}", r.narrative);
    // One bounty per death: ada cannot claim the same kill twice, and the
    // blade passed from hand to hand pays no one else for it.
    assert!(w.execute(&ids[0], "give blade to bo", at(3)).ok);
    let r = w.execute(&ids[1], "report", at(3));
    assert!(r.narrative.contains("already paid"), "{}", r.narrative);
    assert!(w.execute(&ids[1], "give blade to cy", at(3)).ok);
    let r = w.execute(&ids[2], "report", at(3));
    assert!(r.narrative.contains("already paid"), "{}", r.narrative);
    assert_eq!(w.fresh_mints().len(), 1);
    assert_eq!(carried(&w, &ids[2], "healing_draught"), 0);
    assert_eq!(w.state().total_currency(w.pack()), money + 20);
}

#[test]
fn the_watch_pays_once_per_death_three_a_day_and_once_a_day_each() {
    let names = [
        ("ada", "town_square"),
        ("bo", "town_square"),
        ("cy", "town_square"),
        ("di", "town_square"),
    ];
    let (mut w, ids) = world_with(&names, |s, p| {
        kill(s, "skeletal_warden");
        give(s, p, "pc_cy", "warden_blade");
    });
    let money = w.state().total_currency(w.pack());
    assert!(w.execute(&ids[2], "report", at(3)).ok);
    let mut ledger = w.drain_mints();
    assert_eq!(ledger.len(), 1);

    // The warden walks again and is put down again: one more bounty, for
    // whoever brings the blade. The pool pays three a day: the fourth
    // death that day finds the purse empty, and the hook does not fault.
    let mut w = warden_dies_again(&w, &ids);
    let r = w.execute(&ids[2], "report", at(4));
    assert!(r.narrative.contains("Once a day"), "{}", r.narrative);
    assert!(w.execute(&ids[2], "give blade to ada", at(4)).ok);
    assert!(w.execute(&ids[0], "report", at(4)).ok);
    assert_eq!(carried(&w, &ids[0], "healing_draught"), 1);
    assert!(w.execute(&ids[0], "give blade to di", at(4)).ok);
    ledger.extend(w.drain_mints());
    let mut w = warden_dies_again(&w, &ids);
    assert!(w.execute(&ids[3], "report", at(5)).ok);
    assert!(w.execute(&ids[3], "give blade to bo", at(5)).ok);
    ledger.extend(w.drain_mints());
    let mut w = warden_dies_again(&w, &ids);
    let r = w.execute(&ids[1], "report", at(6));
    assert!(r.ok && r.narrative.contains("purse is empty"), "{r:?}");
    assert_eq!(carried(&w, &ids[1], "healing_draught"), 0);
    assert!(w.state().hook_faults.is_empty());
    assert_eq!(ledger.len(), 3);
    assert_eq!(w.state().total_currency(w.pack()), money + 60);
    assert_eq!(w.state().minted_currency(), 60);
    assert_eq!(
        w.state().check_rewards(w.pack(), &ledger),
        Vec::<String>::new()
    );
    // The next game day that unpaid death pays bo, once.
    let r = w.execute(&ids[1], "report", at(26));
    assert!(r.narrative.contains("The watch pays"), "{}", r.narrative);
    assert_eq!(carried(&w, &ids[1], "healing_draught"), 1);
    let r = w.execute(&ids[1], "report", at(26));
    assert!(r.narrative.contains("already paid"), "{}", r.narrative);
    ledger.extend(w.drain_mints());
    // Once a day per player, whatever the deaths: bo is paid no more today,
    // but cy, paid yesterday, is.
    let mut w = warden_dies_again(&w, &ids);
    let r = w.execute(&ids[1], "report", at(27));
    assert!(r.narrative.contains("Once a day"), "{}", r.narrative);
    assert!(w.execute(&ids[1], "give blade to cy", at(27)).ok);
    let r = w.execute(&ids[2], "report", at(27));
    assert!(r.narrative.contains("The watch pays"), "{}", r.narrative);
    ledger.extend(w.drain_mints());
    assert_eq!(ledger.len(), 5);
    assert_eq!(w.state().total_currency(w.pack()), money + 100);
    assert_eq!(w.state().check(), Vec::<String>::new());
    assert_eq!(
        w.state().check_rewards(w.pack(), &ledger),
        Vec::<String>::new()
    );
}

#[test]
fn a_full_mark_table_never_lets_a_player_claim_twice() {
    // ada already holds 32 marks: her once-a-day mark could not be kept,
    // so Hale pays her nothing (rather than every time), and the hook
    // stays up.
    let (mut w, ids) = world_with(&[("ada", "town_square")], |s, p| {
        kill(s, "skeletal_warden");
        give(s, p, "pc_ada", "warden_blade");
        let ada = s.entities.get_mut(&EntityId::from("pc_ada")).unwrap();
        for i in 0..32 {
            ada.marks.insert(format!("quest_{i}"), FlagValue::Int(1));
        }
    });
    for hour in 2..8 {
        let r = w.execute(&ids[0], "report", at(hour));
        assert_eq!(r.error_code, Some(ErrorCode::Refused), "{r:?}");
    }
    assert_eq!(w.fresh_mints(), []);
    assert_eq!(carried(&w, &ids[0], "healing_draught"), 0);
    assert!(w.state().hook_faults.is_empty());
}

#[test]
fn nobody_is_paid_while_hale_is_away() {
    let (mut w, ids) = world_with(&[("ada", "town_square")], |s, p| {
        kill(s, "skeletal_warden");
        give(s, p, "pc_ada", "warden_blade");
        s.entities
            .get_mut(&EntityId::from("guard_01"))
            .unwrap()
            .room = RoomId::from("smithy");
    });
    let r = w.execute(&ids[0], "report", at(2));
    assert_eq!(r.error_code, Some(ErrorCode::Refused));
    assert!(
        r.narrative.contains("no one from the watch"),
        "{}",
        r.narrative
    );
    assert_eq!(w.fresh_mints(), []);
}
