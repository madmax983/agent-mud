//! Random play with the shipped scripts keeps every invariant, and the
//! same play gives the same world.
#![expect(clippy::unwrap_used, reason = "test helpers fail loudly")]

use std::sync::Arc;

use mud_script::ScriptHost;
use mud_world::hooks::WorldHooks;
use mud_world::state::Holder;
use mud_world::{EntityId, Event, HooksChange, ProtoId, RoomId, Seq, World, WorldPack};
use proptest::prelude::*;

const LINES: [&str; 30] = [
    "n",
    "s",
    "e",
    "w",
    "u",
    "d",
    "ne",
    "nw",
    "look",
    "pray",
    "listen",
    "say vich, you there?",
    "say the crypt and the grate",
    "take ore",
    "drop ore",
    "use charm",
    "use charm on warden",
    "use charm on hale",
    "equip torch",
    "attack rat",
    "attack warden",
    "flee",
    "defend",
    "use bandage",
    "take ring from lockbox",
    "attempt stealth",
    "sell ore",
    "give 2s to vich",
    "help",
    "pray for rain",
];

/// Two players in the crypt with a light and a charm, a third in town.
fn world(seed: u64) -> (World, [EntityId; 3]) {
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let mut w = World::new(Arc::clone(&pack), seed);
    let ids = [
        w.login("ada", 1).unwrap(),
        w.login("bo", 1).unwrap(),
        w.login("cy", 1).unwrap(),
    ];
    let mut s = w.snapshot();
    for (id, room) in ids
        .iter()
        .zip(["ossuary_crypt", "cellar_landing", "dark_alley"])
    {
        s.entities.get_mut(id).unwrap().room = RoomId::from(room);
        let charm = s.new_item(&ProtoId::from("bone_charm"), &pack);
        s.attach(charm, &Holder::Entity { entity: id.clone() });
    }
    let mut w = World::from_state(Arc::clone(&pack), s, Vec::new());
    let host = Arc::new(ScriptHost::bundled_crypt_v01(&pack).unwrap());
    w.set_hooks(Some(host as Arc<dyn WorldHooks>), 1, HooksChange::Start);
    for id in &ids {
        w.bring_online(id, 1);
        let r = w.execute(id, "equip torch", 2);
        assert!(r.ok, "{r:?}");
    }
    (w, ids)
}

fn play(seed: u64, steps: &[(usize, usize)]) -> (World, Vec<Event>) {
    let (mut w, ids) = world(seed);
    let mut log = w.drain_fresh();
    let mut now = 1_700_000_000_000;
    for (line, who) in steps {
        now += 7_000;
        w.execute(&ids[*who], LINES[*line], now);
        w.tick(now);
        log.extend(w.drain_fresh());
    }
    (w, log)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]
    #[test]
    fn random_play_with_scripts_keeps_every_invariant(
        seed in 0u64..10_000,
        steps in prop::collection::vec((0usize..LINES.len(), 0usize..3), 1..100),
    ) {
        let (mut w, ids) = world(seed);
        let money = w.state().total_currency(w.pack());
        let next_item = w.state().next_item;
        let mut items = w.state().items.len();
        let mut now = 1_700_000_000_000;
        let mut last = w.last_seq();
        for (line, who) in steps {
            now += 7_000;
            let r = w.execute(&ids[who], LINES[line], now);
            for s in &r.events {
                prop_assert!(*s > last, "event sequence must increase");
                last = *s;
            }
            w.tick(now);
            last = last.max(w.last_seq());
            let bad = w.state().check();
            prop_assert!(bad.is_empty(), "{:?} after {}", bad, LINES[line]);
            prop_assert_eq!(w.state().total_currency(w.pack()), money, "scripts mint no money");
            prop_assert_eq!(w.state().next_item, next_item, "scripts make no items");
            let now_items = w.state().items.len();
            prop_assert!(now_items <= items, "items are only ever used up");
            items = now_items;
            prop_assert!(w.state().flags.len() <= 256);
        }
    }
}

#[test]
fn the_same_play_twice_gives_the_same_snapshot_and_log() {
    let steps: Vec<(usize, usize)> = (0..200)
        .map(|i| ((i * 7 + 3) % LINES.len(), i % 3))
        .collect();
    let (a, la) = play(99, &steps);
    let (b, lb) = play(99, &steps);
    assert_eq!(a.snapshot(), b.snapshot());
    assert_eq!(la, lb);
    assert!(
        la.iter().any(|e| e.tags.iter().any(|t| t == "script")),
        "scripts ran"
    );
    assert!(a.last_seq() > Seq(10));
}
