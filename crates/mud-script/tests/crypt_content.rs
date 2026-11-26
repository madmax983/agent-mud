//! crypt-v01's first content pack, played through the real engine: the
//! root cellar, `search`, liar's bones, the rumor of the day and the dwarf
//! drift. Every action is a parsed line through `World::execute`; setup
//! places bodies and gear, as the other script tests do.
#![expect(clippy::unwrap_used, reason = "test helpers fail loudly")]

use std::collections::BTreeMap;
use std::sync::Arc;

use mud_script::ScriptHost;
use mud_world::hooks::{FLAG_CAP, FlagValue, WorldHooks};
use mud_world::pack::Slot;
use mud_world::rewards::{MintRecord, PoolSpent};
use mud_world::state::{Holder, WorldState};
use mud_world::{
    EntityId, ErrorCode, EventKind, HooksChange, PoolId, ProtoId, RoomId, World, WorldPack,
};

fn pack() -> Arc<WorldPack> {
    Arc::new(WorldPack::crypt_v01().unwrap())
}

/// The first UTC day of [`at`], 2023-11-14.
const DAY0: u64 = 19_675;

/// 2023-11-14 at `hour`:00 UTC; hours past 23 run into the next days.
const fn at(hour: u64) -> u64 {
    DAY0 * 86_400_000 + hour * 3_600_000
}

/// Opens `state` with the shipped scripts and brings `ids` online.
fn open(state: WorldState, ids: &[EntityId]) -> World {
    let pack = pack();
    let mut w = World::from_state(Arc::clone(&pack), state, Vec::new());
    let host = Arc::new(ScriptHost::bundled_crypt_v01(&pack).unwrap());
    w.set_hooks(Some(host as Arc<dyn WorldHooks>), at(1), HooksChange::Start);
    for id in ids {
        w.bring_online(id, at(1));
    }
    w.drain_fresh();
    w
}

/// A world with players `names` logged in at their rooms; `edit` changes
/// the state first.
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
    (open(state, &ids), ids)
}

/// The world again, after `edit` (a mob put down once more).
fn again(w: &World, ids: &[EntityId], edit: impl FnOnce(&mut WorldState)) -> World {
    let mut state = w.snapshot();
    edit(&mut state);
    open(state, ids)
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

fn narrations(w: &mut World) -> Vec<String> {
    w.drain_fresh()
        .into_iter()
        .filter(|e| e.kind == EventKind::System)
        .map(|e| e.narrative)
        .collect()
}

/// What `who` perceived of the fresh events.
fn seen_by(w: &mut World, who: &EntityId) -> Vec<String> {
    w.drain_fresh()
        .into_iter()
        .filter(|e| e.reaches(who))
        .map(|e| e.narrative)
        .collect()
}

fn carried(w: &World, who: &EntityId, proto: &str) -> usize {
    w.entity(who)
        .unwrap()
        .inventory
        .iter()
        .filter(|i| w.state().items[*i].proto.as_str() == proto)
        .count()
}

fn flag(w: &World, name: &str) -> Option<FlagValue> {
    w.state().flags.get(name).cloned()
}

/// Dice for a successful 3d6 roll (1, 1, 1), then the twelve dice a
/// script's `below(100)` reads, made to give `r`.
fn won_then_below(r: u32) -> Vec<u8> {
    rolled_then_below([1, 1, 1], r)
}

/// The 3d6 roll `roll`, then the twelve dice of a `below(100)` giving `r`.
fn rolled_then_below(roll: [u8; 3], r: u32) -> Vec<u8> {
    let mut faces = roll.to_vec();
    let mut digits = Vec::new();
    let mut n = r;
    for _ in 0..12 {
        digits.push(u8::try_from(n % 6).unwrap() + 1);
        n /= 6;
    }
    digits.reverse();
    faces.extend(digits);
    faces
}

/// Puts out `who`'s light: the torch stays in the pack, unlit.
fn douse(state: &mut WorldState, who: &str) {
    let e = state.entities.get_mut(&EntityId::from(who)).unwrap();
    e.equipped.remove(&Slot::Light);
}

fn spent_today(state: &mut WorldState, pool: &str, currency: u64) {
    state.pool_spent.insert(
        PoolId::from(pool),
        PoolSpent {
            period: DAY0,
            currency,
            items: BTreeMap::new(),
        },
    );
}

// ---------------------------------------------------------------- search

#[test]
fn search_pays_each_death_once_and_again_after_the_next() {
    let names = [("ada", "wine_vault"), ("bo", "wine_vault")];
    let (mut w, ids) = world_with(&names, |_, _| {});
    let r = w.execute(&ids[0], "search", at(2));
    assert_eq!(r.error_code, Some(ErrorCode::Refused), "{r:?}");
    assert!(r.narrative.contains("teeth still snapping"), "{r:?}");
    assert_eq!(
        w.execute(&ids[0], "search", at(2)).error_code,
        Some(ErrorCode::Refused)
    );

    let (mut w, ids) = world_with(&names, |s, _| kill(s, "giant_rat_1"));
    let money = w.state().total_currency(w.pack());
    // 30 of 0-99 falls on the rat table's 2s.
    w.fix_dice(won_then_below(30));
    let r = w.execute(&ids[0], "search", at(2));
    assert!(r.ok, "{r:?}");
    assert!(r.narrative.ends_with("You receive 2s."), "{}", r.narrative);
    assert_eq!(flag(&w, "loot.giant_rat_1"), Some(FlagValue::Int(1)));
    assert_eq!(
        seen_by(&mut w, &ids[1]),
        ["ada searches the giant rat's remains, and pockets something."]
    );
    let mint = w.fresh_mints()[0].clone();
    assert_eq!(
        (mint.pool.as_str(), mint.hook.as_str(), mint.currency),
        ("vermin_loot", "verbs.search", 2)
    );
    // The body is spent: neither the finder nor the next player gets more.
    for id in &ids {
        let r = w.execute(id, "search", at(2));
        assert!(
            r.narrative.contains("Nothing here has died lately"),
            "{r:?}"
        );
    }
    assert_eq!(w.state().total_currency(w.pack()), money + 2);

    // The rat comes back and dies again: its next body pays once more.
    let mut w = again(&w, &ids, |s| kill(s, "giant_rat_1"));
    w.fix_dice(won_then_below(75));
    let r = w.execute(&ids[1], "search", at(3));
    assert!(r.narrative.ends_with("You receive a rat tail."), "{r:?}");
    assert_eq!(flag(&w, "loot.giant_rat_1"), Some(FlagValue::Int(2)));
    assert_eq!(carried(&w, &ids[1], "rat_tail"), 1);
}

#[test]
fn a_failed_search_leaves_the_body_for_the_next_player() {
    let names = [("ada", "wine_vault"), ("bo", "wine_vault")];
    let (mut w, ids) = world_with(&names, |s, _| kill(s, "giant_rat_1"));
    w.fix_dice([6, 5, 5]);
    let r = w.execute(&ids[0], "search", at(2));
    assert!(r.narrative.contains("nothing but filth"), "{r:?}");
    assert_eq!(flag(&w, "loot.giant_rat_1"), None);
    let ada = w.entity(&ids[0]).unwrap();
    assert_eq!(ada.marks["searched.giant_rat_1"], FlagValue::Int(1));
    // ada had her try at it; bo has his, and finds the table's nothing.
    w.fix_dice(won_then_below(0));
    let r = w.execute(&ids[0], "search", at(2));
    assert!(
        r.narrative.contains("Nothing here has died lately"),
        "{r:?}"
    );
    w.fix_dice(won_then_below(0));
    let r = w.execute(&ids[1], "search", at(2));
    assert!(r.narrative.contains("Nothing worth taking"), "{r:?}");
    assert_eq!(flag(&w, "loot.giant_rat_1"), Some(FlagValue::Int(1)));
    assert_eq!(w.fresh_mints(), []);
}

#[test]
fn a_search_in_the_dark_rolls_three_lower() {
    // 9 on 3d6 passes ada's IQ 10 in the light, and fails it at -3.
    let (mut w, ids) = world_with(&[("ada", "wine_vault")], |s, _| kill(s, "giant_rat_1"));
    w.fix_dice(rolled_then_below([3, 3, 3], 30));
    let r = w.execute(&ids[0], "search", at(2));
    assert!(r.narrative.ends_with("You receive 2s."), "{r:?}");
    let (mut w, ids) = world_with(&[("ada", "wine_vault")], |s, _| {
        kill(s, "giant_rat_1");
        douse(s, "pc_ada");
    });
    w.fix_dice(rolled_then_below([3, 3, 3], 30));
    let r = w.execute(&ids[0], "search", at(2));
    assert!(r.narrative.contains("nothing but filth"), "{r:?}");
    assert_eq!(w.fresh_mints(), []);
}

#[test]
fn the_crawler_sometimes_leaves_a_miners_pick() {
    let (mut w, ids) = world_with(&[("ada", "collapsed_drift")], |s, _| {
        kill(s, "cave_crawler");
    });
    // 95-99 of 0-99 falls on the crawler table's pick.
    w.fix_dice(won_then_below(97));
    let r = w.execute(&ids[0], "search", at(2));
    assert!(
        r.narrative.ends_with("You receive a miner's pick."),
        "{r:?}"
    );
    assert_eq!(carried(&w, &ids[0], "miners_pick"), 1);
    assert_eq!(w.fresh_mints()[0].pool.as_str(), "vermin_loot");
    // The pool holds one pick a day: the next body's pick roll finds
    // the pickings thin.
    let mut w = again(&w, &ids, |s| kill(s, "cave_crawler"));
    w.fix_dice(won_then_below(97));
    let r = w.execute(&ids[0], "search", at(3));
    assert!(r.narrative.contains("thin today"), "{r:?}");
    assert_eq!(carried(&w, &ids[0], "miners_pick"), 1);
}

#[test]
fn the_root_cellar_holds_two_bodies_and_the_warden_its_own_hoard() {
    let (mut w, ids) = world_with(&[("ada", "root_cellar")], |s, _| {
        kill(s, "rat_pup_1");
        kill(s, "rat_pup_2");
    });
    w.fix_dice(won_then_below(60));
    assert!(
        w.execute(&ids[0], "search", at(2))
            .narrative
            .ends_with("You receive 1s.")
    );
    w.fix_dice(won_then_below(90));
    let r = w.execute(&ids[0], "search", at(2));
    assert!(r.narrative.ends_with("You receive a rat tail."), "{r:?}");
    assert_eq!(flag(&w, "loot.rat_pup_1"), Some(FlagValue::Int(1)));
    assert_eq!(flag(&w, "loot.rat_pup_2"), Some(FlagValue::Int(1)));

    // The warden pays from its own pool, which the vermin never empty.
    let (mut w, ids) = world_with(&[("ada", "ossuary_crypt")], |s, _| {
        kill(s, "skeletal_warden");
        spent_today(s, "vermin_loot", 40);
    });
    w.fix_dice(won_then_below(60));
    let r = w.execute(&ids[0], "search", at(2));
    assert!(
        r.narrative.ends_with("You receive a healing draught."),
        "{r:?}"
    );
    assert_eq!(w.fresh_mints()[0].pool.as_str(), "warden_hoard");
}

#[test]
fn thin_pickings_spend_the_body_and_pay_nothing() {
    let (mut w, ids) = world_with(&[("ada", "wine_vault")], |s, _| {
        kill(s, "giant_rat_1");
        spent_today(s, "vermin_loot", 39);
    });
    w.fix_dice(won_then_below(30));
    let r = w.execute(&ids[0], "search", at(2));
    assert!(r.ok && r.narrative.contains("thin today"), "{r:?}");
    assert_eq!(flag(&w, "loot.giant_rat_1"), Some(FlagValue::Int(1)));
    assert_eq!(w.fresh_mints(), []);
    assert!(w.state().hook_faults.is_empty());
}

/// Kills the warren rat again and again on one day, and searches each
/// body with the world's own seeded dice. Returns the ledger.
fn farm_the_warren(seed: u64) -> (Vec<MintRecord>, World) {
    let pack = pack();
    let mut w = World::new(Arc::clone(&pack), seed);
    let ada = w.login("ada", at(1)).unwrap();
    let mut state = w.snapshot();
    state.entities.get_mut(&ada).unwrap().room = RoomId::from("rat_warren");
    let ids = [ada.clone()];
    let mut w = open(state, &ids);
    let start = w.state().total_currency(w.pack());
    let mut ledger = Vec::new();
    for _ in 0..40 {
        w = again(&w, &ids, |s| kill(s, "giant_rat_2"));
        let r = w.execute(&ada, "search", at(2));
        assert!(r.ok, "{r:?}");
        ledger.extend(w.drain_mints());
    }
    assert_eq!(
        w.state().total_currency(w.pack()),
        start + w.state().minted_currency()
    );
    (ledger, w)
}

#[test]
fn loot_rolls_once_per_death_from_the_table_within_the_day() {
    let (ledger, w) = farm_the_warren(7);
    assert!(ledger.len() > 5, "most bodies pay something: {ledger:?}");
    let mut money = 0;
    for m in &ledger {
        assert_eq!(m.pool.as_str(), "vermin_loot");
        assert_eq!(m.hook, "verbs.search");
        money += m.currency;
        match (m.currency, m.items.as_slice()) {
            (2 | 4 | 6, []) => {}
            (0, [one]) => assert!(
                ["rat_tail", "bandage"].contains(&one.proto.as_str()),
                "{m:?}"
            ),
            _ => panic!("not on the rat table: {m:?}"),
        }
    }
    assert!(money <= 40, "the pool's day: {money}");
    assert_eq!(
        w.state().check_rewards(w.pack(), &ledger),
        Vec::<String>::new()
    );
    assert!(w.state().hook_faults.is_empty());
    // The same seed rolls the same loot.
    let (twice, _) = farm_the_warren(7);
    let show = |l: &[MintRecord]| {
        l.iter()
            .map(|m| {
                (
                    m.currency,
                    m.items.iter().map(|i| i.proto.clone()).collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(show(&ledger), show(&twice));
}

// ---------------------------------------------------------------- listen

#[test]
fn listen_hears_the_pups_and_the_crawler_until_they_die() {
    let (mut w, ids) = world_with(&[("ada", "cellar_landing")], |s, _| {
        kill(s, "giant_rat_1");
        kill(s, "giant_rat_2");
        kill(s, "skeletal_warden");
    });
    w.fix_dice([1, 1, 1]);
    assert_eq!(
        w.execute(&ids[0], "listen", at(2)).narrative,
        "You hold still and listen: small things squeaking to the south of the stair, and deeper still, a dry clicking of many legs."
    );
    let mut w = again(&w, &ids, |s| {
        kill(s, "rat_pup_1");
        kill(s, "rat_pup_2");
    });
    w.fix_dice([1, 1, 1]);
    assert_eq!(
        w.execute(&ids[0], "listen", at(2)).narrative,
        "You hold still and listen: deeper still, a dry clicking of many legs."
    );
}

// --------------------------------------------------------- liar's bones

fn cup(w: &World, id: &EntityId) -> Option<FlagValue> {
    flag(w, &format!("bones.{id}"))
}

/// Round `n` of day `day`, as flagon.rhai counts rounds.
fn round(day: u64, n: i64) -> i64 {
    i64::try_from(day).unwrap() * 100_000 + n
}

/// The flag of a cup of `dice` (as digits) in round `r`.
const fn cup_in(r: i64, dice: i64) -> FlagValue {
    FlagValue::Int(r * 100_000 + dice)
}

#[test]
fn bones_shows_the_cup_to_its_owner_and_call_lifts_every_cup() {
    let names = [
        ("ada", "rusty_flagon_taproom"),
        ("bo", "rusty_flagon_taproom"),
        ("cy", "town_square"),
    ];
    let (mut w, ids) = world_with(&names, |_, _| {});
    let (ada, bo, cy) = (&ids[0], &ids[1], &ids[2]);
    let r = w.execute(ada, "call", at(2));
    assert_eq!(r.error_code, Some(ErrorCode::Refused), "{r:?}");
    assert!(r.narrative.contains("no cup down"), "{r:?}");
    w.fix_dice([3, 5, 5, 2, 6]);
    let r = w.execute(ada, "bones", at(2));
    assert!(r.ok, "{r:?}");
    assert!(
        r.narrative.ends_with("peek under the rim: 3 5 5 2 6."),
        "{r:?}"
    );
    assert_eq!(
        seen_by(&mut w, bo),
        ["ada shakes the bone cup, slaps it down and peeks under the rim."]
    );
    assert_eq!(cup(&w, ada), Some(cup_in(round(DAY0, 0), 35_526)));
    // cy shakes and walks out before the call: the call leaves cy's cup
    // out of the count, and says so.
    w.fix_dice([4, 4, 4, 4, 4]);
    assert!(w.execute(cy, "move e", at(2)).ok);
    assert!(w.execute(cy, "bones", at(2)).ok);
    assert!(w.execute(cy, "move w", at(2)).ok);
    w.fix_dice([1, 1, 1, 1, 6]);
    assert!(w.execute(bo, "bones", at(2)).ok);
    w.drain_fresh();
    let r = w.execute(bo, "call", at(2));
    let line = "The cups come up: bo 1 1 1 1 6; ada 3 5 5 2 6. On the table: 4 1s, 1 2s, 1 3s, 0 4s, 2 5s, 2 6s. One more cup lies under a hand that has left the table, out of the count.";
    assert_eq!(r.narrative, format!("You call. {line}"));
    assert_eq!(narrations(&mut w), [format!("bo calls. {line}")]);
    assert_eq!(
        flag(&w, "bones_round"),
        Some(FlagValue::Int(round(DAY0, 1)))
    );
    assert_eq!(cup(&w, bo), None);
    // cy comes back with a cup from the round that was called. It is
    // stale: it never counts in the next round.
    assert!(w.execute(cy, "move e", at(2)).ok);
    w.fix_dice([2, 2, 2, 2, 2]);
    assert!(w.execute(ada, "bones", at(2)).ok);
    assert_eq!(cup(&w, ada), Some(cup_in(round(DAY0, 1), 22_222)));
    let r = w.execute(ada, "call", at(2));
    assert!(r.narrative.contains("Nobody else at the table"), "{r:?}");
    assert!(!r.narrative.contains("4 4 4 4 4"), "{r:?}");
    // Alone at the table, a call tips the dice back and ends the round.
    assert_eq!(cup(&w, ada), None);
    assert_eq!(
        flag(&w, "bones_round"),
        Some(FlagValue::Int(round(DAY0, 2)))
    );
    assert!(w.state().hook_faults.is_empty());
}

#[test]
fn a_cup_down_stays_as_it_fell_until_the_call() {
    let names = [
        ("ada", "rusty_flagon_taproom"),
        ("bo", "rusty_flagon_taproom"),
    ];
    let (mut w, ids) = world_with(&names, |_, _| {});
    let (ada, bo) = (&ids[0], &ids[1]);
    w.fix_dice([3, 5, 5, 2, 6]);
    assert!(w.execute(ada, "bones", at(2)).ok);
    // A peek, a bid heard, and a second shake for a better hand: refused.
    w.fix_dice([6, 6, 6, 6, 6]);
    let r = w.execute(ada, "bones", at(2));
    assert_eq!(r.error_code, Some(ErrorCode::Refused), "{r:?}");
    assert!(r.narrative.contains("already down"), "{r:?}");
    assert_eq!(cup(&w, ada), Some(cup_in(round(DAY0, 0), 35_526)));
    let marks = &w.entity(ada).unwrap().marks;
    assert_eq!(
        marks["bones"],
        FlagValue::Int(i64::try_from(DAY0).unwrap() * 100 + 1),
        "a refused shake is not a round"
    );
    // After the call, a new round takes a new cup.
    w.fix_dice([1, 2, 3, 4, 5]);
    assert!(w.execute(bo, "bones", at(2)).ok);
    let r = w.execute(bo, "call", at(2));
    assert!(r.narrative.contains("ada 3 5 5 2 6"), "{r:?}");
    w.fix_dice([6, 6, 6, 6, 6]);
    assert!(w.execute(ada, "bones", at(2)).ok);
    assert_eq!(cup(&w, ada), Some(cup_in(round(DAY0, 1), 66_666)));
}

#[test]
fn bones_allows_eight_rounds_a_day_and_sweeps_old_cups() {
    let (mut w, ids) = world_with(&[("ada", "rusty_flagon_taproom")], |s, _| {
        s.flags.insert(
            "bones.pc_gone".into(),
            FlagValue::Int(round(DAY0 - 1, 3) * 100_000 + 11_111),
        );
        // A round counter from yesterday is round 0 of today.
        s.flags
            .insert("bones_round".into(), FlagValue::Int(round(DAY0 - 1, 3)));
    });
    let ada = &ids[0];
    for n in 0..8 {
        let r = w.execute(ada, "bones", at(2));
        assert!(r.narrative.contains("peek under the rim"), "{r:?}");
        let r = w.execute(ada, "call", at(2));
        assert!(r.narrative.contains("tip your dice back"), "{r:?}");
        assert_eq!(
            flag(&w, "bones_round"),
            Some(FlagValue::Int(round(DAY0, n + 1)))
        );
    }
    assert_eq!(flag(&w, "bones.pc_gone"), None, "a stale cup is swept");
    let r = w.execute(ada, "bones", at(3));
    assert!(r.narrative.contains("enough bones"), "{r:?}");
    let r = w.execute(ada, "bones", at(26));
    assert!(
        r.narrative.contains("peek under the rim"),
        "the next day: {r:?}"
    );
    assert!(w.state().hook_faults.is_empty());
}

#[test]
fn a_flag_table_full_of_stale_cups_still_takes_a_new_shake() {
    let (mut w, ids) = world_with(&[("ada", "rusty_flagon_taproom")], |s, _| {
        let mut n = 0;
        while s.flags.len() < FLAG_CAP {
            s.flags.insert(
                format!("bones.pc_old{n}"),
                FlagValue::Int(round(DAY0 - 1, 0) * 100_000 + 11_111),
            );
            n += 1;
        }
    });
    let ada = &ids[0];
    assert_eq!(w.state().flags.len(), FLAG_CAP);
    w.fix_dice([3, 5, 5, 2, 6]);
    let r = w.execute(ada, "bones", at(2));
    assert!(r.narrative.contains("peek under the rim"), "{r:?}");
    assert_eq!(cup(&w, ada), Some(cup_in(round(DAY0, 0), 35_526)));
    assert_eq!(w.state().flags.len(), FLAG_CAP - 3, "four swept, one set");
    assert!(w.state().hook_faults.is_empty());
}

// ------------------------------------------------------ rumor of the day

const CACHE_ROOMS: [&str; 5] = [
    "flooded_cistern",
    "wine_vault",
    "ossuary_crypt",
    "root_cellar",
    "collapsed_drift",
];
const RIDDLES: [&str; 5] = [
    "where the black water stands to the knee",
    "among the broken casks",
    "where the skulls are stacked",
    "under the rotting turnips",
    "down the dwarf stair",
];
const SPOTS: [&str; 3] = ["corner", "floor", "wall"];

/// Today's cache as rumor.rhai picks it: the room and the spot.
fn cache(day: u64) -> (usize, &'static str) {
    let t = (((day + 7) * 2_654_435_761) % 4_294_967_296) / 65_536;
    (
        usize::try_from(t % 5).unwrap(),
        SPOTS[usize::try_from((t / 5) % 3).unwrap()],
    )
}

#[test]
fn the_cache_moves_with_the_day_and_eavesdrop_hints_at_its_room() {
    assert_ne!(cache(DAY0).0, cache(DAY0 + 1).0, "a new day, a new room");
    let rooms: std::collections::BTreeSet<usize> = (DAY0..DAY0 + 20).map(|d| cache(d).0).collect();
    assert_eq!(rooms.len(), 5, "every room comes up");
    let (mut w, ids) = world_with(&[("ada", "rusty_flagon_taproom")], |_, _| {});
    let ada = &ids[0];
    for day in 0..2 {
        w.fix_dice([1, 1, 1]);
        let r = w.execute(ada, "eavesdrop", at(2 + 24 * day));
        let riddle = RIDDLES[cache(DAY0 + day).0];
        assert!(r.narrative.contains(riddle), "day {day}: {r:?}");
        let r = w.execute(ada, "eavesdrop", at(3 + 24 * day));
        assert!(r.narrative.contains("all the talk"), "once a day: {r:?}");
    }
    // A critical failure hears a room just as surely, and it is wrong.
    w.fix_dice([6, 6, 6]);
    let r = w.execute(ada, "eavesdrop", at(50));
    let right = RIDDLES[cache(DAY0 + 2).0];
    assert!(
        r.narrative.contains("something was hidden below today"),
        "{r:?}"
    );
    assert!(!r.narrative.contains(right), "{r:?}");
    w.fix_dice([5, 5, 5]);
    let r = w.execute(ada, "eavesdrop", at(74));
    assert!(r.narrative.contains("Talk of rain"), "{r:?}");
}

#[test]
fn case_needs_vich_and_tells_the_spot_never_the_room() {
    let (mut w, ids) = world_with(&[("ada", "fence_backroom")], |s, _| {
        s.entities.get_mut(&EntityId::from("vich")).unwrap().room = RoomId::from("dark_alley");
    });
    let r = w.execute(&ids[0], "case", at(2));
    assert_eq!(r.error_code, Some(ErrorCode::Refused), "{r:?}");
    let (mut w, ids) = world_with(&[("ada", "fence_backroom")], |_, _| {});
    let talk = match cache(DAY0).1 {
        "corner" => "tucked into the far corner",
        "floor" => "under a loose stone in the floor",
        _ => "in a gap in the wall",
    };
    w.fix_dice([1, 1, 1]);
    let r = w.execute(&ids[0], "case", at(2));
    assert!(r.narrative.contains(talk), "{r:?}");
    assert!(r.narrative.contains("Nobody says which room"), "{r:?}");
    assert!(!RIDDLES.iter().any(|x| r.narrative.contains(x)), "{r:?}");
    let r = w.execute(&ids[0], "case", at(3));
    assert!(r.narrative.contains("already"), "once a day: {r:?}");
    // Vich himself cases his own backroom, as any player does.
    let vich = EntityId::from("vich");
    w.fix_dice([1, 1, 1]);
    let r = w.execute(&vich, "case", at(3));
    assert!(r.narrative.contains(talk), "{r:?}");
}

#[test]
fn dig_pays_the_first_finder_of_the_day_once() {
    // The day the cache lies in the cistern, which no mob guards.
    let day = (DAY0..DAY0 + 30).find(|d| cache(*d).0 == 0).unwrap();
    let hour = (day - DAY0) * 24 + 2;
    let spot = cache(day).1;
    let wrong = SPOTS.iter().find(|s| **s != spot).unwrap();
    // The room comes from the table here, so a script that lists its
    // rooms in another order fails this test.
    let room = CACHE_ROOMS[cache(day).0];
    let names = [("ada", room), ("bo", room)];
    let (mut w, ids) = world_with(&names, |_, _| {});
    let (ada, bo) = (&ids[0], &ids[1]);
    let money = w.state().total_currency(w.pack());
    let r = w.execute(ada, "dig", at(hour));
    assert_eq!(r.error_code, Some(ErrorCode::Refused), "{r:?}");
    assert!(r.narrative.contains("Dig where?"), "{r:?}");
    let r = w.execute(ada, &format!("dig at the {wrong}"), at(hour));
    assert!(r.narrative.contains("Nothing."), "{r:?}");
    w.drain_fresh();
    let r = w.execute(ada, &format!("dig {spot}"), at(hour));
    assert!(
        r.narrative
            .ends_with("You receive 15s and a healing draught."),
        "{r:?}"
    );
    assert_eq!(seen_by(&mut w, bo), [format!("ada scrapes at the {spot}.")]);
    assert_eq!(
        flag(&w, "cache.found"),
        Some(FlagValue::Int(i64::try_from(day).unwrap()))
    );
    let r = w.execute(bo, &format!("dig {spot}"), at(hour));
    assert!(r.narrative.contains("Someone was here first"), "{r:?}");
    let r = w.execute(ada, &format!("dig {spot}"), at(hour));
    assert!(
        r.narrative.contains("Dig again tomorrow"),
        "two tries: {r:?}"
    );
    assert_eq!(w.state().total_currency(w.pack()), money + 15);
    assert_eq!(w.fresh_mints().len(), 1);
    assert_eq!(w.fresh_mints()[0].pool.as_str(), "rumor_cache");

    // A guarded room: no digging past the rat.
    let (mut w, ids) = world_with(&[("ada", "wine_vault")], |_, _| {});
    let r = w.execute(&ids[0], "dig floor", at(2));
    assert_eq!(r.error_code, Some(ErrorCode::Refused), "{r:?}");
    assert!(r.narrative.contains("wants you dead"), "{r:?}");
}

#[test]
fn eavesdrop_tells_when_the_cache_was_already_dug_up() {
    let (mut w, ids) = world_with(&[("ada", "rusty_flagon_taproom")], |s, _| {
        s.flags.insert(
            "cache.found".into(),
            FlagValue::Int(i64::try_from(DAY0).unwrap()),
        );
    });
    w.fix_dice([1, 1, 1]);
    let r = w.execute(&ids[0], "eavesdrop", at(2));
    assert!(r.narrative.contains(RIDDLES[cache(DAY0).0]), "{r:?}");
    assert!(r.narrative.ends_with("someone already dug it up."), "{r:?}");
    // Yesterday's find says nothing about today's cache.
    w.fix_dice([1, 1, 1]);
    let r = w.execute(&ids[0], "eavesdrop", at(26));
    assert!(!r.narrative.contains("already dug"), "{r:?}");
}

#[test]
fn case_rolls_iq_less_one_without_streetwise_and_streetwise_with_it() {
    let talk = |r: &mud_world::CommandResult| r.narrative.contains("Between the crates");
    // ada has no Streetwise: IQ 10 less 1 is 9, over Streetwise's
    // untrained 5. A 9 passes and a 10 fails.
    for (dice, heard) in [([3, 3, 3], true), ([4, 3, 3], false)] {
        let (mut w, ids) = world_with(&[("ada", "fence_backroom")], |_, _| {});
        w.fix_dice(dice);
        let r = w.execute(&ids[0], "case", at(2));
        assert_eq!(talk(&r), heard, "{dice:?}: {r:?}");
    }
    // Vich has Streetwise 13, over his IQ 12 less 1: a 12 passes.
    let (mut w, _) = world_with(&[("ada", "town_square")], |_, _| {});
    w.fix_dice([4, 4, 4]);
    let r = w.execute(&EntityId::from("vich"), "case", at(2));
    assert!(talk(&r), "{r:?}");
}

// ------------------------------------------------------------ the drift

fn exit_locked(w: &World, who: &EntityId, dir: &str) -> bool {
    w.look_view(who)["exits"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x["dir"] == dir && x["locked"] == true)
}

#[test]
fn clear_opens_the_stair_while_the_warden_is_down_and_it_closes_again() {
    let (mut w, ids) = world_with(&[("ada", "ossuary_crypt")], |_, _| {});
    let ada = &ids[0];
    let r = w.execute(ada, "clear", at(2));
    assert_eq!(r.error_code, Some(ErrorCode::Refused), "{r:?}");
    assert!(r.narrative.contains("Not while it stands"), "{r:?}");

    let (mut w, ids) = world_with(&[("ada", "ossuary_crypt")], |s, _| {
        kill(s, "skeletal_warden");
    });
    let ada = &ids[0];
    assert!(exit_locked(&w, ada, "d"));
    assert!(
        !w.execute(ada, "d", at(2)).ok,
        "the rubble blocks the stair"
    );
    w.fix_dice([5, 5, 5]);
    let r = w.execute(ada, "clear", at(2));
    assert!(r.narrative.contains("settles back"), "{r:?}");
    let hp = w.entity(ada).unwrap().pools.hp;
    w.fix_dice([6, 6, 6, 6]);
    let r = w.execute(ada, "clear", at(2));
    assert!(r.narrative.contains("cracks across your shin"), "{r:?}");
    assert_eq!(w.entity(ada).unwrap().pools.hp, hp - 4);
    w.fix_dice([1, 1, 1]);
    let r = w.execute(ada, "clear", at(2));
    assert!(r.narrative.contains("lies open"), "{r:?}");
    assert!(!exit_locked(&w, ada, "d"));
    let r = w.execute(ada, "clear", at(2));
    assert!(r.narrative.contains("The dwarf stair is clear"), "{r:?}");
    let r = w.execute(ada, "d", at(2));
    assert!(r.ok, "{r:?}");
    // An hour on, the rubble has slid back. The way up stays open.
    w.tick(at(2) + 61 * 60_000);
    assert!(w.execute(ada, "u", at(4)).ok);
    assert!(exit_locked(&w, ada, "d"));
}

#[test]
fn no_pick_opens_the_rubble_whether_the_warden_stands_or_not() {
    for warden_down in [false, true] {
        let (mut w, ids) = world_with(&[("ada", "ossuary_crypt")], |s, _| {
            if warden_down {
                kill(s, "skeletal_warden");
            }
        });
        let ada = &ids[0];
        let hp = w.entity(ada).unwrap().pools.hp;
        for word in ["rubble", "stair", "d", "grate", "lock"] {
            // A roll of 3 would open any pickable lock.
            w.fix_dice([1, 1, 1]);
            let r = w.execute(ada, &format!("attempt lockpicking on {word}"), at(2));
            assert_eq!(r.error_code, Some(ErrorCode::Refused), "{word}: {r:?}");
            assert!(r.narrative.contains("no pick can open the rubble"), "{r:?}");
            assert!(exit_locked(&w, ada, "d"), "{word}");
        }
        // The warden draws no round on a refused try.
        assert_eq!(w.entity(ada).unwrap().pools.hp, hp);
    }
}

#[test]
fn mine_in_the_dark_rolls_three_lower_and_a_bad_slip_hurts() {
    let setup = |dark: bool| {
        world_with(&[("ada", "collapsed_drift")], move |s, p| {
            kill(s, "cave_crawler");
            give(s, p, "pc_ada", "miners_pick");
            if dark {
                douse(s, "pc_ada");
            }
        })
    };
    // 9 on 3d6 passes ada's HT 10 in the light, and fails it at -3.
    for (dark, chunks) in [(false, 1), (true, 0)] {
        let (mut w, ids) = setup(dark);
        w.fix_dice([3, 3, 3]);
        w.execute(&ids[0], "mine", at(2));
        assert_eq!(carried(&w, &ids[0], "iron_ore"), chunks, "dark {dark}");
    }
    let (mut w, ids) = setup(false);
    let hp = w.entity(&ids[0]).unwrap().pools.hp;
    w.fix_dice([6, 6, 6, 6]);
    let r = w.execute(&ids[0], "mine", at(2));
    assert!(r.narrative.contains("A slab peels off the roof"), "{r:?}");
    assert_eq!(w.entity(&ids[0]).unwrap().pools.hp, hp - 4);
    assert_eq!(carried(&w, &ids[0], "iron_ore"), 0);
}

#[test]
fn mine_needs_a_pick_and_the_crawler_down_once_a_day() {
    let (mut w, ids) = world_with(&[("ada", "collapsed_drift")], |s, p| {
        give(s, p, "pc_ada", "miners_pick");
    });
    let r = w.execute(&ids[0], "mine", at(2));
    assert_eq!(r.error_code, Some(ErrorCode::Refused), "{r:?}");
    assert!(r.narrative.contains("mandibles"), "{r:?}");

    let (mut w, ids) = world_with(&[("ada", "collapsed_drift")], |s, _| {
        kill(s, "cave_crawler");
    });
    let r = w.execute(&ids[0], "mine", at(2));
    assert!(r.narrative.contains("You need a miner's pick"), "{r:?}");

    let (mut w, ids) = world_with(&[("ada", "collapsed_drift")], |s, p| {
        kill(s, "cave_crawler");
        give(s, p, "pc_ada", "miners_pick");
    });
    let ada = &ids[0];
    w.fix_dice([1, 1, 1]);
    let r = w.execute(ada, "mine", at(2));
    assert!(
        r.narrative
            .ends_with("You receive a chunk of dwarven iron ore."),
        "{r:?}"
    );
    assert_eq!(carried(&w, ada, "iron_ore"), 1);
    let r = w.execute(ada, "mine", at(3));
    assert!(r.narrative.contains("arms are spent"), "{r:?}");
    // The next day, a failure takes the day's swing and gives nothing.
    w.fix_dice([5, 5, 5]);
    let r = w.execute(ada, "mine", at(26));
    assert!(r.narrative.contains("Nothing worth carrying"), "{r:?}");
    assert_eq!(carried(&w, ada, "iron_ore"), 1);
}

#[test]
fn the_seam_gives_four_chunks_a_day_and_torin_buys_them() {
    let names = [
        ("ada", "collapsed_drift"),
        ("bo", "collapsed_drift"),
        ("cy", "collapsed_drift"),
        ("di", "collapsed_drift"),
        ("ed", "collapsed_drift"),
    ];
    let (mut w, ids) = world_with(&names, |s, p| {
        kill(s, "cave_crawler");
        kill(s, "skeletal_warden");
        for (n, _) in &names {
            give(s, p, &format!("pc_{n}"), "miners_pick");
        }
    });
    let money = w.state().total_currency(w.pack());
    let mut ledger = Vec::new();
    for id in &ids[..4] {
        w.fix_dice([1, 1, 1]);
        assert!(w.execute(id, "mine", at(2)).ok);
    }
    let r = w.execute(&ids[4], "mine", at(2));
    assert!(r.narrative.contains("Every loose chunk is gone"), "{r:?}");
    assert_eq!(carried(&w, &ids[4], "iron_ore"), 0);
    ledger.extend(w.drain_mints());
    assert_eq!(ledger.len(), 4);
    // The seam comes back the next day.
    w.fix_dice([1, 1, 1]);
    let r = w.execute(&ids[4], "mine", at(26));
    assert!(r.narrative.contains("dwarven iron ore"), "{r:?}");
    ledger.extend(w.drain_mints());
    assert_eq!(
        w.state().check_rewards(w.pack(), &ledger),
        Vec::<String>::new()
    );
    assert_eq!(
        w.state().total_currency(w.pack()),
        money,
        "ore is not money"
    );

    // Torin pays his 15s for a chunk, from his own purse.
    let ada = &ids[0];
    for dir in ["u", "ne", "w", "u", "n", "w"] {
        let r = w.execute(ada, dir, at(27));
        assert!(r.ok, "{dir}: {r:?}");
    }
    let torin = EntityId::from("torin");
    let (before, his) = (
        w.entity(ada).unwrap().currency,
        w.entity(&torin).unwrap().currency,
    );
    let r = w.execute(ada, "sell ore", at(27));
    assert!(r.ok, "{r:?}");
    assert_eq!(w.entity(ada).unwrap().currency, before + 15);
    assert_eq!(
        w.entity(&torin).unwrap().currency,
        his,
        "the town's ore bounty pays, not his purse"
    );
    let mint = w.fresh_mints()[0].clone();
    assert_eq!(
        (mint.pool.as_str(), mint.hook.as_str(), mint.currency),
        ("ore_bounty", "shops.torin", 15)
    );
}

#[test]
fn torins_purse_outlasts_a_month_of_the_seams_ore() {
    let (mut w, ids) = world_with(&[("ada", "smithy")], |s, p| {
        for _ in 0..(4 * 30 + 4) {
            give(s, p, "pc_ada", "iron_ore");
        }
    });
    let (ada, torin) = (&ids[0], EntityId::from("torin"));
    let purse = w.entity(&torin).unwrap().currency;
    let money = w.state().total_currency(w.pack());
    let mut ledger = Vec::new();
    // The ossuary's four one-off chunks on the first day come out of his
    // purse once the day's bounty is spent.
    for n in 0..8 {
        let r = w.execute(ada, "sell ore", at(2));
        assert!(r.ok, "chunk {n}: {r:?}");
    }
    assert_eq!(w.entity(&torin).unwrap().currency, purse - 60);
    ledger.extend(w.drain_mints());
    // Then the seam's four a day, for a month: his purse never moves.
    for day in 1..30 {
        for n in 0..4 {
            let r = w.execute(ada, "sell ore", at(2 + 24 * day));
            assert!(r.ok, "day {day}, chunk {n}: {r:?}");
        }
        assert_eq!(w.entity(&torin).unwrap().currency, purse - 60, "day {day}");
        ledger.extend(w.drain_mints());
    }
    assert_eq!(ledger.len(), 4 * 30);
    assert!(ledger.iter().all(|m| m.pool.as_str() == "ore_bounty"));
    // He smelts what the bounty buys. Only the four his purse paid for
    // wait on his rack, so the rack does not grow with the days.
    let rack = w.state().shop_fixtures[&mud_world::ShopId::from("torin")];
    let ore = w.state().items[&rack]
        .contents
        .iter()
        .filter(|i| w.state().items[*i].proto.as_str() == "iron_ore")
        .count();
    assert_eq!(ore, 4);
    assert_eq!(
        w.state().check_rewards(w.pack(), &ledger),
        Vec::<String>::new()
    );
    assert_eq!(
        w.state().total_currency(w.pack()),
        money + 15 * 4 * 30,
        "every coin of the bounty is minted"
    );
}

#[test]
fn torin_sells_picks_and_hatchets() {
    let (mut w, ids) = world_with(&[("ada", "smithy")], |s, _| {
        s.entities
            .get_mut(&EntityId::from("pc_ada"))
            .unwrap()
            .currency = 60;
    });
    let ada = &ids[0];
    let r = w.execute(ada, "list", at(2));
    assert!(r.narrative.contains("miner's pick"), "{r:?}");
    assert!(r.narrative.contains("hatchet"), "{r:?}");
    assert!(w.execute(ada, "buy pick", at(2)).ok);
    assert!(w.execute(ada, "buy hatchet", at(2)).ok);
    assert_eq!(w.entity(ada).unwrap().currency, 20);
    assert!(w.execute(ada, "wield hatchet", at(2)).ok);
}

// ---------------------------------------------------------- a saved world

#[test]
fn a_saved_world_gains_the_new_rooms_and_mobs_but_not_the_new_stock() {
    use mud_world::ShopId;
    use mud_world::pack::Direction;
    // The pack as it stood before this content: no root cellar, no drift,
    // no pups or crawler, and the rack's six pieces.
    let mut old = (*pack()).clone();
    for r in ["root_cellar", "collapsed_drift"] {
        old.rooms.remove(&RoomId::from(r));
    }
    let landing = old.rooms.get_mut(&RoomId::from("cellar_landing")).unwrap();
    landing.exits.remove(&Direction::S);
    let ossuary = old.rooms.get_mut(&RoomId::from("ossuary_crypt")).unwrap();
    ossuary.exits.remove(&Direction::D);
    ossuary.locks.clear();
    for c in ["rat_pup_1", "rat_pup_2", "cave_crawler"] {
        old.cast.remove(&EntityId::from(c));
    }
    old.economy
        .shops
        .get_mut(&ShopId::from("torin"))
        .unwrap()
        .stock
        .truncate(6);
    let saved = World::new(Arc::new(old), 42).snapshot();
    let mut w = open(saved, &[]);
    for (mob, room) in [
        ("rat_pup_1", "root_cellar"),
        ("rat_pup_2", "root_cellar"),
        ("cave_crawler", "collapsed_drift"),
    ] {
        assert_eq!(w.entity(&EntityId::from(mob)).unwrap().room.as_str(), room);
    }
    let ada = w.login("ada", at(2)).unwrap();
    let mut state = w.snapshot();
    state.entities.get_mut(&ada).unwrap().room = RoomId::from("ossuary_crypt");
    kill(&mut state, "skeletal_warden");
    let w = open(state, std::slice::from_ref(&ada));
    assert!(exit_locked(&w, &ada, "d"), "the rubble starts in place");
    let rack = w.state().shop_fixtures[&ShopId::from("torin")];
    let protos: Vec<&str> = w.state().items[&rack]
        .contents
        .iter()
        .map(|i| w.state().items[i].proto.as_str())
        .collect();
    assert_eq!(
        protos.len(),
        6,
        "a saved rack keeps its own stock: {protos:?}"
    );
    assert!(!protos.contains(&"miners_pick"));
}
