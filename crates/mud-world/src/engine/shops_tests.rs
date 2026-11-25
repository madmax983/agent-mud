//! Keepers keep shop: stock in a shop fixture, sales while the keeper
//! minds it, honesty boxes, soft posts, and saves from before fixtures.

use std::sync::Arc;

use super::*;
use crate::ids::ShopId;
use crate::pack::PackSources;

const MINUTE: u64 = 60_000;

fn id(s: &str) -> EntityId {
    EntityId::from(s)
}

fn crypt() -> World {
    World::new(Arc::new(WorldPack::crypt_v01().unwrap()), 42)
}

fn with_pack(edit: impl FnOnce(&mut WorldPack)) -> World {
    let mut pack = WorldPack::crypt_v01().unwrap();
    edit(&mut pack);
    World::new(Arc::new(pack), 42)
}

/// crypt-v01 with no shop fixtures: the stock rides on the keepers.
fn body_stock_pack() -> WorldPack {
    let mut pack = WorldPack::crypt_v01().unwrap();
    for shop in pack.economy.shops.values_mut() {
        shop.fixture = None;
        shop.room = None;
        shop.unattended = false;
    }
    pack
}

fn run(w: &mut World, who: &EntityId, line: &str) -> CommandResult {
    w.execute(who, line, 1_000)
}

fn ok(w: &mut World, who: &EntityId, line: &str) -> CommandResult {
    let r = run(w, who, line);
    assert!(r.ok, "{line:?} failed: {r:?}");
    r
}

fn refused(w: &mut World, who: &EntityId, line: &str) -> CommandResult {
    let r = run(w, who, line);
    assert!(!r.ok, "{line:?} should fail: {r:?}");
    r
}

fn place(w: &mut World, who: &EntityId, room: &str) {
    w.state.entities.get_mut(who).unwrap().room = RoomId::from(room);
}

fn shelf(w: &World) -> ItemId {
    w.state.shop_fixtures[&ShopId::from("marta")]
}

/// How many of `proto` the Flagon's shelf holds as stock.
fn on_shelf(w: &World, proto: &str) -> usize {
    w.state.items[&shelf(w)]
        .contents
        .iter()
        .filter(|i| w.state.items[i].stock && w.state.items[i].proto.as_str() == proto)
        .count()
}

/// Whether `who` carries any shop stock on its body.
fn carries_stock(w: &World, who: &EntityId) -> bool {
    w.entity(who)
        .unwrap()
        .inventory
        .iter()
        .any(|i| w.state.items[i].stock)
}

/// A player standing in the Flagon.
fn ada_at_the_flagon(w: &mut World) -> EntityId {
    let ada = w.login("ada", 1).unwrap();
    ok(w, &ada, "e");
    ada
}

#[test]
fn the_stock_starts_in_the_shop_fixture_not_on_the_keeper() {
    let w = crypt();
    for (shop, room) in [
        ("marta", "rusty_flagon_taproom"),
        ("torin", "smithy"),
        ("vich", "fence_backroom"),
    ] {
        let f = w.state.shop_fixtures[&ShopId::from(shop)];
        assert_eq!(
            w.state.holder_of(f),
            Some(Holder::Room {
                room: RoomId::from(room)
            })
        );
        let n = w.pack.economy.shops[&ShopId::from(shop)].stock.len();
        assert_eq!(w.state.items[&f].contents.len(), n, "{shop}");
        assert!(!carries_stock(&w, &id(shop)), "{shop} carries none");
    }
    assert_eq!(w.state.check(), Vec::<String>::new());
}

#[test]
fn buying_at_the_flagon_works_with_marta_behind_the_bar() {
    let mut w = crypt();
    let ada = ada_at_the_flagon(&mut w);
    let list = ok(&mut w, &ada, "list");
    assert!(list.narrative.starts_with("Marta sells:"), "{list:?}");
    assert!(list.narrative.contains("mug of ale - 1s (x5)"));
    let marta_had = w.entity(&id("marta")).unwrap().currency;
    let r = ok(&mut w, &ada, "buy ale");
    assert_eq!(r.resolution["price"], 1);
    assert!(r.narrative.contains("You buy the mug of ale"));
    assert_eq!(on_shelf(&w, "ale"), 4);
    assert_eq!(w.entity(&id("marta")).unwrap().currency, marta_had + 1);
    let bought = r.resolution["item"].as_u64().unwrap();
    assert!(w.entity(&ada).unwrap().inventory.contains(&ItemId(bought)));
    assert!(!w.state.items[&ItemId(bought)].stock, "the buyer's own");
    // Naming the keeper or the fixture finds the same shop.
    ok(&mut w, &ada, "buy ale from marta");
    ok(&mut w, &ada, "buy ale from shelf");
    assert_eq!(on_shelf(&w, "ale"), 2);
    assert_eq!(w.state.check(), Vec::<String>::new());
}

#[test]
fn with_a_fixture_marta_in_the_cellar_leaves_the_stock_in_the_flagon() {
    let mut w = crypt();
    let ada = ada_at_the_flagon(&mut w);
    place(&mut w, &id("marta"), "wine_vault");
    assert_eq!(on_shelf(&w, "ale"), 5, "the ale stays home");
    assert!(!carries_stock(&w, &id("marta")));
    let r = refused(&mut w, &ada, "buy ale");
    assert_eq!(r.error_code, Some(ErrorCode::NoKeeper));
    assert!(
        r.narrative
            .contains("nobody is minding the shelf behind the bar"),
        "{r:?}"
    );
    assert_eq!(
        refused(&mut w, &ada, "list").error_code,
        Some(ErrorCode::NoKeeper)
    );
    assert_eq!(on_shelf(&w, "ale"), 5);
}

#[test]
fn an_unconscious_keeper_means_no_sale() {
    let mut w = crypt();
    let ada = ada_at_the_flagon(&mut w);
    w.state.entities.get_mut(&id("marta")).unwrap().unconscious = true;
    assert_eq!(
        refused(&mut w, &ada, "buy ale").error_code,
        Some(ErrorCode::NoKeeper)
    );
    let pack = Arc::clone(&w.pack);
    let ring = w.state.new_item(&ProtoId::from("silver_ring"), &pack);
    w.state.attach(
        ring,
        &Holder::Entity {
            entity: ada.clone(),
        },
    );
    assert_eq!(
        refused(&mut w, &ada, "sell ring").error_code,
        Some(ErrorCode::NoKeeper)
    );
}

#[test]
fn an_unattended_shop_sells_at_the_posted_price_and_buys_nothing() {
    let mut w = with_pack(|p| {
        p.economy
            .shops
            .get_mut(&ShopId::from("marta"))
            .unwrap()
            .unattended = true;
    });
    let ada = ada_at_the_flagon(&mut w);
    let marta = id("marta");
    place(&mut w, &marta, "town_square");
    let had = w.entity(&marta).unwrap().currency;
    let list = ok(&mut w, &ada, "list");
    assert!(list.narrative.starts_with("shelf behind the bar sells:"));
    assert!(list.narrative.contains("Nobody minds it"));
    assert_eq!(list.resolution["minded"], false);
    let r = ok(&mut w, &ada, "buy draught");
    assert_eq!(r.resolution["price"], 20, "the posted price");
    assert_eq!(w.entity(&marta).unwrap().currency, had + 20);
    let line = w.logged().find(|e| e.kind == EventKind::Trade).unwrap();
    assert!(
        line.narrative
            .contains("takes the healing draught from the shelf behind the bar and leaves 20s"),
        "{}",
        line.narrative
    );
    assert!(line.target.is_none(), "no keeper to name");
    // Buying from customers still needs the keeper.
    let pack = Arc::clone(&w.pack);
    let ring = w.state.new_item(&ProtoId::from("silver_ring"), &pack);
    w.state.attach(
        ring,
        &Holder::Entity {
            entity: ada.clone(),
        },
    );
    assert_eq!(
        refused(&mut w, &ada, "sell ring").error_code,
        Some(ErrorCode::NoKeeper)
    );
    assert_eq!(w.state.check(), Vec::<String>::new());
}

#[test]
fn nobody_takes_stock_out_of_a_fixture() {
    let mut w = crypt();
    let ada = ada_at_the_flagon(&mut w);
    let r = refused(&mut w, &ada, "take ale from shelf");
    assert_eq!(r.error_code, Some(ErrorCode::PolicyTradeRequired));
    assert_eq!(
        refused(&mut w, &ada, "take shelf").error_code,
        Some(ErrorCode::CannotTake)
    );
    assert_eq!(
        refused(&mut w, &id("marta"), "take ale from shelf").error_code,
        Some(ErrorCode::PolicyTradeRequired),
        "not even the keeper"
    );
    let r = ok(&mut w, &ada, "use shelf");
    assert!(r.narrative.contains("holds wares for sale"), "{r:?}");
    assert!(r.narrative.contains("(list, buy <item>)"));
    assert_eq!(on_shelf(&w, "ale"), 5);
}

#[test]
fn a_sold_out_fixture_says_so() {
    let mut w = crypt();
    let ada = ada_at_the_flagon(&mut w);
    w.state.entities.get_mut(&ada).unwrap().currency = 500;
    for _ in 0..5 {
        ok(&mut w, &ada, "buy ale");
    }
    ok(&mut w, &ada, "buy torch");
    ok(&mut w, &ada, "buy torch");
    ok(&mut w, &ada, "buy draught");
    let r = ok(&mut w, &ada, "use shelf");
    assert!(r.narrative.contains("sold out"), "{r:?}");
    assert!(
        ok(&mut w, &ada, "list")
            .narrative
            .contains("nothing right now")
    );
}

#[test]
fn goods_sold_to_a_fixture_shop_go_onto_its_fixture() {
    let mut w = crypt();
    let ada = w.login("ada", 1).unwrap();
    ok(&mut w, &ada, "w");
    let pack = Arc::clone(&w.pack);
    let ore = w.state.new_item(&ProtoId::from("iron_ore"), &pack);
    w.state.attach(
        ore,
        &Holder::Entity {
            entity: ada.clone(),
        },
    );
    let r = ok(&mut w, &ada, "sell ore");
    assert_eq!(r.resolution["price"], 15);
    let rack = w.state.shop_fixtures[&ShopId::from("torin")];
    assert_eq!(
        w.state.holder_of(ore),
        Some(Holder::Container { item: rack })
    );
    assert!(w.state.items[&ore].stock);
    assert_eq!(w.state.items[&ore].paid, Some(15));
    assert!(!carries_stock(&w, &id("torin")));
    // He never sells it back for less than he paid.
    let r = ok(&mut w, &ada, "buy ore");
    assert_eq!(r.resolution["price"], 15);
    assert_eq!(w.state.check(), Vec::<String>::new());
}

#[test]
fn a_keepers_sheet_shows_its_counter_and_post() {
    let mut w = crypt();
    let sheet = w.sheet_view(&id("marta"));
    let shop = &sheet["shop"];
    assert_eq!(shop["counter"]["fixture"], "shelf behind the bar");
    assert_eq!(shop["counter"]["room"], "rusty_flagon_taproom");
    assert_eq!(shop["counter"]["unattended"], false);
    let ale = shop["sells"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "mug of ale")
        .unwrap();
    assert_eq!(ale["count"], 5);
    assert_eq!(sheet["post"]["at_post"], true);
    assert!(sheet["post"].get("way").is_none(), "home: no way to give");
    place(&mut w, &id("marta"), "wine_vault");
    let post = w.sheet_view(&id("marta"))["post"].clone();
    assert_eq!(post["at_post"], false);
    assert_eq!(post["return_to"], "rusty_flagon_taproom");
    assert_eq!(post["return_title"], "The Rusty Flagon");
    assert_eq!(post["way"], serde_json::json!(["e", "u", "n", "e"]));
    // A player has no post and no shop.
    let ada = w.login("ada", 1).unwrap();
    let mine = w.sheet_view(&ada);
    assert!(mine.get("post").is_none() && mine.get("shop").is_none());
    assert_eq!(w.post_view(&ada), Value::Null);
}

#[test]
fn the_way_to_a_post_is_the_shortest_and_stable() {
    let w = crypt();
    let at = |r: &str| RoomId::from(r);
    assert_eq!(
        w.way_to(&at("ossuary_crypt"), &[at("smithy")]),
        Some((
            at("smithy"),
            vec![
                Direction::Ne,
                Direction::W,
                Direction::U,
                Direction::N,
                Direction::W
            ]
        ))
    );
    assert_eq!(
        w.way_to(&at("smithy"), &[at("smithy")]),
        Some((at("smithy"), Vec::new()))
    );
    assert_eq!(w.way_to(&at("smithy"), &[at("nowhere")]), None);
}

/// The regression of the owner's log: Marta goes down, collapses in the
/// wine vault, and the Flagon's stock waits for her behind the bar. Once
/// she is back on her feet she walks home, the way she came in for free,
/// and sells again.
#[test]
fn marta_collapses_in_wine_vault_and_the_flagon_stock_waits_for_her() {
    let mut w = crypt();
    let ada = ada_at_the_flagon(&mut w);
    let marta = id("marta");
    place(&mut w, &marta, "cellar_landing");
    // The rat on the vault floor misses her as she comes in.
    w.fix_dice([6, 6, 6]);
    let r = ok(&mut w, &marta, "w");
    assert_eq!(r.resolution["ambush"].as_array().map(Vec::len), Some(1));
    {
        let e = w.state.entities.get_mut(&marta).unwrap();
        e.pools.hp = -3;
        e.unconscious = true;
    }
    assert_eq!(on_shelf(&w, "ale"), 5, "today the stock fell with her");
    assert_eq!(on_shelf(&w, "healing_draught"), 1);
    assert!(!carries_stock(&w, &marta));
    assert_eq!(
        refused(&mut w, &ada, "buy ale").error_code,
        Some(ErrorCode::NoKeeper)
    );
    // She makes her HT roll at the next recovery step.
    w.fix_dice([3, 3, 3]);
    let out = w.tick(MINUTE);
    assert_eq!(out.came_to.len(), 1);
    // Back out the way she came is a walk, with the rat still there.
    let r = ok(&mut w, &marta, "e");
    assert_eq!(r.resolution["ambush"], serde_json::json!([]));
    for step in ["u", "n", "e"] {
        ok(&mut w, &marta, step);
    }
    assert_eq!(
        w.entity(&marta).unwrap().room,
        RoomId::from("rusty_flagon_taproom")
    );
    let r = ok(&mut w, &ada, "buy draught");
    assert_eq!(r.resolution["price"], 20);
    assert_eq!(on_shelf(&w, "healing_draught"), 0);
    assert_eq!(w.state.check(), Vec::<String>::new());
}

#[test]
fn a_shop_without_a_fixture_still_trades_from_the_keepers_body() {
    let mut w = World::new(Arc::new(body_stock_pack()), 42);
    assert!(w.state.shop_fixtures.is_empty());
    let ada = ada_at_the_flagon(&mut w);
    assert!(carries_stock(&w, &id("marta")));
    ok(&mut w, &ada, "buy ale");
    // The shop goes where the keeper goes.
    let marta = id("marta");
    place(&mut w, &marta, "town_square");
    assert_eq!(
        refused(&mut w, &ada, "buy ale").error_code,
        Some(ErrorCode::NoKeeper)
    );
    ok(&mut w, &ada, "w");
    ok(&mut w, &ada, "buy ale from marta");
    assert!(w.sheet_view(&marta)["shop"]["counter"].is_null());
    assert_eq!(w.state.check(), Vec::<String>::new());
}

#[test]
fn an_older_save_moves_the_keepers_stock_into_the_fixtures() {
    let old = World::new(Arc::new(body_stock_pack()), 42);
    let before = old.state.items.len();
    let money = old.state.total_currency(&old.pack);
    let mut json = serde_json::to_value(old.snapshot()).unwrap();
    let obj = json.as_object_mut().unwrap();
    obj.remove("shop_fixtures");
    obj.remove("cast_gear");
    let state: WorldState = serde_json::from_value(json).unwrap();
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let mut w = World::from_state(Arc::clone(&pack), state, Vec::new());
    assert_eq!(
        w.state.items.len(),
        before + 3,
        "three fixtures, no new stock"
    );
    assert_eq!(w.state.total_currency(&pack), money);
    for keeper in ["marta", "torin", "vich"] {
        assert!(!carries_stock(&w, &id(keeper)), "{keeper}");
    }
    assert_eq!(on_shelf(&w, "ale"), 5);
    assert_eq!(w.state.check(), Vec::<String>::new());
    let ada = ada_at_the_flagon(&mut w);
    ok(&mut w, &ada, "buy ale");
    // The fixtures round-trip through a snapshot as they are.
    let again: WorldState =
        serde_json::from_value(serde_json::to_value(w.snapshot()).unwrap()).unwrap();
    assert_eq!(again, w.snapshot());
}

#[test]
fn a_pack_that_drops_a_fixture_puts_the_stock_back_on_the_keeper() {
    let w = crypt();
    let before = w.state.items.len();
    let mut w = World::from_state(Arc::new(body_stock_pack()), w.snapshot(), Vec::new());
    assert!(w.state.shop_fixtures.is_empty());
    assert_eq!(
        w.state.items.len(),
        before - 3,
        "the fixtures go, the stock stays"
    );
    assert!(carries_stock(&w, &id("marta")));
    assert_eq!(w.state.check(), Vec::<String>::new());
    let ada = ada_at_the_flagon(&mut w);
    ok(&mut w, &ada, "buy ale");
}

#[test]
fn a_shop_the_pack_drops_leaves_its_stock_as_plain_goods() {
    let w = crypt();
    let mut pack = WorldPack::crypt_v01().unwrap();
    pack.economy.shops.remove(&ShopId::from("vich"));
    pack.cast.get_mut(&id("vich")).unwrap().shop = None;
    let before = w.state.items.len();
    let w = World::from_state(Arc::new(pack), w.snapshot(), Vec::new());
    assert_eq!(w.state.items.len(), before - 1);
    let floor = &w.state.rooms[&RoomId::from("fence_backroom")].items;
    assert_eq!(floor.len(), 4, "the key, two bandages and a torch");
    assert!(floor.iter().all(|i| !w.state.items[i].stock));
    assert_eq!(w.state.check(), Vec::<String>::new());
}

#[test]
fn a_keeper_new_to_the_save_gets_its_stock_in_its_fixture() {
    let w = crypt();
    let mut state = w.snapshot();
    state.entities.remove(&id("marta"));
    let shelf_id = state.shop_fixtures.remove(&ShopId::from("marta")).unwrap();
    let taproom = state
        .rooms
        .get_mut(&RoomId::from("rusty_flagon_taproom"))
        .unwrap();
    taproom.items.retain(|i| *i != shelf_id);
    let contents = state.items.remove(&shelf_id).unwrap().contents;
    for i in contents {
        state.items.remove(&i);
    }
    let w = World::from_state(Arc::new(WorldPack::crypt_v01().unwrap()), state, Vec::new());
    assert_eq!(on_shelf(&w, "ale"), 5);
    assert!(!carries_stock(&w, &id("marta")));
    assert_eq!(w.state.check(), Vec::<String>::new());
}

fn crypt_sources(edit: impl Fn(&str, &str) -> String) -> Result<WorldPack, crate::pack::PackError> {
    let world = edit(
        "world",
        include_str!("../../../../worlds/crypt-v01/world.toml"),
    );
    let items = edit(
        "items",
        include_str!("../../../../worlds/crypt-v01/items.toml"),
    );
    let economy = edit(
        "economy",
        include_str!("../../../../worlds/crypt-v01/economy.toml"),
    );
    let cast = edit(
        "cast",
        include_str!("../../../../worlds/crypt-v01/cast.toml"),
    );
    WorldPack::from_sources(PackSources {
        world: &world,
        skills: include_str!("../../../../worlds/crypt-v01/skills.toml"),
        items: &items,
        economy: &economy,
        cast: &cast,
    })
}

fn problems_of(file: &'static str, from: &'static str, to: &'static str) -> String {
    let err = crypt_sources(|f, text| {
        if f == file {
            assert!(text.contains(from), "{from:?} is not in {f}");
            text.replacen(from, to, 1)
        } else {
            text.to_owned()
        }
    })
    .unwrap_err();
    err.to_string()
}

#[test]
fn the_pack_checks_fixtures_posts_and_relocks() {
    let e = problems_of("economy", "fixture = \"flagon_shelf\"", "fixture = \"ale\"");
    assert!(
        e.contains("fixture ale must be a fixed, unlocked container"),
        "{e}"
    );
    let e = problems_of(
        "economy",
        "fixture = \"flagon_shelf\"",
        "fixture = \"nothing\"",
    );
    assert!(e.contains("fixture names unknown item"), "{e}");
    let e = problems_of(
        "economy",
        "fixture = \"smithy_rack\"",
        "fixture = \"flagon_shelf\"",
    );
    assert!(e.contains("share the fixture flagon_shelf"), "{e}");
    let e = problems_of(
        "economy",
        "fixture = \"flagon_shelf\"",
        "fixture = \"flagon_shelf\"\nroom = \"moon\"",
    );
    assert!(e.contains("shop marta names unknown room"), "{e}");
    let e = problems_of("economy", "fixture = \"flagon_shelf\"", "unattended = true");
    assert!(e.contains("shop marta: unattended needs a fixture"), "{e}");
    let e = problems_of("economy", "fixture = \"flagon_shelf\"", "room = \"smithy\"");
    assert!(e.contains("shop marta: room needs a fixture"), "{e}");
    let e = problems_of(
        "cast",
        "post = { rooms = [\"smithy\"] }",
        "post = { rooms = [\"forge\"] }",
    );
    assert!(e.contains("cast torin post names unknown room"), "{e}");
    let e = problems_of(
        "cast",
        "post = { rooms = [\"smithy\"] }",
        "post = { rooms = [] }",
    );
    assert!(
        e.contains("cast torin: post needs at least one room"),
        "{e}"
    );
    let e = problems_of("world", "relock_minutes = 30", "relock_minutes = 0");
    assert!(e.contains("relock_minutes is at least 1"), "{e}");
    let e = problems_of("world", "retreat_free = true", "retreat_frees = true");
    assert!(e.contains("retreat_frees"), "a typo fails the pack: {e}");
}

#[test]
fn a_fixture_shop_in_another_room_trades_there() {
    let mut w = with_pack(|p| {
        p.economy
            .shops
            .get_mut(&ShopId::from("marta"))
            .unwrap()
            .room = Some(RoomId::from("town_square"));
    });
    let shelf_at = w.state.holder_of(shelf(&w));
    assert_eq!(
        shelf_at,
        Some(Holder::Room {
            room: RoomId::from("town_square")
        })
    );
    let ada = w.login("ada", 1).unwrap();
    assert_eq!(
        refused(&mut w, &ada, "buy ale").error_code,
        Some(ErrorCode::NoKeeper),
        "Marta is in the Flagon, not at the stall"
    );
    place(&mut w, &id("marta"), "town_square");
    ok(&mut w, &ada, "buy ale");
}

/// The proto of each item `who` has equipped, by slot.
fn equipped_protos(w: &World, who: &EntityId) -> Vec<(Slot, String)> {
    w.entity(who)
        .unwrap()
        .equipped
        .iter()
        .map(|(s, i)| (*s, w.state.items[i].proto.to_string()))
        .collect()
}

/// The protos `who` carries, sorted.
fn carried_protos(w: &World, who: &EntityId) -> Vec<String> {
    let mut v: Vec<String> = w
        .entity(who)
        .unwrap()
        .inventory
        .iter()
        .map(|i| w.state.items[i].proto.to_string())
        .collect();
    v.sort();
    v
}

/// The save the owner's log ran on: crypt-v01 before fixtures, with the
/// old loadouts (Torin unarmed, kestrel with only a shortsword).
fn the_owners_old_save() -> WorldState {
    let mut pack = body_stock_pack();
    let torin = pack.cast.get_mut(&id("torin")).unwrap();
    torin.inventory.clear();
    torin.equip.clear();
    let kestrel = pack.cast.get_mut(&id("pc_kestrel")).unwrap();
    kestrel.inventory = vec![ProtoId::from("shortsword")];
    kestrel.equip = vec![ProtoId::from("shortsword")];
    let old = World::new(Arc::new(pack), 42);
    let mut json = serde_json::to_value(old.snapshot()).unwrap();
    let obj = json.as_object_mut().unwrap();
    obj.remove("shop_fixtures");
    obj.remove("cast_gear");
    serde_json::from_value(json).unwrap()
}

#[test]
fn an_older_save_gives_the_existing_cast_its_new_starting_gear() {
    let old = the_owners_old_save();
    let before = old.items.len();
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let money = old.total_currency(&pack);
    let w = World::from_state(Arc::clone(&pack), old, Vec::new());
    let torin = id("torin");
    assert_eq!(carried_protos(&w, &torin), ["broadsword", "leather_jerkin"]);
    assert_eq!(
        equipped_protos(&w, &torin),
        [
            (Slot::Weapon, "broadsword".to_owned()),
            (Slot::Torso, "leather_jerkin".to_owned())
        ]
    );
    assert!(!carries_stock(&w, &torin), "his own gear is not stock");
    let rack = w.state.shop_fixtures[&ShopId::from("torin")];
    assert_eq!(
        w.state.items[&rack].contents.len(),
        6,
        "the rack keeps all six"
    );
    let kestrel = id("pc_kestrel");
    assert_eq!(
        carried_protos(&w, &kestrel),
        ["bandage", "shortsword", "torch"]
    );
    assert!(
        equipped_protos(&w, &kestrel).contains(&(Slot::Light, "torch".to_owned())),
        "kestrel holds a lit torch"
    );
    assert_eq!(
        w.state.items.len(),
        before + 3 + 4,
        "three fixtures, Torin's two and kestrel's two"
    );
    assert_eq!(w.state.total_currency(&pack), money);
    assert_eq!(w.state.check(), Vec::<String>::new());
    // The migration runs once: a second load makes nothing more.
    let count = w.state.items.len();
    let again = World::from_state(Arc::clone(&pack), w.snapshot(), Vec::new());
    assert_eq!(again.state.items.len(), count);
    assert_eq!(again.snapshot(), w.snapshot());
}

#[test]
fn starting_gear_a_cast_member_lost_does_not_come_back_on_load() {
    let mut w = crypt();
    let torin = id("torin");
    ok(&mut w, &torin, "drop broadsword");
    let count = w.state.items.len();
    let pack = Arc::clone(&w.pack);
    let w = World::from_state(Arc::clone(&pack), w.snapshot(), Vec::new());
    assert_eq!(w.state.items.len(), count, "no second broadsword");
    assert_eq!(carried_protos(&w, &torin), ["leather_jerkin"]);
}

#[test]
fn a_pack_that_adds_starting_gear_gives_only_the_new_piece() {
    let w = crypt();
    let count = w.state.items.len();
    let mut pack = WorldPack::crypt_v01().unwrap();
    let def = pack.cast.get_mut(&id("torin")).unwrap();
    def.inventory.push(ProtoId::from("lantern"));
    def.equip.push(ProtoId::from("lantern"));
    // Money sealed in an item is never made by a pack edit.
    def.inventory.push(ProtoId::from("old_coin_pouch"));
    let pack = Arc::new(pack);
    let money = w.state.total_currency(&pack);
    let w = World::from_state(Arc::clone(&pack), w.snapshot(), Vec::new());
    assert_eq!(w.state.items.len(), count + 1, "only the lantern");
    assert_eq!(
        carried_protos(&w, &id("torin")),
        ["broadsword", "lantern", "leather_jerkin"]
    );
    assert!(equipped_protos(&w, &id("torin")).contains(&(Slot::Light, "lantern".to_owned())));
    assert_eq!(w.state.total_currency(&pack), money);
    assert_eq!(w.state.check(), Vec::<String>::new());
}

#[test]
fn a_pack_that_moves_a_shop_room_moves_its_fixture_on_load() {
    let mut w = crypt();
    let shelf_id = shelf(&w);
    let count = w.state.items.len();
    let mut pack = WorldPack::crypt_v01().unwrap();
    pack.economy
        .shops
        .get_mut(&ShopId::from("marta"))
        .unwrap()
        .room = Some(RoomId::from("town_square"));
    w = World::from_state(Arc::new(pack), w.snapshot(), Vec::new());
    assert_eq!(shelf(&w), shelf_id, "the same shelf, moved");
    assert_eq!(
        w.state.holder_of(shelf_id),
        Some(Holder::Room {
            room: RoomId::from("town_square")
        })
    );
    assert_eq!(on_shelf(&w, "ale"), 5, "its stock moves with it");
    assert_eq!(w.state.items.len(), count);
    assert_eq!(w.state.check(), Vec::<String>::new());
    let ada = w.login("ada", 1).unwrap();
    place(&mut w, &id("marta"), "town_square");
    ok(&mut w, &ada, "examine shelf");
    ok(&mut w, &ada, "buy ale");
    ok(&mut w, &ada, "e");
    assert_eq!(
        refused(&mut w, &ada, "examine shelf").error_code,
        Some(ErrorCode::NoSuchTarget),
        "no shelf is left in the taproom"
    );
}

/// A pack with no fixture whose keeper is a mob: the goblin trader keeps
/// Vich's stock on its body.
fn mob_keeper_pack() -> WorldPack {
    let mut pack = body_stock_pack();
    let sid = ShopId::from("vich");
    pack.economy.shops.get_mut(&sid).unwrap().keeper = id("giant_rat_1");
    pack.cast.get_mut(&id("vich")).unwrap().shop = None;
    pack.cast.get_mut(&id("giant_rat_1")).unwrap().shop = Some(sid);
    pack
}

#[test]
fn a_mob_keeper_that_dies_drops_its_stock_as_plain_loot() {
    let mut w = World::new(Arc::new(mob_keeper_pack()), 42);
    let rat = id("giant_rat_1");
    assert!(carries_stock(&w, &rat));
    let ada = w.login("ada", 1).unwrap();
    place(&mut w, &ada, "wine_vault");
    let mut ctx = Ctx {
        now: 1_000,
        ..Ctx::default()
    };
    w.wound(&mut ctx, &rat, 1_000, &ada);
    assert!(w.entity(&rat).unwrap().dead_until_ms.is_some());
    let floor = &w.state.rooms[&RoomId::from("wine_vault")].items;
    assert!(floor.iter().all(|i| !w.state.items[i].stock));
    assert_eq!(w.state.check(), Vec::<String>::new());
    ok(&mut w, &ada, "take key");
}

#[test]
fn the_check_flags_stock_outside_a_shop() {
    let mut w = crypt();
    let ale = w.state.items[&shelf(&w)].contents[0];
    w.state.move_item(
        ale,
        &Holder::Room {
            room: RoomId::from("town_square"),
        },
    );
    let bad = w.state.check();
    assert!(
        bad.iter().any(|m| m.contains("is stock outside a shop")),
        "{bad:?}"
    );
}
