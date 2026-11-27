//! `use_up()`: a hook takes items the actor carries out of the world
//! (ADR 0010 amendment), with `FakeHooks`.
//!
//! A fresh player carries a dagger and a torch, both equipped, and a
//! bandage. Hale's bounty uses the effect for the warden's blade.

use std::collections::BTreeMap;

use mud_rules::DamageDice;

use super::super::script::{HookCall, HookRun};
use super::*;
use crate::hooks::USE_UP_MAX;
use crate::ids::{PoolId, ProtoId};
use crate::pack::Slot;
use crate::rewards::DAY_MS;

/// Noon of game day 19 000 (in 2022).
const DAY: u64 = 19_000 * DAY_MS + DAY_MS / 2;

fn use_up(proto: &str, count: u32) -> ScriptEffect {
    ScriptEffect::UseUp {
        proto: ProtoId::from(proto),
        count,
    }
}

fn bounty() -> ScriptEffect {
    ScriptEffect::Reward {
        pool: PoolId::from("warden_bounty"),
        currency: 20,
        items: BTreeMap::from([(ProtoId::from("healing_draught"), 1)]),
    }
}

/// A world whose verb `hand` (anywhere) rolls one die and answers `fx`,
/// with ada logged in at the square.
fn handing(fx: Vec<ScriptEffect>) -> (Arc<FakeHooks>, World, EntityId) {
    let h = Arc::new(FakeHooks::new(vec![verb("hand")], roll_then(fx, 1)));
    let mut w = world_with(&h);
    let ada = w.login("ada", 1).unwrap();
    w.drain_fresh();
    w.drain_audit();
    (h, w, ada)
}

fn square() -> RoomId {
    RoomId::from("town_square")
}

/// Gives `who` a new item of `proto`, outside any ledger (a test fixture).
fn give(w: &mut World, who: &EntityId, proto: &str) -> ItemId {
    let pack = Arc::clone(&w.pack);
    let id = w.state.new_item(&ProtoId::from(proto), &pack);
    w.state.attach(
        id,
        &Holder::Entity {
            entity: who.clone(),
        },
    );
    id
}

fn carried(w: &World, who: &EntityId, proto: &str) -> Vec<ItemId> {
    w.entity(who)
        .unwrap()
        .inventory
        .iter()
        .copied()
        .filter(|i| w.state.items[i].proto.as_str() == proto)
        .collect()
}

fn equipped(w: &World, who: &EntityId, slot: Slot) -> Option<ItemId> {
    w.entity(who).unwrap().equipped.get(&slot).copied()
}

fn declined(msg: &str) -> Result<(), Inadmissible> {
    Err(Inadmissible::Declined(msg.to_owned()))
}

fn faulted(msg: &str) -> Result<(), Inadmissible> {
    Err(Inadmissible::Fault(msg.to_owned()))
}

fn logs(w: &mut World) -> Vec<String> {
    w.drain_audit()
        .into_iter()
        .filter_map(|a| match a {
            Audit::ScriptLog { line, .. } => Some(line),
            _ => None,
        })
        .collect()
}

// ------------------------------------------------------------ admission

#[test]
fn use_up_is_admitted_for_what_the_actor_carries_and_declined_past_it() {
    let (_, mut w, ada) = handing(vec![]);
    let room = square();
    assert_eq!(
        w.check_effects(&room, &ada, &[use_up("bandage", 1)], DAY),
        Ok(())
    );
    assert_eq!(
        w.check_effects(&room, &ada, &[use_up("bandage", 2)], DAY),
        declined("use_up(bandage): carries 1, asks 2")
    );
    assert_eq!(
        w.check_effects(&room, &ada, &[use_up("iron_ore", 1)], DAY),
        declined("use_up(iron_ore): carries 0, asks 1")
    );
    // Two calls of one prototype count together, against the state
    // before the list.
    let twice = [use_up("bandage", 1), use_up("bandage", 1)];
    assert_eq!(
        w.check_effects(&room, &ada, &twice, DAY),
        declined("use_up(bandage): carries 1, asks 2")
    );
    give(&mut w, &ada, "bandage");
    assert_eq!(w.check_effects(&room, &ada, &twice, DAY), Ok(()));
    // An equipped item counts: it is taken out of the hand.
    assert_eq!(
        w.check_effects(&room, &ada, &[use_up("torch", 1), use_up("dagger", 1)], DAY),
        Ok(())
    );
    // The most one call takes.
    for _ in 0..USE_UP_MAX {
        give(&mut w, &ada, "iron_ore");
    }
    assert_eq!(
        w.check_effects(&room, &ada, &[use_up("iron_ore", USE_UP_MAX)], DAY),
        Ok(())
    );
}

#[test]
fn a_bad_use_up_is_a_script_fault() {
    let (_, w, ada) = handing(vec![]);
    let room = square();
    for (fx, why) in [
        (
            vec![use_up("no_such", 1)],
            "use_up(no_such): no such item in items.toml",
        ),
        (
            vec![use_up("rat_teeth", 1)],
            "use_up(rat_teeth): a hidden item",
        ),
        (
            vec![use_up("old_coin_pouch", 1)],
            "use_up(old_coin_pouch): it holds money; a purse is emptied with use",
        ),
        (
            vec![use_up("dropped_purse", 1)],
            "use_up(dropped_purse): it holds money; a purse is emptied with use",
        ),
        (
            vec![use_up("bandage", 0)],
            "use_up(bandage): n is a count of 1 to 16",
        ),
        (
            vec![use_up("bandage", 17)],
            "use_up(bandage): n is a count of 1 to 16",
        ),
        (
            vec![bounty(), use_up("bandage", 1)],
            "use_up() comes before reward()",
        ),
    ] {
        assert_eq!(
            w.check_effects(&room, &ada, &fx, DAY),
            faulted(why),
            "{why}"
        );
    }
    let mut away = w;
    away.state.entities.get_mut(&ada).unwrap().online = false;
    assert_eq!(
        away.check_effects(&room, &ada, &[use_up("bandage", 1)], DAY),
        faulted("the actor is not here")
    );
    // Through a command, a fault fails it and counts on the breaker.
    let (_, mut w, ada) = handing(vec![use_up("rat_teeth", 1)]);
    let before = w.snapshot().dice_state;
    let r = w.execute(&ada, "hand", DAY);
    assert_eq!(r.error_code, Some(ErrorCode::ScriptFailed));
    assert_eq!(w.state.hook_faults["verbs.hand"], 1);
    assert_eq!(w.snapshot().dice_state, before);
}

#[test]
fn a_hurt_between_use_up_and_reward_is_a_script_fault() {
    // A fatal hurt there would take the price and skip the pay, and a
    // player keeps their gear on death. So admission asks for the hurt
    // first.
    let hurt = || ScriptEffect::Hurt(DamageDice::new(6, 0));
    let (_, mut w, ada) = handing(vec![]);
    let room = RoomId::from("cellar_landing");
    w.state.entities.get_mut(&ada).unwrap().room = room.clone();
    assert_eq!(
        w.check_effects(&room, &ada, &[use_up("bandage", 1), hurt(), bounty()], DAY),
        faulted("hurt() comes before use_up()")
    );
    assert_eq!(
        w.check_effects(&room, &ada, &[hurt(), use_up("bandage", 1), bounty()], DAY),
        Ok(())
    );
    // With no reward after it, a hurt after a use_up costs no pay.
    assert_eq!(
        w.check_effects(&room, &ada, &[use_up("bandage", 1), hurt()], DAY),
        Ok(())
    );
    assert_eq!(
        w.check_effects(&room, &ada, &[bounty(), use_up("bandage", 1), hurt()], DAY),
        faulted("use_up() comes before reward()")
    );
    // Through a command, the faulted list keeps the item and pays nothing.
    let (_, mut w, ada) = handing(vec![use_up("bandage", 1), hurt(), bounty()]);
    w.state.entities.get_mut(&ada).unwrap().room = room;
    w.state.entities.get_mut(&ada).unwrap().pools.hp = 1;
    w.fix_dice([6; 40]);
    let r = w.execute(&ada, "hand", DAY);
    assert_eq!(r.error_code, Some(ErrorCode::ScriptFailed));
    assert_eq!(carried(&w, &ada, "bandage").len(), 1);
    assert!(w.fresh_mints().is_empty() && w.state.pool_spent.is_empty());
}

#[test]
fn contents_money_and_merchandise_are_never_used_up() {
    let (_, mut w, ada) = handing(vec![]);
    let room = square();
    // A container that holds something is not a candidate; an empty one is.
    let lockbox = give(&mut w, &ada, "rusted_lockbox");
    assert_eq!(
        w.check_effects(&room, &ada, &[use_up("rusted_lockbox", 1)], DAY),
        Ok(())
    );
    let pack = Arc::clone(&w.pack);
    let ring = w.state.new_item(&ProtoId::from("silver_ring"), &pack);
    w.state.attach(ring, &Holder::Container { item: lockbox });
    assert_eq!(
        w.check_effects(&room, &ada, &[use_up("rusted_lockbox", 1)], DAY),
        declined("use_up(rusted_lockbox): carries 0, asks 1")
    );
    // An item that holds coins of its own (as a dropped purse does) is not.
    let coins = give(&mut w, &ada, "silver_ring");
    w.state.items.get_mut(&coins).unwrap().currency = Some(5);
    assert_eq!(
        w.check_effects(&room, &ada, &[use_up("silver_ring", 1)], DAY),
        declined("use_up(silver_ring): carries 0, asks 1")
    );
    // Nor is merchandise.
    let bandage = carried(&w, &ada, "bandage")[0];
    w.state.items.get_mut(&bandage).unwrap().stock = true;
    assert_eq!(
        w.check_effects(&room, &ada, &[use_up("bandage", 1)], DAY),
        declined("use_up(bandage): carries 0, asks 1")
    );
}

#[test]
fn a_declined_hand_in_applies_none_of_its_list_and_never_trips_the_breaker() {
    let (_, mut w, ada) = handing(vec![
        ScriptEffect::SetFlag("bounty.paid".into(), FlagValue::Int(1)),
        ScriptEffect::Mark("bounty_day".into(), FlagValue::Int(19_000)),
        ScriptEffect::Msg("Hale keeps it.".into()),
        use_up("iron_ore", 1),
        bounty(),
    ]);
    let before = w.snapshot();
    let max = HookLimits::default().max_faults;
    for _ in 0..u32::from(max) * 3 {
        let r = w.execute(&ada, "hand", DAY);
        assert_eq!(r.error_code, Some(ErrorCode::Refused), "{r:?}");
        assert_eq!(r.narrative, DECLINED);
        assert_eq!(r.state_changes, []);
    }
    assert!(faults(&mut w).is_empty(), "a decline is not a fault");
    let mut after = w.snapshot();
    assert_eq!(
        after.dice_state, before.dice_state,
        "the dice are untouched"
    );
    after.next_seq = before.next_seq;
    assert_eq!(after, before, "no flag, no mark, no reward, no fault");
    assert_eq!(w.fresh_mints(), []);
}

// ------------------------------------------------------------ applying

#[test]
fn use_up_takes_a_spare_before_the_one_in_hand() {
    let (_, mut w, ada) = handing(vec![use_up("dagger", 1)]);
    let held = equipped(&w, &ada, Slot::Weapon).unwrap();
    let spare = give(&mut w, &ada, "dagger");
    let r = w.execute(&ada, "hand", DAY);
    assert!(r.ok, "{r:?}");
    assert_eq!(r.narrative, "You give up a dagger.");
    assert_eq!(carried(&w, &ada, "dagger"), [held]);
    assert_eq!(equipped(&w, &ada, Slot::Weapon), Some(held));
    assert!(!w.state.items.contains_key(&spare));
    assert!(
        !r.state_changes
            .iter()
            .any(|c| matches!(c, StateChange::Equipped { .. })),
        "{:?}",
        r.state_changes
    );
}

#[test]
fn an_item_in_hand_is_put_away_then_used_up() {
    let (_, mut w, ada) = handing(vec![use_up("torch", 1)]);
    let bo = w.login("bo", 1).unwrap();
    w.drain_fresh();
    let torch = equipped(&w, &ada, Slot::Light).unwrap();
    let money = w.state.total_currency(&w.pack);
    let items = w.state.items.len();
    let r = w.execute(&ada, "hand", DAY);
    assert!(r.ok, "{r:?}");
    assert_eq!(r.narrative, "You give up a pitch torch.");
    let off = r.state_changes.iter().position(|c| {
        *c == StateChange::Equipped {
            entity: ada.clone(),
            slot: Slot::Light,
            item: None,
        }
    });
    let gone = r.state_changes.iter().position(|c| {
        *c == StateChange::Item {
            item: torch,
            proto: ProtoId::from("torch"),
            from: Some(Holder::Entity {
                entity: ada.clone(),
            }),
            to: None,
        }
    });
    assert!(off.unwrap() < gone.unwrap(), "{:?}", r.state_changes);
    assert_eq!(equipped(&w, &ada, Slot::Light), None);
    assert_eq!(carried(&w, &ada, "torch"), Vec::<ItemId>::new());
    assert_eq!(w.state.check(), Vec::<String>::new());
    assert_eq!(w.state.total_currency(&w.pack), money);
    assert_eq!(w.state.items.len(), items - 1);
    // One event, for the actor only, with the ids in its data.
    let events = w.drain_fresh();
    let ev = events
        .iter()
        .find(|e| e.tags.iter().any(|t| t == "use_up"))
        .unwrap();
    assert_eq!(ev.kind, EventKind::System);
    assert_eq!(ev.recipients, std::slice::from_ref(&ada));
    assert!(!ev.reaches(&bo));
    assert_eq!(ev.verb.as_deref(), Some("use_up"));
    assert_eq!(ev.tags, ["script", "verbs.hand", "use_up"]);
    assert_eq!(ev.narrative, "You give up a pitch torch.");
    assert!(ev.untrusted_text.is_none());
    assert_eq!(ev.data["proto"], "torch");
    assert_eq!(
        ev.data["items"],
        serde_json::json!([{ "id": torch, "proto": "torch" }])
    );
    let view = ev.view_for(&ada);
    let mut ks = Vec::new();
    keys(&view, &mut ks);
    for bad in FORBIDDEN_KEYS {
        assert!(!ks.iter().any(|k| k == bad), "{bad} in {view}");
    }
}

#[test]
fn a_hand_in_gives_up_the_price_then_pays_in_list_order() {
    let (_, mut w, ada) = handing(vec![
        ScriptEffect::Msg("Torin weighs the ore.".into()),
        use_up("iron_ore", 3),
        bounty(),
    ]);
    for _ in 0..4 {
        give(&mut w, &ada, "iron_ore");
    }
    let money = w.state.total_currency(&w.pack);
    let items = w.state.items.len();
    let r = w.execute(&ada, "hand", DAY);
    assert!(r.ok, "{r:?}");
    assert_eq!(
        r.narrative,
        "Torin weighs the ore.\nYou give up 3 x chunk of dwarven iron ore.\nYou receive 20s and a healing draught."
    );
    assert_eq!(carried(&w, &ada, "iron_ore").len(), 1);
    assert_eq!(w.state.items.len(), items - 3 + 1);
    assert_eq!(w.state.total_currency(&w.pack), money + 20);
    assert_eq!(w.state.check(), Vec::<String>::new());
    assert_eq!(
        w.state.check_rewards(&w.pack, w.fresh_mints()),
        Vec::<String>::new()
    );
    let effects: Vec<Vec<String>> = w
        .drain_audit()
        .into_iter()
        .filter_map(|a| match a {
            Audit::ScriptEffects { effects, .. } => Some(effects),
            _ => None,
        })
        .collect();
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0][1], "use_up(iron_ore, 3)");
}

#[test]
fn a_price_gone_by_apply_time_skips_the_reward() {
    // Admission checked the price; were it gone by the time the list
    // applies, the reward would not pay without it.
    let (_, mut w, ada) = handing(vec![]);
    let mut ctx = Ctx {
        now: DAY,
        ..Ctx::default()
    };
    let call = HookCall::new(verb("hand"), square());
    let run = w.apply_effects(
        &mut ctx,
        &ada,
        &call,
        "verbs.hand",
        vec![use_up("iron_ore", 1), bounty()],
    );
    assert!(
        matches!(&run, HookRun::Applied { msgs, refused: None } if msgs.is_empty()),
        "{run:?}"
    );
    assert!(w.fresh_mints().is_empty() && w.state.pool_spent.is_empty());
    assert_eq!(ctx.changes, []);
    assert_eq!(
        logs(&mut w),
        [
            "skipped use_up(iron_ore): the actor no longer carries it",
            "skipped reward(warden_bounty): its price was not paid",
        ]
    );
}

#[test]
fn the_reward_line_reads_as_before() {
    let (_, w, _) = handing(vec![]);
    let items = |list: &[(&str, u32)]| -> BTreeMap<ProtoId, u32> {
        list.iter().map(|(p, n)| (ProtoId::from(*p), *n)).collect()
    };
    assert_eq!(
        w.reward_line(20, &items(&[("healing_draught", 1)])),
        "You receive 20s and a healing draught."
    );
    assert_eq!(
        w.reward_line(5, &items(&[("healing_draught", 1), ("iron_ore", 2)])),
        "You receive 5s, a healing draught and 2 x chunk of dwarven iron ore."
    );
    assert_eq!(
        w.reward_line(0, &items(&[("iron_ore", 2), ("ale", 0)])),
        "You receive 2 x chunk of dwarven iron ore."
    );
    assert_eq!(w.reward_line(7, &items(&[])), "You receive 7s.");
    assert_eq!(
        w.item_phrase(&items(&[("ale", 1), ("iron_ore", 1)])),
        "a mug of ale and a chunk of dwarven iron ore"
    );
}

#[test]
fn use_up_replays_the_same_after_a_store_round_trip() {
    let (h, mut w, ada) = handing(vec![use_up("torch", 1), bounty()]);
    let torch = equipped(&w, &ada, Slot::Light).unwrap();
    let mut store = crate::store::Store::open_in_memory(1, 0).unwrap();
    store
        .save("crypt-v01", &w.snapshot(), &w.drain_fresh(), &[], 5)
        .unwrap();
    let loaded = store.load(1).unwrap().unwrap().state;
    let reload = || {
        let mut twin = World::from_state(Arc::clone(&w.pack), loaded.clone(), Vec::new());
        twin.set_hooks(
            Some(Arc::clone(&h) as Arc<dyn WorldHooks>),
            1,
            HooksChange::Start,
        );
        if !twin.entity(&ada).unwrap().online {
            twin.bring_online(&ada, 1);
        }
        twin.drain_fresh();
        twin
    };
    let (mut one, mut two) = (reload(), reload());
    let live = w.execute(&ada, "hand", DAY);
    let first = one.execute(&ada, "hand", DAY);
    let second = two.execute(&ada, "hand", DAY);
    assert!(live.ok, "{live:?}");
    // The reloaded world does what the live one did.
    assert_eq!(
        (&live.narrative, &live.state_changes),
        (&first.narrative, &first.state_changes)
    );
    assert!(!one.state.items.contains_key(&torch));
    // Two reloads do exactly the same.
    assert_eq!(first, second);
    assert_eq!(one.snapshot(), two.snapshot());
    assert_eq!(one.fresh_mints(), two.fresh_mints());
    assert_eq!(one.drain_fresh(), two.drain_fresh());
    // The saved world no longer holds the torch, and its books balance.
    let ledger = w.drain_mints();
    store
        .save("crypt-v01", &w.snapshot(), &w.drain_fresh(), &ledger, 6)
        .unwrap();
    let back = store.load(1).unwrap().unwrap().state;
    assert_eq!(back.check(), Vec::<String>::new());
    assert_eq!(
        back.check_rewards(&w.pack, &store.mints().unwrap()),
        Vec::<String>::new()
    );
    assert!(!back.items.contains_key(&torch));
    assert!(!back.entities[&ada].inventory.contains(&torch));
    assert_eq!(back.entities[&ada].equipped.get(&Slot::Light), None);
}
