//! The balance of crypt-v01, measured with the real engine.
//!
//! Each fight runs through `World::execute` with parsed lines (`w`, `s`,
//! `attack rat`, `flee`, `use bandage`), and the engine's own mob round
//! resolves every strike. A row plays one seeded world per seed, so a row
//! is deterministic. The fast test plays a few hundred seeds per row and
//! checks wide bands, so a pack edit that breaks the balance fails here.
//! It also checks that each heal in a run works, that the pups almost
//! never kill, and the orderings with a margin each (a heal between the
//! rats helps, a bigger party wins more, a broadsword beats a shortsword
//! over the party sizes summed, the dagger beats the grave-blade, the
//! pups are easier than a rat, and the hatchet beats the dagger against
//! the crawler and the warden). It checks that the hatchet and the dagger
//! stay close against a rat.
//! The ignored test plays more seeds, prints the table, and checks the
//! targets themselves (the win rate, for one rat the share of wins that
//! end wounded, and for the pups the share of deaths), the orderings and
//! the close pairs with the margins the targets ask for. It prints a
//! `MISS` line for each one that fails:
//!
//! ```text
//! cargo test -p mud-world --lib balance_table -- --ignored --nocapture
//! ```
//!
//! `BAL_N` sets the seeds per row (default 2000), `BAL_FIRST` the first
//! seed (default 1), `BAL_PACK` names a pack
//! directory to measure in place of crypt-v01, and `BAL_ROWS` keeps only
//! the rows whose label contains one of its comma-separated words.
//!
//! How a row reads:
//! - "fresh" is the player template as it logs in.
//! - "sword" and "broad" swap the template's weapon for a shortsword or a
//!   broadsword and add a leather jerkin.
//! - A fight ends when the mob cannot act (a win) or when no player in
//!   the party can act (a loss).
//! - "wounded" means the winner ends at half HP or less.
//! - "collapsed" and "dead" count a party in which any member fell.
//! - In the rat run one player walks into the wine vault, fights the
//!   first rat, may heal with no hostile in the room, and walks on into
//!   the warren to take the second rat's strike on entering.
//! - A warden row walks in from the cistern. Each member of a party acts
//!   once per round and takes its own strike on entering.
//! - A "respawned" warden row first kills the warden and ticks the world
//!   past its respawn, so the party meets the warden that comes back.
//! - The pups row walks one player from the landing into the root cellar,
//!   where both pups strike on entering. It fights the runt, then the
//!   squealer, and each pup still standing strikes after every action.
//! - A crawler row walks down the dwarf stair from the ossuary, with the
//!   stair open and the warden out of the way, so it measures the
//!   crawler alone. "hatchet" swaps the dagger for a hatchet, no armor,
//!   in a crawler, rat or warden row.
//! - A "saved" row makes its world, and logs the party in, under the pack
//!   as it stood before the balance step. It then loads that state under
//!   the pack as it is now, as a server that restarts on an edited pack
//!   does. Reconcile does not refresh a cast entity's attrs or skills, or
//!   a player's template skills, so these rows measure what an old world
//!   meets. They have bands but no targets: the targets hold for a fresh
//!   world.

use std::fmt::Write as _;
use std::sync::Arc;

use super::*;
use crate::ids::SkillId;
use crate::pack::Slot;

const NAMES: [&str; 3] = ["ada", "bob", "cyd"];
const MAX_ROUNDS: u32 = 120;
/// The time between two actions of a fight, in ms.
const STEP_MS: u64 = 3_000;
const START_MS: u64 = 10_000;

fn eid(s: &str) -> EntityId {
    EntityId::from(s)
}

#[derive(Clone, Copy, Debug)]
enum Heal {
    None,
    Bandage,
    Draught,
}

#[derive(Clone, Copy, Debug)]
enum Scenario {
    Duel {
        mob: &'static str,
        word: &'static str,
        from: &'static str,
        dir: &'static str,
        party: usize,
        retreat_at: Option<i32>,
        respawned: bool,
    },
    RatRun(Heal),
    /// One player walks into a room of several mobs and fights them in
    /// turn, each by its own word, while every one still standing strikes.
    Den {
        mobs: &'static [(&'static str, &'static str)],
        from: &'static str,
        dir: &'static str,
    },
}

impl Scenario {
    /// How many players the scenario logs in.
    const fn party(self) -> usize {
        match self {
            Self::Duel { party, .. } => party,
            Self::RatRun(_) | Self::Den { .. } => 1,
        }
    }

    /// Whether the scenario heals between the rats.
    const fn heals(self) -> bool {
        !matches!(
            self,
            Self::RatRun(Heal::None) | Self::Duel { .. } | Self::Den { .. }
        )
    }
}

#[derive(Default, Clone, Copy, Debug)]
struct Stats {
    trials: u64,
    wins: u64,
    escaped: u64,
    collapsed: u64,
    dead: u64,
    rounds: u64,
    hp_left: i64,
    winners: u64,
    wounded: u64,
    /// Heals used between the rats, and the HP they gave back.
    heals: u64,
    healed: i64,
}

impl Stats {
    const fn add(&mut self, o: &Self) {
        self.trials += o.trials;
        self.wins += o.wins;
        self.escaped += o.escaped;
        self.collapsed += o.collapsed;
        self.dead += o.dead;
        self.rounds += o.rounds;
        self.hp_left += o.hp_left;
        self.winners += o.winners;
        self.wounded += o.wounded;
        self.heals += o.heals;
        self.healed += o.healed;
    }

    /// The win rate in tenths of a percent.
    fn win_permille(&self) -> u64 {
        permille(self.wins, self.trials)
    }
}

fn permille(a: u64, b: u64) -> u64 {
    (a * 1000 + b / 2).checked_div(b).unwrap_or(0)
}

/// `num / den` with `places` decimals, rounded half away from zero.
fn decimal(num: i64, den: u64, places: u32) -> String {
    if den == 0 {
        return "-".to_owned();
    }
    let scale = 10u64.pow(places);
    let scaled = (num.unsigned_abs() * scale + den / 2) / den;
    let sign = if num < 0 && scaled > 0 { "-" } else { "" };
    let whole = scaled / scale;
    let frac = scaled % scale;
    let width = usize::try_from(places).unwrap();
    format!("{sign}{whole}.{frac:0width$}")
}

fn pct(a: u64, b: u64) -> String {
    decimal(i64::try_from(a * 100).unwrap(), b, 1)
}

fn can_act_in(w: &World, id: &EntityId, room: &RoomId) -> bool {
    w.state
        .entities
        .get(id)
        .is_some_and(|e| e.can_act() && &e.room == room)
}

fn mob_can_act(w: &World, mob: &EntityId) -> bool {
    w.state.entities[mob].can_act()
}

/// How a fight ended.
struct Outcome {
    won: bool,
    rounds: u32,
    escaped: bool,
}

/// The party fights `mob` until it falls or no member stands.
fn fight(
    w: &mut World,
    party: &[EntityId],
    mob: &EntityId,
    word: &str,
    retreat_at: Option<i32>,
    now: &mut u64,
) -> Outcome {
    let room = w.state.entities[mob].room.clone();
    let mut escaped = false;
    for round in 1..=MAX_ROUNDS {
        for p in party {
            if !mob_can_act(w, mob) {
                break;
            }
            if !can_act_in(w, p, &room) {
                continue;
            }
            *now += STEP_MS;
            let hp = w.state.entities[p].pools.hp;
            let flee = retreat_at.is_some_and(|t| hp <= t);
            let line = if flee {
                "flee".to_owned()
            } else {
                format!("attack {word}")
            };
            let r = w.execute(p, &line, *now);
            assert!(r.ok, "{line:?} failed: {r:?}");
            escaped |= flee && w.state.entities[p].room != room;
        }
        if !mob_can_act(w, mob) {
            return Outcome {
                won: true,
                rounds: round,
                escaped,
            };
        }
        if !party.iter().any(|p| can_act_in(w, p, &room)) {
            return Outcome {
                won: false,
                rounds: round,
                escaped,
            };
        }
    }
    Outcome {
        won: false,
        rounds: MAX_ROUNDS,
        escaped,
    }
}

/// Kills `mob` where it stands and ticks the world past its respawn, so
/// the mob that stands there now is the one that came back. Returns the
/// time after the respawn.
fn respawn(w: &mut World, mob: &str, now: u64) -> u64 {
    let zed = w.login("zed", now).unwrap();
    let id = eid(mob);
    let room = w.state.entities[&id].room.clone();
    w.state.entities.get_mut(&zed).unwrap().room = room;
    let m = w.state.entities.get_mut(&id).unwrap();
    m.pools.hp = -6 * m.pools.hp_max;
    let name = m.aliases.first().cloned().unwrap_or_else(|| m.name.clone());
    // A critical hit on a body past saving: the mob dies. The seeded dice
    // come back after it, so the fight that follows rolls as a first
    // fight would.
    let seeded = w.dice.clone();
    w.fix_dice([1, 1, 1, 6, 6, 6]);
    let r = w.execute(&zed, &format!("attack {name}"), now);
    assert!(r.ok, "{r:?}");
    w.dice = seeded;
    let due = w.state.entities[&id].dead_until_ms.unwrap() + 60_000;
    w.state.entities.get_mut(&zed).unwrap().room = RoomId::from("town_square");
    w.tick(due);
    assert!(mob_can_act(w, &id), "{mob} is back");
    due
}

/// A world made under `old` with the party logged in, then saved and
/// loaded under `pack`. Loading reconciles the state with `pack`, as a
/// server that restarts on an edited pack does. Nothing rolls before the
/// save, so the fight rolls the dice a fresh world would.
fn saved_world(old: &Arc<WorldPack>, pack: &Arc<WorldPack>, sc: Scenario, seed: u64) -> World {
    let mut o = World::new(Arc::clone(old), seed);
    for nm in &NAMES[..sc.party()] {
        o.login(nm, START_MS).unwrap();
    }
    World::from_state(Arc::clone(pack), o.snapshot(), Vec::new())
}

/// One player walks from `from` through `dir` into a room of `mobs` and
/// fights them in turn, each by its word, until one fight is lost.
fn den(
    w: &mut World,
    mobs: &[(&str, &str)],
    from: &str,
    dir: &str,
    now: &mut u64,
) -> (Vec<EntityId>, EntityId, Outcome) {
    let ada = w.login("ada", *now).unwrap();
    w.state.entities.get_mut(&ada).unwrap().room = RoomId::from(from);
    let party = vec![ada.clone()];
    *now += STEP_MS;
    let r = w.execute(&ada, dir, *now);
    assert!(r.ok, "{dir:?} failed: {r:?}");
    let mut rounds = 0;
    let mut last = None;
    for (mob, word) in mobs {
        let mob = eid(mob);
        let mut o = fight(w, &party, &mob, word, None, now);
        rounds += o.rounds;
        o.rounds = rounds;
        let won = o.won;
        last = Some((mob, o));
        if !won {
            break;
        }
    }
    let (mob, outcome) = last.unwrap();
    (party, mob, outcome)
}

fn trial(pack: &Arc<WorldPack>, saved: Option<&Arc<WorldPack>>, sc: Scenario, seed: u64) -> Stats {
    let mut w = saved.map_or_else(
        || World::new(Arc::clone(pack), seed),
        |old| saved_world(old, pack, sc, seed),
    );
    let mut now = START_MS;
    let mut s = Stats {
        trials: 1,
        ..Stats::default()
    };
    let (party, mob, outcome) = match sc {
        Scenario::Duel {
            mob,
            word,
            from,
            dir,
            party,
            retreat_at,
            respawned,
        } => {
            if respawned {
                now = respawn(&mut w, mob, now);
            }
            let party: Vec<EntityId> = NAMES[..party]
                .iter()
                .map(|nm| w.login(nm, now).unwrap())
                .collect();
            for p in &party {
                w.state.entities.get_mut(p).unwrap().room = RoomId::from(from);
            }
            for p in &party {
                now += STEP_MS;
                let r = w.execute(p, dir, now);
                assert!(r.ok, "{dir:?} failed: {r:?}");
            }
            let mob = eid(mob);
            let outcome = fight(&mut w, &party, &mob, word, retreat_at, &mut now);
            (party, mob, outcome)
        }
        Scenario::Den { mobs, from, dir } => den(&mut w, mobs, from, dir, &mut now),
        Scenario::RatRun(heal) => {
            let ada = w.login("ada", now).unwrap();
            w.state.entities.get_mut(&ada).unwrap().room = RoomId::from("cellar_landing");
            let party = vec![ada.clone()];
            now += STEP_MS;
            assert!(w.execute(&ada, "w", now).ok);
            let first = fight(&mut w, &party, &eid("giant_rat_1"), "rat", None, &mut now);
            let mob = eid("giant_rat_2");
            if first.won {
                now += STEP_MS;
                let line = match heal {
                    Heal::None => None,
                    Heal::Bandage => Some("use bandage"),
                    Heal::Draught => Some("use draught"),
                };
                if let Some(line) = line {
                    // A heal on someone unhurt is refused, and the item
                    // stays. A hurt player's heal must work.
                    let before = w.state.entities[&ada].pools;
                    let r = w.execute(&ada, line, now);
                    assert_eq!(r.ok, before.hp < before.hp_max, "{line:?}: {r:?}");
                    if r.ok {
                        let hp = w.state.entities[&ada].pools.hp;
                        assert!(hp >= before.hp, "{line:?} took HP: {r:?}");
                        s.heals += 1;
                        s.healed += i64::from(hp - before.hp);
                    }
                }
                now += STEP_MS;
                assert!(w.execute(&ada, "w", now).ok, "into the warren");
                let mut second = fight(&mut w, &party, &mob, "rat", None, &mut now);
                second.rounds += first.rounds;
                (party, mob, second)
            } else {
                (party, mob, first)
            }
        }
    };
    s.rounds = u64::from(outcome.rounds);
    if outcome.won {
        s.wins = 1;
        let room = w.state.entities[&mob].room.clone();
        for p in &party {
            if can_act_in(&w, p, &room) {
                let e = &w.state.entities[p];
                s.hp_left += i64::from(e.pools.hp);
                s.winners += 1;
                if 2 * e.pools.hp <= e.pools.hp_max {
                    s.wounded = 1;
                }
            }
        }
    } else if outcome.escaped {
        s.escaped = 1;
    }
    s.collapsed = u64::from(party.iter().any(|p| w.state.entities[p].unconscious));
    s.dead = u64::from(party.iter().any(|p| w.state.entities[p].deaths > 0));
    s
}

/// Plays seeds `first..first + n`, split over a few threads.
fn run(
    pack: &Arc<WorldPack>,
    saved: Option<&Arc<WorldPack>>,
    sc: Scenario,
    first: u64,
    n: u64,
) -> Stats {
    const THREADS: u64 = 4;
    let parts: Vec<Stats> = std::thread::scope(|scope| {
        #[expect(
            clippy::needless_collect,
            reason = "every thread starts before the first join"
        )]
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let pack = Arc::clone(pack);
                let saved = saved.map(Arc::clone);
                scope.spawn(move || {
                    let mut acc = Stats::default();
                    let mut seed = first + t;
                    while seed < first + n {
                        acc.add(&trial(&pack, saved.as_ref(), sc, seed));
                        seed += THREADS;
                    }
                    acc
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut s = Stats::default();
    for p in &parts {
        s.add(p);
    }
    s
}

// ------------------------------------------------------------ builds

type Build = fn(&mut WorldPack);

const fn fresh(_: &mut WorldPack) {}

fn with_draught(p: &mut WorldPack) {
    p.player_template
        .inventory
        .push(ProtoId::from("healing_draught"));
}

/// The template's weapon swapped for `weapon`, and `armor` worn when
/// there is one.
fn armed(p: &mut WorldPack, weapon: &str, armor: Option<&str>) {
    let is_weapon = |x: &ProtoId| p.items.get(x).is_some_and(|d| d.slot == Some(Slot::Weapon));
    let mut gear: Vec<ProtoId> = vec![ProtoId::from(weapon)];
    gear.extend(armor.map(ProtoId::from));
    let mut inv = gear.clone();
    inv.extend(
        p.player_template
            .inventory
            .iter()
            .filter(|x| !is_weapon(x))
            .cloned(),
    );
    let mut equip = gear;
    equip.extend(
        p.player_template
            .equip
            .iter()
            .filter(|x| !is_weapon(x))
            .cloned(),
    );
    p.player_template.inventory = inv;
    p.player_template.equip = equip;
}

fn sword(p: &mut WorldPack) {
    armed(p, "shortsword", Some("leather_jerkin"));
}

fn broad(p: &mut WorldPack) {
    armed(p, "broadsword", Some("leather_jerkin"));
}

/// The template wields the warden's grave-blade, as a player who loots
/// it might.
fn grave_blade(p: &mut WorldPack) {
    armed(p, "warden_blade", None);
}

/// The mobs and the template as they stood before the balance step:
/// brawling-12 rats, the warren rat at HT 12, a broadsword-12 HT-12
/// warden, and no broadsword skill in the template. Item damage is read
/// from the pack at each strike, so it needs no entry here.
fn before_balance(p: &mut WorldPack) {
    let skill = |p: &mut WorldPack, who: &str, name: &str, level: i32| {
        p.cast
            .get_mut(&eid(who))
            .unwrap()
            .skills
            .insert(SkillId::from(name), level);
    };
    skill(p, "giant_rat_1", "brawling", 12);
    skill(p, "giant_rat_2", "brawling", 12);
    skill(p, "skeletal_warden", "broadsword", 12);
    p.cast.get_mut(&eid("giant_rat_2")).unwrap().attrs.ht = 12;
    p.cast.get_mut(&eid("skeletal_warden")).unwrap().attrs.ht = 12;
    p.player_template
        .skills
        .remove(&SkillId::from("broadsword"));
}

/// The template plays kestrel, the cast's agent PC.
fn kestrel(p: &mut WorldPack) {
    let k = p.cast[&eid("pc_kestrel")].clone();
    p.player_template.attrs = k.attrs;
    p.player_template.skills = k.skills;
    p.player_template.inventory = k.inventory;
    p.player_template.equip = k.equip;
}

fn kestrel_jerkin(p: &mut WorldPack) {
    kestrel(p);
    sword(p);
}

/// The template wields a hatchet, with no armor.
fn hatchet(p: &mut WorldPack) {
    armed(p, "hatchet", None);
}

/// The dwarf stair stands open and the warden waits in the smithy, so a
/// row can walk from the ossuary straight down to the crawler.
fn drift_open(p: &mut WorldPack) {
    p.rooms
        .get_mut(&RoomId::from("ossuary_crypt"))
        .unwrap()
        .locks
        .clear();
    p.cast.get_mut(&eid("skeletal_warden")).unwrap().room = RoomId::from("smithy");
}

fn fresh_drift(p: &mut WorldPack) {
    drift_open(p);
}

fn sword_drift(p: &mut WorldPack) {
    sword(p);
    drift_open(p);
}

fn hatchet_drift(p: &mut WorldPack) {
    hatchet(p);
    drift_open(p);
}

/// The first rat waits in the smithy, out of the second rat's fight.
fn no_rat1(p: &mut WorldPack) {
    p.cast.get_mut(&eid("giant_rat_1")).unwrap().room = RoomId::from("smithy");
}

fn sword_no_rat1(p: &mut WorldPack) {
    sword(p);
    no_rat1(p);
}

// ------------------------------------------------------------- rows

const fn rat1() -> Scenario {
    Scenario::Duel {
        mob: "giant_rat_1",
        word: "rat",
        from: "cellar_landing",
        dir: "w",
        party: 1,
        retreat_at: None,
        respawned: false,
    }
}

const fn rat2() -> Scenario {
    Scenario::Duel {
        mob: "giant_rat_2",
        word: "rat",
        from: "wine_vault",
        dir: "w",
        party: 1,
        retreat_at: None,
        respawned: false,
    }
}

const fn warden(party: usize, respawned: bool) -> Scenario {
    Scenario::Duel {
        mob: "skeletal_warden",
        word: "warden",
        from: "flooded_cistern",
        dir: "s",
        party,
        retreat_at: None,
        respawned,
    }
}

/// Both rat pups of the root cellar, the runt first.
const fn pups() -> Scenario {
    Scenario::Den {
        mobs: &[("rat_pup_1", "runt"), ("rat_pup_2", "squealer")],
        from: "cellar_landing",
        dir: "s",
    }
}

const fn crawler(party: usize) -> Scenario {
    Scenario::Duel {
        mob: "cave_crawler",
        word: "crawler",
        from: "ossuary_crypt",
        dir: "d",
        party,
        retreat_at: None,
        respawned: false,
    }
}

/// A bound on a rate, in percent: `(low, high)`, both inclusive.
type Bound = (u64, u64);

/// One row of the table.
struct Row {
    label: String,
    build: Build,
    scenario: Scenario,
    /// The pack the world is saved under before the row plays it, made
    /// from the row's own pack. `None` plays a fresh world.
    saved: Option<Build>,
    /// The band the fast test holds the win rate to.
    band: Option<Bound>,
    /// The target the long run holds the win rate to.
    target: Option<Bound>,
    /// The target the long run holds the share of wounded wins to.
    wounded: Option<Bound>,
    /// The bound every run holds the share of deaths to.
    dead: Option<Bound>,
}

impl Row {
    const fn target(mut self, lo: u64, hi: u64) -> Self {
        self.target = Some((lo, hi));
        self
    }

    const fn wounded(mut self, lo: u64, hi: u64) -> Self {
        self.wounded = Some((lo, hi));
        self
    }

    const fn dead(mut self, lo: u64, hi: u64) -> Self {
        self.dead = Some((lo, hi));
        self
    }

    const fn saved(mut self, under: Build) -> Self {
        self.saved = Some(under);
        self
    }
}

fn row(label: &str, build: Build, scenario: Scenario, band: Bound) -> Row {
    Row {
        band: Some(band),
        ..info(label, build, scenario)
    }
}

fn info(label: &str, build: Build, scenario: Scenario) -> Row {
    Row {
        label: label.to_owned(),
        build,
        scenario,
        saved: None,
        band: None,
        target: None,
        wounded: None,
        dead: None,
    }
}

/// The checked rows. Each band is wider than its target, so the few seeds
/// of the fast test and small pack edits stay inside it. The long run
/// checks the targets (design-v0.1 section 7). The "saved" rows have
/// bands only, so the gap an old world meets cannot widen unseen.
fn targets() -> Vec<Row> {
    let mut rows = vec![
        row("fresh vs rat1", fresh, rat1(), (62, 88))
            .target(70, 80)
            .wounded(35, 65),
        row("fresh vs rat2", no_rat1, rat2(), (55, 85))
            .target(70, 80)
            .wounded(35, 65),
        row(
            "fresh run, no heal",
            fresh,
            Scenario::RatRun(Heal::None),
            (25, 55),
        )
        .target(35, 50),
        row(
            "fresh run, bandage between",
            fresh,
            Scenario::RatRun(Heal::Bandage),
            (30, 62),
        ),
        row(
            "fresh run, draught between",
            with_draught,
            Scenario::RatRun(Heal::Draught),
            (35, 68),
        ),
        row("grave-blade vs rat1", grave_blade, rat1(), (0, 80)),
        row("hatchet vs rat1", hatchet, rat1(), (55, 88)),
        row("fresh vs the pups", fresh, pups(), (80, 97))
            .target(85, 95)
            .dead(0, 2),
        row("fresh vs crawler", fresh_drift, crawler(1), (40, 66)).target(45, 60),
        row("hatchet vs crawler", hatchet_drift, crawler(1), (50, 78)),
        row("sword vs crawler", sword_drift, crawler(1), (82, 100)).target(85, 100),
    ];
    for respawned in [false, true] {
        let w = if respawned {
            "respawned warden"
        } else {
            "warden"
        };
        let at = |s: &str| format!("{s} vs {w}");
        rows.extend([
            row(&at("fresh"), fresh, warden(1, respawned), (0, 12)).target(0, 10),
            row(&at("hatchet"), hatchet, warden(1, respawned), (4, 24)),
            row(&at("sword x1"), sword, warden(1, respawned), (14, 42)).target(20, 35),
            row(&at("sword x2"), sword, warden(2, respawned), (52, 82)).target(60, 75),
            row(&at("sword x3"), sword, warden(3, respawned), (78, 100)).target(85, 100),
            row(&at("broad x1"), broad, warden(1, respawned), (18, 48)),
            row(&at("broad x2"), broad, warden(2, respawned), (56, 88)),
            row(&at("broad x3"), broad, warden(3, respawned), (82, 100)),
        ]);
    }
    rows.extend([
        row("saved: fresh vs rat1", fresh, rat1(), (48, 75)).saved(before_balance),
        row(
            "saved: fresh run, no heal",
            fresh,
            Scenario::RatRun(Heal::None),
            (6, 28),
        )
        .saved(before_balance),
        row(
            "saved: sword x3 vs warden",
            sword,
            warden(3, false),
            (36, 68),
        )
        .saved(before_balance),
        row("saved: broad x3 vs warden", broad, warden(3, false), (0, 8)).saved(before_balance),
    ]);
    rows
}

/// Rows the long run prints for reference, with no target.
fn reference() -> Vec<Row> {
    let retreat = Scenario::Duel {
        mob: "giant_rat_1",
        word: "rat",
        from: "cellar_landing",
        dir: "w",
        party: 1,
        retreat_at: Some(3),
        respawned: false,
    };
    vec![
        info("fresh vs rat1, retreat at HP<=3", fresh, retreat),
        info("sword vs rat1", sword, rat1()),
        info("sword vs rat2", sword_no_rat1, rat2()),
        info("sword run, no heal", sword, Scenario::RatRun(Heal::None)),
        info("kestrel vs rat1", kestrel, rat1()),
        info(
            "kestrel run, bandage between",
            kestrel,
            Scenario::RatRun(Heal::Bandage),
        ),
        info("kestrel vs warden", kestrel, warden(1, false)),
        info("kestrel+jerkin vs warden", kestrel_jerkin, warden(1, false)),
        info(
            "2x kestrel+jerkin vs warden",
            kestrel_jerkin,
            warden(2, false),
        ),
    ]
}

fn measure(base: &WorldPack, rows: &[Row], first: u64, n: u64) -> Vec<Stats> {
    rows.iter()
        .map(|r| {
            let mut p = base.clone();
            (r.build)(&mut p);
            let saved = r.saved.map(|under| {
                let mut old = p.clone();
                under(&mut old);
                Arc::new(old)
            });
            run(&Arc::new(p), saved.as_ref(), r.scenario, first, n)
        })
        .collect()
}

fn table(rows: &[Row], stats: &[Stats]) -> String {
    let mut out = String::from(
        "| matchup | win % | escaped % | mean rounds | mean HP left (winners) | wounded on win % | collapsed % | dead % | heals | mean HP per heal |\n|---|---|---|---|---|---|---|---|---|---|\n",
    );
    for (r, s) in rows.iter().zip(stats) {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            r.label,
            pct(s.wins, s.trials),
            pct(s.escaped, s.trials),
            decimal(i64::try_from(s.rounds).unwrap(), s.trials, 2),
            decimal(s.hp_left, s.winners, 2),
            pct(s.wounded, s.wins),
            pct(s.collapsed, s.trials),
            pct(s.dead, s.trials),
            s.heals,
            decimal(s.healed, s.heals, 2),
        );
    }
    out
}

fn outside(label: &str, what: &str, (lo, hi): Bound, a: u64, b: u64) -> Option<String> {
    let got = permille(a, b);
    (got < lo * 10 || got > hi * 10)
        .then(|| format!("{label}: {what} {}% is outside {lo}-{hi}%", pct(a, b)))
}

/// The rows that miss a bound, as lines. Every run checks the bands and
/// that each heal row healed; the long run (`long`) also checks the
/// targets.
fn misses(rows: &[Row], stats: &[Stats], long: bool) -> Vec<String> {
    let mut out = Vec::new();
    for (r, s) in rows.iter().zip(stats) {
        let mut check = |what: &str, bound: Option<Bound>, a: u64, b: u64| {
            out.extend(bound.and_then(|bd| outside(&r.label, what, bd, a, b)));
        };
        check("win", r.band, s.wins, s.trials);
        check("dead", r.dead, s.dead, s.trials);
        if long {
            check("win (target)", r.target, s.wins, s.trials);
            check("wounded on win (target)", r.wounded, s.wounded, s.wins);
        }
        if r.scenario.heals() && (s.heals == 0 || s.healed <= 0) {
            out.push(format!("{}: no heal worked", r.label));
        }
    }
    out
}

fn win(rows: &[Row], stats: &[Stats], label: &str) -> u64 {
    rows.iter()
        .zip(stats)
        .find(|(r, _)| r.label == label)
        .map(|(_, s)| s.win_permille())
        .unwrap()
}

/// One ordering: the rows of `better` win, summed, at least a margin
/// more than the rows of `worse`, in tenths of a percent. `fast` is the
/// margin of the fast test and `long` the one of the long run; `None`
/// leaves the ordering out of that run. Rows share their seeds, so their
/// noise is correlated, but a few hundred seeds still blur a gap of a few
/// points. So the fast test sums the broadsword rows over the party sizes,
/// and the long run checks each size.
struct Order {
    better: Vec<String>,
    worse: Vec<String>,
    fast: Option<u64>,
    long: Option<u64>,
}

fn order(better: &str, worse: &str, fast: u64, long: u64) -> Order {
    Order {
        better: vec![better.to_owned()],
        worse: vec![worse.to_owned()],
        fast: Some(fast),
        long: Some(long),
    }
}

/// The orderings the targets ask for.
/// - A heal between the rats helps clearly: a bandage by 5 points or more,
///   a draught by 10 or more, and a draught by 2 more than a bandage.
/// - A bigger party wins more: 20 points from one to two, 10 from two to
///   three.
/// - A broadsword beats a shortsword by a point or more at each size.
/// - The starting dagger beats the grave-blade by 10 points or more. The
///   blade is worth nothing (items.toml), and this keeps its price true.
/// - The root cellar is the way in: a fresh player beats both pups 10
///   points more often than one giant rat.
/// - Against the crawler's chitin, the hatchet beats the dagger by 8
///   points or more, and a shortsword with a jerkin beats the hatchet by
///   20 or more. So the hatchet earns its price, and a sword still pays.
/// - Against the warden's bone, one player with the hatchet beats one
///   with the dagger by 8 points or more.
fn orders() -> Vec<Order> {
    let (bandage, draught, none) = (
        "fresh run, bandage between",
        "fresh run, draught between",
        "fresh run, no heal",
    );
    let mut out = vec![
        order(bandage, none, 10, 50),
        order(draught, none, 60, 100),
        order(draught, bandage, 10, 20),
        order("fresh vs rat1", "grave-blade vs rat1", 100, 100),
        order("fresh vs the pups", "fresh vs rat1", 50, 100),
        order("hatchet vs crawler", "fresh vs crawler", 40, 80),
        order("sword vs crawler", "hatchet vs crawler", 150, 200),
        order("hatchet vs warden", "fresh vs warden", 40, 80),
        order(
            "hatchet vs respawned warden",
            "fresh vs respawned warden",
            40,
            80,
        ),
    ];
    for foe in ["warden", "respawned warden"] {
        for arm in ["sword", "broad"] {
            out.push(order(
                &format!("{arm} x2 vs {foe}"),
                &format!("{arm} x1 vs {foe}"),
                100,
                200,
            ));
            out.push(order(
                &format!("{arm} x3 vs {foe}"),
                &format!("{arm} x2 vs {foe}"),
                50,
                100,
            ));
        }
        let at = |arm: &str, n: &str| format!("{arm} {n} vs {foe}");
        for n in ["x1", "x2", "x3"] {
            out.push(Order {
                fast: None,
                ..order(&at("broad", n), &at("sword", n), 0, 10)
            });
        }
        let sizes = ["x1", "x2", "x3"];
        out.push(Order {
            better: sizes.iter().map(|n| at("broad", n)).collect(),
            worse: sizes.iter().map(|n| at("sword", n)).collect(),
            fast: Some(30),
            long: None,
        });
    }
    out
}

/// Two rows that win about as often as each other: no more than `fast`
/// (the fast test) or `long` (the long run) apart, in tenths of a percent.
struct Near {
    a: &'static str,
    b: &'static str,
    fast: u64,
    long: u64,
}

/// The rows the targets ask to stay close.
/// - Against a rat the hatchet does about as well as the dagger: within
///   8 points (12 in the fast test).
const fn nears() -> [Near; 1] {
    [Near {
        a: "hatchet vs rat1",
        b: "fresh vs rat1",
        fast: 120,
        long: 80,
    }]
}

/// The orderings and the close pairs that fail, as lines, with the fast
/// margins or the long ones (`long`).
fn order_misses(rows: &[Row], stats: &[Stats], long: bool) -> Vec<String> {
    let sum = |labels: &[String]| labels.iter().map(|l| win(rows, stats, l)).sum::<u64>();
    let show = |permille: u64| decimal(i64::try_from(permille).unwrap(), 10, 1);
    let near = nears().into_iter().filter_map(|n| {
        let gap = if long { n.long } else { n.fast };
        let (a, b) = (win(rows, stats, n.a), win(rows, stats, n.b));
        (a.abs_diff(b) > gap).then(|| {
            format!(
                "{} ({}) and {} ({}) should be within {}",
                n.a,
                show(a),
                n.b,
                show(b),
                show(gap)
            )
        })
    });
    orders()
        .into_iter()
        .filter_map(|o| {
            let margin = if long { o.long } else { o.fast }?;
            let (b, w) = (sum(&o.better), sum(&o.worse));
            (b < w + margin).then(|| {
                format!(
                    "{} ({}) should beat {} ({}) by {}",
                    o.better.join(" + "),
                    show(b),
                    o.worse.join(" + "),
                    show(w),
                    show(margin),
                )
            })
        })
        .chain(near)
        .collect()
}

/// The seeds per row of the fast test: enough to catch a broken balance,
/// few enough for a normal test run.
const FAST_N: u64 = 400;

#[test]
fn crypt_v01_holds_its_balance() {
    let pack = WorldPack::crypt_v01().unwrap();
    let rows = targets();
    let stats = measure(&pack, &rows, 1, FAST_N);
    let mut bad = misses(&rows, &stats, false);
    bad.extend(order_misses(&rows, &stats, false));
    assert!(
        bad.is_empty(),
        "crypt-v01 is out of balance:\n{}\n{}",
        bad.join("\n"),
        table(&rows, &stats)
    );
}

#[test]
fn a_respawned_warden_fights_armed() {
    let pack = Arc::new(WorldPack::crypt_v01().unwrap());
    let mut w = World::new(Arc::clone(&pack), 7);
    respawn(&mut w, "skeletal_warden", START_MS);
    let warden = &w.state.entities[&eid("skeletal_warden")];
    let blade = warden.equipped.get(&Slot::Weapon).unwrap();
    assert_eq!(w.state.items[blade].proto, ProtoId::from("warden_blade"));
    assert_eq!(warden.pools.hp, warden.pools.hp_max);
}

#[test]
fn the_decimal_helper_rounds_and_signs() {
    assert_eq!(decimal(1, 3, 1), "0.3");
    assert_eq!(decimal(2, 3, 2), "0.67");
    assert_eq!(decimal(-5, 2, 1), "-2.5");
    assert_eq!(decimal(-1, 1000, 1), "0.0");
    assert_eq!(decimal(7, 0, 1), "-");
}

#[test]
#[ignore = "the long balance run; prints the table"]
fn balance_table() {
    let base = std::env::var("BAL_PACK").map_or_else(
        |_| WorldPack::crypt_v01().unwrap(),
        |dir| WorldPack::load(std::path::Path::new(&dir)).unwrap(),
    );
    let var = |name: &str, default: u64| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let (first, n) = (var("BAL_FIRST", 1), var("BAL_N", 2000));
    let mut rows = targets();
    rows.extend(reference());
    let only = std::env::var("BAL_ROWS").ok();
    if let Some(only) = &only {
        rows.retain(|r| only.split(',').any(|k| r.label.contains(k)));
    }
    let stats = measure(&base, &rows, first, n);
    println!(
        "\nN={n} seeds per row, from seed {first}\n\n{}",
        table(&rows, &stats)
    );
    let mut bad = misses(&rows, &stats, true);
    if only.is_none() {
        bad.extend(order_misses(&rows, &stats, true));
    }
    for line in &bad {
        println!("MISS {line}");
    }
}
