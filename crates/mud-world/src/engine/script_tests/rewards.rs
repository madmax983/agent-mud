//! Ledgered rewards in the engine (ADR 0010), with `FakeHooks`.
//!
//! crypt-v01's `warden_bounty` pool gives 60s and 3 healing draughts a
//! day of the game clock.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use super::*;
use crate::ids::{PoolId, ProtoId};
use crate::rewards::{DAY_MS, PoolFacts, RewardPeriod, RewardPool};
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
    assert_eq!(w.state.mints.len(), 1);
    let m = w.state.mints[0].clone();
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
    assert_eq!(w.state.check_rewards(&w.pack), Vec::<String>::new());
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

#[test]
fn a_pool_never_passes_its_budget_and_refills_the_next_day() {
    let (_, mut w, ada) = begging(vec![bounty()]);
    for _ in 0..3 {
        assert!(w.execute(&ada, "beg", DAY).ok);
    }
    let purse = w.entity(&ada).unwrap().currency;
    let r = w.execute(&ada, "beg", DAY + 1);
    assert_eq!(r.error_code, Some(ErrorCode::ScriptFailed));
    assert_eq!(r.state_changes, []);
    assert_eq!(
        fault_messages(&mut w).last().unwrap(),
        "reward(warden_bounty): asks 20 currency; currency: 0 left today"
    );
    assert_eq!(w.entity(&ada).unwrap().currency, purse, "never partly");
    assert_eq!(w.state.mints.len(), 3);
    assert_eq!(draughts(&w, &ada).len(), 3);
    // A new day of the game clock refills it.
    let r = w.execute(&ada, "beg", DAY + DAY_MS);
    assert!(r.ok, "{r:?}");
    assert_eq!(w.state.mints.len(), 4);
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
        Some(ErrorCode::ScriptFailed)
    );
    assert_eq!(w.state.check_rewards(&w.pack), Vec::<String>::new());
    assert_eq!(
        w.state.minted_currency(),
        w.state.mints.iter().map(|m| m.currency).sum::<i64>()
    );
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
            "reward(warden_bounty): asks 61 currency; currency: 60 left today",
        ),
        (
            reward("warden_bounty", 0, &[("healing_draught", 4)]),
            "reward(warden_bounty): asks 4 healing_draught; healing_draught: 3 left today",
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
    assert!(w.state.mints.is_empty() && w.state.pool_spent.is_empty());
}

#[test]
fn an_absent_actor_gets_no_reward() {
    let (_, w, ada) = begging(vec![]);
    let mut w2 = w.clone();
    w2.state.entities.get_mut(&ada).unwrap().online = false;
    let room = RoomId::from("town_square");
    assert_eq!(
        w2.check_effects(&room, &ada, &[bounty()], DAY),
        Err("the actor is not here".to_owned())
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
    assert!(w.state.mints.is_empty() && w.state.pool_spent.is_empty());
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
        fault_messages(&mut w),
        ["reward(relic): asks 5 currency; currency: 0 left ever"]
    );
    // A store round trip keeps the ledger and the counts.
    let saved: WorldState =
        serde_json::from_str(&serde_json::to_string(&w.snapshot()).unwrap()).unwrap();
    assert_eq!(saved, w.snapshot());
    // Under a pack without the pool, the count goes and the ledger stays.
    let w2 = World::from_state(pack_with(&[]), saved, Vec::new());
    assert!(w2.state.pool_spent.is_empty());
    assert_eq!(w2.state.mints.len(), 1);
    assert_eq!(w2.state.check(), Vec::<String>::new());
    assert_eq!(w2.state.check_rewards(&w2.pack), Vec::<String>::new());
    assert!(w2.pool_facts(DAY).is_empty());
}

#[test]
fn the_checkers_find_a_ledger_that_does_not_add_up() {
    let (_, mut w, ada) = begging(vec![bounty()]);
    assert!(w.execute(&ada, "beg", DAY).ok);
    let mut s = w.snapshot();
    s.pool_spent.get_mut(&pool()).unwrap().currency = 70;
    let bad = s.check_rewards(&w.pack);
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
    let bad = s.check_rewards(&w.pack);
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
        s.check_rewards(&w.pack)
            .contains(&"pool gone is spent but not in the pack".to_owned())
    );
    let mut s = w.snapshot();
    let again = s.mints[0].clone();
    s.mints.push(again);
    assert!(
        s.check().iter().any(|m| m.contains("the ledger names")),
        "{:?}",
        s.check()
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
            let bad = w.state.check_rewards(&w.pack);
            prop_assert!(bad.is_empty(), "{:?}", bad);
            prop_assert_eq!(
                w.state.total_currency(&w.pack),
                money + w.state.minted_currency(),
                "money is conserved except for ledgered mints"
            );
            let ledger: BTreeSet<ItemId> = w
                .state
                .mints
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
