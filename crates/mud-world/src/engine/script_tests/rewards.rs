//! Ledgered rewards in the engine (ADR 0010), with `FakeHooks`.
//!
//! crypt-v01's `warden_bounty` pool gives 60s and 3 healing draughts a
//! day of the game clock.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use super::*;
use crate::ids::{PoolId, ProtoId};
use crate::rewards::{DAY_MS, MintTotals, PoolFacts, RewardPeriod, RewardPool};
use crate::state::WorldState;

/// Noon of game day 19 000 (in 2022).
const DAY: u64 = 19_000 * DAY_MS + DAY_MS / 2;

fn pool() -> PoolId {
    PoolId::from("warden_bounty")
}

fn draught() -> ProtoId {
    ProtoId::from("healing_draught")
}

fn reward(pool: &str, currency: i64, items: &[(&str, u32)]) -> ScriptEffect {
    ScriptEffect::Reward {
        pool: PoolId::from(pool),
        currency,
        items: items.iter().map(|(p, n)| (ProtoId::from(*p), *n)).collect(),
    }
}

fn bounty() -> ScriptEffect {
    reward("warden_bounty", 20, &[("healing_draught", 1)])
}

/// A world whose verb `beg` (anywhere) answers `fx`, with ada logged in.
fn begging(fx: Vec<ScriptEffect>) -> (Arc<FakeHooks>, World, EntityId) {
    let h = Arc::new(FakeHooks::new(vec![verb("beg")], effects(fx)));
    let mut w = world_with(&h);
    let ada = w.login("ada", 1).unwrap();
    w.drain_fresh();
    w.drain_audit();
    (h, w, ada)
}

fn fault_messages(w: &mut World) -> Vec<String> {
    faults(w)
        .into_iter()
        .filter_map(|a| match a {
            Audit::ScriptFault { message, .. } => Some(message),
            _ => None,
        })
        .collect()
}

fn draughts(w: &World, who: &EntityId) -> Vec<ItemId> {
    w.entity(who)
        .unwrap()
        .inventory
        .iter()
        .copied()
        .filter(|i| w.state.items[i].proto == draught())
        .collect()
}

#[test]
fn a_reward_mints_into_the_ledger_and_tells_only_its_receiver() {
    let (_, mut w, ada) = begging(vec![ScriptEffect::Msg("Coins!".into()), bounty()]);
    let bo = w.login("bo", 1).unwrap();
    w.drain_fresh();
    let money = w.state.total_currency(&w.pack);
    let next = w.state.next_item;
    let purse = w.entity(&ada).unwrap().currency;
    let r = w.execute(&ada, "beg", DAY);
    assert!(r.ok, "{r:?}");
    assert_eq!(
        r.narrative,
        "Coins!\nYou receive 20s and a healing draught."
    );
    assert_eq!(w.entity(&ada).unwrap().currency, purse + 20);
    let made = ItemId(next);
    assert!(draughts(&w, &ada).contains(&made));
    assert!(!w.state.items[&made].stock && w.state.items[&made].paid.is_none());
    // The ledger names everything, and the books balance.
    assert_eq!(w.fresh_mints().len(), 1);
    let m = w.fresh_mints()[0].clone();
    assert_eq!(
        (
            &m.pool,
            m.period,
            m.hook.as_str(),
            &m.entity,
            m.currency,
            m.at_ms
        ),
        (&pool(), 19_000, "verbs.beg", &ada, 20, DAY)
    );
    assert_eq!(m.items.len(), 1);
    assert_eq!((m.items[0].id, &m.items[0].proto), (made, &draught()));
    assert_eq!(w.state.total_currency(&w.pack), money + 20);
    assert_eq!(w.state.minted_currency(), 20);
    assert_eq!(w.state.check(), Vec::<String>::new());
    assert_eq!(
        w.state.check_rewards(&w.pack, w.fresh_mints()),
        Vec::<String>::new()
    );
    let spent = &w.state.pool_spent[&pool()];
    assert_eq!((spent.period, spent.currency), (19_000, 20));
    assert_eq!(spent.items[&draught()], 1);
    // The changes say what was made, from nowhere.
    assert!(r.state_changes.contains(&StateChange::Currency {
        entity: ada.clone(),
        delta: 20,
        balance: purse + 20
    }));
    assert!(r.state_changes.contains(&StateChange::Item {
        item: made,
        proto: draught(),
        from: None,
        to: Some(Holder::Entity {
            entity: ada.clone()
        }),
    }));
    assert!(r.state_changes.contains(&StateChange::Minted {
        pool: pool(),
        entity: ada.clone(),
        currency: 20,
        items: vec![made],
    }));
    // One event, for the receiver only, with the reward in its data.
    let events = w.drain_fresh();
    let ev = events
        .iter()
        .find(|e| e.tags.iter().any(|t| t == "reward"))
        .unwrap();
    assert_eq!(ev.seq, m.seq);
    assert_eq!(ev.recipients, std::slice::from_ref(&ada));
    assert!(!ev.reaches(&bo));
    assert_eq!(ev.kind, EventKind::System);
    assert_eq!(ev.narrative, "You receive 20s and a healing draught.");
    assert!(ev.untrusted_text.is_none());
    let view = ev.view_for(&ada);
    assert_eq!(view["pool"], "warden_bounty");
    assert_eq!(view["currency"], 20);
    assert_eq!(view["items"][0]["proto"], "healing_draught");
    let mut ks = Vec::new();
    keys(&view, &mut ks);
    for bad in FORBIDDEN_KEYS {
        assert!(!ks.iter().any(|k| k == bad), "{bad} in {view}");
    }
    let minted: Vec<Audit> = w
        .drain_audit()
        .into_iter()
        .filter(|a| matches!(a, Audit::Minted { .. }))
        .collect();
    assert_eq!(
        minted,
        [Audit::Minted {
            hook: "verbs.beg".into(),
            entity: ada,
            pool: pool(),
            currency: 20,
            items: vec![draught()],
            at_ms: DAY,
        }]
    );
}

fn declines(w: &mut World) -> Vec<String> {
    w.drain_audit()
        .into_iter()
        .filter_map(|a| match a {
            Audit::ScriptLog { line, .. } => line.strip_prefix("declined: ").map(str::to_owned),
            _ => None,
        })
        .collect()
}

#[test]
fn a_pool_never_passes_its_budget_and_refills_the_next_day() {
    let (_, mut w, ada) = begging(vec![bounty()]);
    for _ in 0..3 {
        assert!(w.execute(&ada, "beg", DAY).ok);
    }
    let purse = w.entity(&ada).unwrap().currency;
    w.drain_audit();
    let r = w.execute(&ada, "beg", DAY + 1);
    assert_eq!(r.error_code, Some(ErrorCode::Refused));
    assert_eq!(r.narrative, DECLINED);
    assert_eq!(r.state_changes, []);
    assert_eq!(
        declines(&mut w),
        ["reward(warden_bounty): asks 20 currency; currency: 0 left today"]
    );
    assert_eq!(w.entity(&ada).unwrap().currency, purse, "never partly");
    assert_eq!(w.fresh_mints().len(), 3);
    assert_eq!(draughts(&w, &ada).len(), 3);
    // A new day of the game clock refills it.
    let r = w.execute(&ada, "beg", DAY + DAY_MS);
    assert!(r.ok, "{r:?}");
    assert_eq!(w.fresh_mints().len(), 4);
    assert_eq!(w.state.pool_spent[&pool()].period, 19_001);
    assert_eq!(w.state.pool_spent[&pool()].currency, 20);
    // Setting the clock back refills nothing: the pool keeps counting in
    // the later day.
    let left = &w.pool_facts(DAY)[&pool()];
    assert_eq!(left.currency, 40);
    assert_eq!(left.items[&draught()], 2);
    assert!(w.execute(&ada, "beg", DAY).ok);
    assert!(w.execute(&ada, "beg", DAY).ok);
    assert_eq!(
        w.execute(&ada, "beg", DAY).error_code,
        Some(ErrorCode::Refused)
    );
    assert_eq!(
        w.state.check_rewards(&w.pack, w.fresh_mints()),
        Vec::<String>::new()
    );
    assert_eq!(
        w.state.minted_currency(),
        w.fresh_mints().iter().map(|m| m.currency).sum::<i64>()
    );
}

#[test]
fn an_empty_pool_never_trips_the_breaker() {
    // Anyone can ask an empty pool on purpose, as often as they like: the
    // hook declines, and stays up for everyone (lemma_declines_never_trip).
    let (_, mut w, ada) = begging(vec![bounty()]);
    let max = HookLimits::default().max_faults;
    for _ in 0..3 {
        assert!(w.execute(&ada, "beg", DAY).ok);
    }
    w.drain_audit();
    for _ in 0..u32::from(max) * 3 {
        let r = w.execute(&ada, "beg", DAY + 1);
        assert_eq!(r.error_code, Some(ErrorCode::Refused));
    }
    assert!(faults(&mut w).is_empty(), "a decline is not a fault");
    assert!(w.state.hook_faults.is_empty());
    assert!(w.hook_status().iter().all(|s| !s.tripped));
    // The next day it pays as usual.
    let r = w.execute(&ada, "beg", DAY + DAY_MS);
    assert!(r.ok, "{r:?}");
    // A decline leaves a real fault count where it was.
    for _ in 0..2 {
        assert!(w.execute(&ada, "beg", DAY + DAY_MS).ok);
    }
    w.state.hook_faults.insert("verbs.beg".into(), max - 1);
    let r = w.execute(&ada, "beg", DAY + DAY_MS);
    assert_eq!(r.error_code, Some(ErrorCode::Refused));
    assert_eq!(w.state.hook_faults["verbs.beg"], max - 1);
}

#[test]
fn an_event_hook_that_declines_applies_nothing_and_says_nothing() {
    let h = Arc::new(FakeHooks::new(
        vec![HookId::Enter(RoomId::from("town_square"))],
        effects(vec![ScriptEffect::Msg("You find a coin.".into()), bounty()]),
    ));
    let mut w = world_with(&h);
    let ada = w.login("ada", 1).unwrap();
    for _ in 0..3 {
        assert!(w.execute(&ada, "go north", DAY).ok);
        let back = w.execute(&ada, "go south", DAY);
        assert!(back.narrative.contains("You find a coin."), "{back:?}");
    }
    w.drain_audit();
    for _ in 0..4 {
        assert!(w.execute(&ada, "go north", DAY).ok);
        let back = w.execute(&ada, "go south", DAY);
        assert!(back.ok, "the walk itself never fails: {back:?}");
        assert!(!back.narrative.contains("You find a coin."));
        assert!(!back.narrative.contains(DECLINED));
    }
    assert_eq!(declines(&mut w).len(), 4);
    assert!(w.state.hook_faults.is_empty());
    assert_eq!(w.fresh_mints().len(), 3);
}

/// An actor with `MARK_CAP` marks.
fn full_of_marks(w: &mut World, who: &EntityId) {
    let e = w.state.entities.get_mut(who).unwrap();
    for i in 0..MARK_CAP {
        e.marks.insert(format!("quest_{i}"), FlagValue::Int(1));
    }
}

#[test]
fn a_mark_that_would_not_fit_declines_the_whole_list() {
    // The once-per-player guard of a reward is a mark. If the actor's marks
    // are full the guard could not be kept, so nothing is minted either.
    let guard = ScriptEffect::Mark("bounty_day".into(), FlagValue::Int(19_000));
    let (_, mut w, ada) = begging(vec![guard.clone(), bounty()]);
    full_of_marks(&mut w, &ada);
    let before = w.snapshot();
    for _ in 0..3 {
        let r = w.execute(&ada, "beg", DAY);
        assert_eq!(r.error_code, Some(ErrorCode::Refused));
        assert_eq!(r.narrative, DECLINED);
    }
    assert_eq!(
        declines(&mut w),
        ["marks full (32); bounty_day would not be kept"; 3]
    );
    assert_eq!(w.fresh_mints(), []);
    let mut after = w.snapshot();
    after.next_seq = before.next_seq;
    assert_eq!(after, before, "nothing applied, no fault counted");
    // A mark the actor already holds fits; so does one after an unmark.
    let room = RoomId::from("town_square");
    let held = ScriptEffect::Mark("quest_3".into(), FlagValue::Int(2));
    assert_eq!(w.check_effects(&room, &ada, &[held, bounty()], DAY), Ok(()));
    let swap = [
        ScriptEffect::Unmark("quest_3".into()),
        guard.clone(),
        bounty(),
    ];
    assert_eq!(w.check_effects(&room, &ada, &swap, DAY), Ok(()));
    let two = [
        ScriptEffect::Unmark("quest_3".into()),
        guard,
        ScriptEffect::Mark("other".into(), FlagValue::Bool(true)),
    ];
    assert_eq!(
        w.check_effects(&room, &ada, &two, DAY),
        Err(Inadmissible::Declined(
            "marks full (32); other would not be kept".into()
        ))
    );
}

#[test]
fn a_flag_that_would_not_fit_declines_the_whole_list() {
    let guard = ScriptEffect::SetFlag("bounty.paid".into(), FlagValue::Int(7));
    let (_, mut w, ada) = begging(vec![guard, bounty()]);
    for i in 0..FLAG_CAP {
        w.state.flags.insert(format!("f{i}"), FlagValue::Bool(true));
    }
    let r = w.execute(&ada, "beg", DAY);
    assert_eq!(r.error_code, Some(ErrorCode::Refused));
    assert_eq!(
        declines(&mut w),
        ["flags full (256); bounty.paid would not be kept"]
    );
    assert!(w.fresh_mints().is_empty() && w.state.pool_spent.is_empty());
    // A flag set already, or cleared first, fits.
    let room = RoomId::from("town_square");
    let set = ScriptEffect::SetFlag("f3".into(), FlagValue::Int(1));
    assert_eq!(w.check_effects(&room, &ada, &[set, bounty()], DAY), Ok(()));
    let swap = [
        ScriptEffect::ClearFlag("f3".into()),
        ScriptEffect::SetFlag("bounty.paid".into(), FlagValue::Int(7)),
    ];
    assert_eq!(w.check_effects(&room, &ada, &swap, DAY), Ok(()));
}

#[test]
fn hook_input_shows_what_is_left_of_each_pool() {
    let (h, mut w, ada) = begging(vec![bounty()]);
    assert!(w.execute(&ada, "beg", DAY).ok);
    assert!(w.execute(&ada, "beg", DAY + 1).ok);
    let inputs = h.inputs.lock().unwrap().clone();
    assert_eq!(
        inputs[0].pools[&pool()],
        PoolFacts {
            per: RewardPeriod::Day,
            currency: 60,
            items: BTreeMap::from([(draught(), 3)]),
        }
    );
    let v = serde_json::to_value(&inputs[1].pools).unwrap();
    assert_eq!(
        v,
        serde_json::json!({ "warden_bounty": { "per": "day", "currency": 40, "items": { "healing_draught": 2 } } })
    );
}

#[test]
fn hooks_see_how_often_each_of_the_cast_has_died() {
    let (h, mut w, ada) = begging(vec![]);
    w.state
        .entities
        .get_mut(&EntityId::from("skeletal_warden"))
        .unwrap()
        .deaths = 4;
    w.execute(&ada, "beg", DAY);
    let input = h.inputs.lock().unwrap().last().cloned().unwrap();
    let warden = &input.cast[&EntityId::from("skeletal_warden")];
    assert_eq!((warden.deaths, warden.alive), (4, true));
    assert_eq!(input.cast[&EntityId::from("guard_01")].deaths, 0);
    // The count is a snapshot field, absent while zero.
    let json = serde_json::to_value(w.snapshot()).unwrap();
    assert_eq!(json["entities"]["skeletal_warden"]["deaths"], 4);
    assert!(json["entities"]["guard_01"].get("deaths").is_none());
}

#[test]
fn a_bad_reward_discards_every_effect() {
    for (fx, why) in [
        (
            reward("no_such", 1, &[]),
            "reward(no_such): no such pool in economy.toml",
        ),
        (
            reward("warden_bounty", 0, &[("iron_ore", 1)]),
            "reward(warden_bounty): the pool offers no iron_ore",
        ),
        (
            reward("warden_bounty", -5, &[]),
            "reward(warden_bounty): currency -5 is negative",
        ),
        (
            reward("warden_bounty", 0, &[]),
            "reward(warden_bounty): it asks for nothing",
        ),
        (
            reward("warden_bounty", 61, &[]),
            "reward(warden_bounty): asks 61 currency; the pool gives at most 60 a day",
        ),
        (
            reward("warden_bounty", 0, &[("healing_draught", 4)]),
            "reward(warden_bounty): asks 4 healing_draught; the pool gives at most 3 a day",
        ),
        (
            reward("warden_bounty", 0, &[("healing_draught", 17)]),
            "reward(warden_bounty): 17 items; one reward makes at most 16",
        ),
    ] {
        let (_, mut w, ada) = begging(vec![
            ScriptEffect::SetFlag("before".into(), FlagValue::Bool(true)),
            fx,
        ]);
        let snapshot = w.snapshot();
        let r = w.execute(&ada, "beg", DAY);
        assert_eq!(r.error_code, Some(ErrorCode::ScriptFailed), "{why}");
        assert_eq!(fault_messages(&mut w), [why.to_owned()]);
        let mut after = w.snapshot();
        after.next_seq = snapshot.next_seq;
        after.hook_faults.clear();
        assert_eq!(after, snapshot, "nothing applied: {why}");
    }
    let (_, mut w, ada) = begging(vec![bounty(), bounty()]);
    assert!(!w.execute(&ada, "beg", DAY).ok);
    assert_eq!(fault_messages(&mut w), ["at most one reward() per hook"]);
    let (_, mut w, ada) = begging(vec![ScriptEffect::Refuse("No.".into()), bounty()]);
    assert!(!w.execute(&ada, "beg", DAY).ok);
    assert_eq!(fault_messages(&mut w), ["refuse() must be the only effect"]);
    assert!(w.fresh_mints().is_empty() && w.state.pool_spent.is_empty());
}

#[test]
fn an_absent_actor_gets_no_reward() {
    let (_, w, ada) = begging(vec![]);
    let mut w2 = w.clone();
    w2.state.entities.get_mut(&ada).unwrap().online = false;
    let room = RoomId::from("town_square");
    assert_eq!(
        w2.check_effects(&room, &ada, &[bounty()], DAY),
        Err(Inadmissible::Fault("the actor is not here".to_owned()))
    );
    assert_eq!(w.check_effects(&room, &ada, &[bounty()], DAY), Ok(()));
}

#[test]
fn a_reward_after_a_deadly_hurt_is_skipped() {
    let h = Arc::new(FakeHooks::new(
        vec![verb("hurtme")],
        effects(vec![ScriptEffect::Hurt(DamageDice::new(6, 0)), bounty()]),
    ));
    let mut w = world_with(&h);
    let ada = w.login("ada", 1).unwrap();
    w.state.entities.get_mut(&ada).unwrap().room = RoomId::from("cellar_landing");
    w.state.entities.get_mut(&ada).unwrap().pools.hp = 1;
    w.fix_dice([6; 40]);
    let money = w.state.total_currency(&w.pack);
    w.drain_audit();
    assert!(w.execute(&ada, "hurtme", DAY).ok);
    assert!(w.fresh_mints().is_empty() && w.state.pool_spent.is_empty());
    assert_eq!(w.state.total_currency(&w.pack), money);
    assert!(w.drain_audit().iter().any(|a| matches!(
        a,
        Audit::ScriptLog { line, .. } if line.starts_with("skipped reward(warden_bounty")
    )));
}

fn pack_with(rewards: &[(&str, RewardPool)]) -> Arc<WorldPack> {
    let mut pack = WorldPack::crypt_v01().unwrap();
    pack.economy.rewards = rewards
        .iter()
        .map(|(id, p)| (PoolId::from(*id), p.clone()))
        .collect();
    Arc::new(pack)
}

#[test]
fn an_ever_pool_never_refills_and_a_dropped_pool_keeps_its_ledger() {
    let relic = RewardPool {
        currency: 5,
        items: BTreeMap::from([(ProtoId::from("bone_charm"), 1)]),
        per: RewardPeriod::Ever,
    };
    let pack = pack_with(&[("relic", relic)]);
    let h = Arc::new(FakeHooks::new(
        vec![verb("beg")],
        effects(vec![reward("relic", 5, &[("bone_charm", 1)])]),
    ));
    let mut w = World::new(Arc::clone(&pack), 3);
    w.set_hooks(
        Some(Arc::clone(&h) as Arc<dyn WorldHooks>),
        1,
        HooksChange::Start,
    );
    let ada = w.login("ada", 1).unwrap();
    let r = w.execute(&ada, "beg", 5);
    assert_eq!(r.narrative, "You receive 5s and a bone charm.");
    assert_eq!(w.state.pool_spent[&PoolId::from("relic")].period, 0);
    w.drain_audit();
    assert!(!w.execute(&ada, "beg", DAY * 3).ok);
    assert_eq!(
        declines(&mut w),
        ["reward(relic): asks 5 currency; currency: 0 left ever"]
    );
    // A store round trip keeps the counts and the totals.
    let saved: WorldState =
        serde_json::from_str(&serde_json::to_string(&w.snapshot()).unwrap()).unwrap();
    assert_eq!(saved, w.snapshot());
    // Under a pack without the pool, the count goes and the totals stay.
    let ledger = w.drain_mints();
    let w2 = World::from_state(pack_with(&[]), saved, Vec::new());
    assert!(w2.state.pool_spent.is_empty());
    assert_eq!(w2.state.minted[&PoolId::from("relic")].mints, 1);
    assert_eq!(w2.state.check(), Vec::<String>::new());
    assert_eq!(
        w2.state.check_rewards(&w2.pack, &ledger),
        Vec::<String>::new()
    );
    assert!(w2.pool_facts(DAY).is_empty());
}

#[test]
fn the_snapshot_keeps_totals_and_the_store_keeps_the_ledger() {
    // A year of daily mints: the snapshot does not grow with the ledger.
    let (_, mut w, ada) = begging(vec![bounty()]);
    assert!(w.execute(&ada, "beg", DAY).ok);
    let size = |w: &World| serde_json::to_string(&w.snapshot()).unwrap().len();
    let mut ledger = w.drain_mints();
    let one = size(&w);
    for d in 1..365 {
        for _ in 0..3 {
            assert!(w.execute(&ada, "beg", DAY + d * DAY_MS).ok);
        }
        // Each commit drains what it saves.
        ledger.extend(w.drain_mints());
        w.state.entities.get_mut(&ada).unwrap().inventory.clear();
        w.state.items.retain(|_, i| i.proto != draught());
    }
    assert_eq!(ledger.len(), 1 + 364 * 3);
    assert_eq!(w.fresh_mints(), []);
    assert_eq!(
        w.state.minted[&pool()],
        MintTotals {
            mints: 1 + 364 * 3,
            currency: 20 * (1 + 364 * 3),
            items: 1 + 364 * 3,
        }
    );
    let json = serde_json::to_value(w.snapshot()).unwrap();
    assert!(
        json.get("mints").is_none(),
        "no ledger rows in the snapshot"
    );
    assert_eq!(json["minted"]["warden_bounty"]["mints"], 1 + 364 * 3);
    assert!(
        size(&w) < one + 64,
        "the snapshot stays the same size: {one} then {}",
        size(&w)
    );
    // The store appends the rows with the events, in one transaction.
    let mut store = crate::store::Store::open_in_memory().unwrap();
    store
        .save("crypt-v01", &w.snapshot(), &w.drain_fresh(), &ledger, 5)
        .unwrap();
    store
        .save("crypt-v01", &w.snapshot(), &[], &ledger[..2], 6)
        .unwrap();
    let back = store.mints().unwrap();
    assert_eq!(back, ledger, "saved once each, in order");
    let loaded = store.load(1).unwrap().unwrap();
    assert_eq!(loaded.state.minted, w.state.minted);
}

#[test]
fn a_game_saves_each_mint_with_its_command() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("world.db");
    let (_, w, ada) = begging(vec![bounty()]);
    let clock = Arc::new(crate::game::ManualClock::new(DAY));
    let store = crate::store::Store::open(&db).unwrap();
    let game = crate::game::Game::new(w, Some(store), clock);
    assert!(game.execute(&ada, "beg").ok);
    assert!(game.execute(&ada, "beg").ok);
    assert!(game.read(|w| w.fresh_mints().is_empty()), "drained on save");
    assert_eq!(game.store_errors(), 0);
    drop(game);
    let store = crate::store::Store::open(&db).unwrap();
    let ledger = store.mints().unwrap();
    assert_eq!(ledger.len(), 2);
    assert!(ledger.iter().all(|m| m.entity == ada && m.currency == 20));
    let state = store.load(1).unwrap().unwrap().state;
    assert_eq!(
        state.check_rewards(&pack_with_bounty(), &ledger),
        Vec::<String>::new()
    );
}

fn pack_with_bounty() -> WorldPack {
    WorldPack::crypt_v01().unwrap()
}

#[test]
fn the_checkers_find_a_ledger_that_does_not_add_up() {
    let (_, mut w, ada) = begging(vec![bounty()]);
    assert!(w.execute(&ada, "beg", DAY).ok);
    let ledger = w.fresh_mints().to_vec();
    let mut s = w.snapshot();
    s.pool_spent.get_mut(&pool()).unwrap().currency = 70;
    let bad = s.check_rewards(&w.pack, &ledger);
    assert!(
        bad.contains(&"pool warden_bounty gave 70 currency".to_owned()),
        "{bad:?}"
    );
    assert!(
        bad.contains(&"pool warden_bounty counts 70 currency; its ledger 20".to_owned()),
        "{bad:?}"
    );
    let mut s = w.snapshot();
    s.pool_spent
        .get_mut(&pool())
        .unwrap()
        .items
        .insert(draught(), 4);
    let bad = s.check_rewards(&w.pack, &ledger);
    assert!(
        bad.contains(&"pool warden_bounty made 4 healing_draught".to_owned()),
        "{bad:?}"
    );
    assert!(
        bad.contains(&"pool warden_bounty counts 4 healing_draught; its ledger 1".to_owned()),
        "{bad:?}"
    );
    let mut s = w.snapshot();
    s.pool_spent
        .insert(PoolId::from("gone"), s.pool_spent[&pool()].clone());
    assert!(
        s.check_rewards(&w.pack, &ledger)
            .contains(&"pool gone is spent but not in the pack".to_owned())
    );
    let s = w.snapshot();
    let twice = [ledger[0].clone(), ledger[0].clone()];
    let bad = s.check_rewards(&w.pack, &twice);
    assert!(
        bad.iter().any(|m| m.contains("the ledger names")),
        "{bad:?}"
    );
    assert!(
        bad.contains(
            &"pool warden_bounty totals 1 mints, 20 currency, 1 items; its ledger 2, 40, 2"
                .to_owned()
        ),
        "{bad:?}"
    );
}

/// What `beg` asks for at game time `now`: varied, sometimes past the
/// budget, sometimes nothing (a fault).
fn ask_at(_: &HookId, input: &HookInput, dice: WorldDice) -> HookReply {
    let n = input.now_ms / 1_000;
    let items = BTreeMap::from([(draught(), u32::try_from(n % 3).unwrap())]);
    let currency = i64::try_from(n % 37).unwrap();
    HookReply {
        outcome: Ok((
            vec![ScriptEffect::Reward {
                pool: pool(),
                currency,
                items,
            }],
            dice,
        )),
        log: Vec::new(),
    }
}

const LINES: [&str; 9] = [
    "beg",
    "beg",
    "give 5s to bo",
    "give draught to bo",
    "drop draught",
    "take draught",
    "use draught",
    "say hi",
    "look",
];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]
    #[test]
    fn money_and_items_are_conserved_except_for_ledgered_mints(
        steps in prop::collection::vec((0usize..LINES.len(), 0usize..2, 0u64..4, 0u64..40_000), 1..120),
    ) {
        let h = Arc::new(FakeHooks::new(vec![verb("beg")], ask_at));
        let mut w = world_with(&h);
        let ids = [w.login("ada", 1).unwrap(), w.login("bo", 1).unwrap()];
        let money = w.state.total_currency(&w.pack);
        let start_next = w.state.next_item;
        let start_items = w.state.items.len();
        let mut now = DAY;
        for (line, who, jump, ms) in steps {
            // Mostly forward, by up to a day; now and then back a day.
            now = match jump {
                0 => now.saturating_sub(DAY_MS),
                1 => now + DAY_MS,
                _ => now + ms * 1_000,
            };
            w.execute(&ids[who], LINES[line], now);
            let bad = w.state.check();
            prop_assert!(bad.is_empty(), "{:?}", bad);
            let bad = w.state.check_rewards(&w.pack, w.fresh_mints());
            prop_assert!(bad.is_empty(), "{:?}", bad);
            prop_assert_eq!(
                w.state.total_currency(&w.pack),
                money + w.state.minted_currency(),
                "money is conserved except for ledgered mints"
            );
            let ledger: BTreeSet<ItemId> = w
                .fresh_mints()
                .iter()
                .flat_map(|m| m.items.iter().map(|i| i.id))
                .collect();
            prop_assert_eq!(
                w.state.next_item - start_next,
                u64::try_from(ledger.len()).unwrap(),
                "every new item id is a ledgered mint"
            );
            for id in w.state.items.keys().filter(|i| i.0 >= start_next) {
                prop_assert!(ledger.contains(id), "{} was made outside the ledger", id);
            }
            prop_assert!(w.state.items.len() <= start_items + ledger.len());
        }
    }
}
