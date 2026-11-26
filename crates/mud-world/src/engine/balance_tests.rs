//! The balance of crypt-v01, measured with the real engine.
//!
//! Each fight runs through `World::execute` with parsed lines (`w`, `s`,
//! `attack rat`, `flee`, `use bandage`), and the engine's own mob round
//! resolves every strike. A row plays one seeded world per seed, so a row
//! is deterministic. The fast test plays a few hundred seeds per row and
//! checks wide bands, so a pack edit that breaks the balance fails here.
//! The ignored test plays more seeds and prints the table:
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

use std::fmt::Write as _;
use std::sync::Arc;

use super::*;
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

fn trial(pack: &Arc<WorldPack>, sc: Scenario, seed: u64) -> Stats {
    let mut w = World::new(Arc::clone(pack), seed);
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
                    // Refused when unhurt: then the bandage stays.
                    let _ = w.execute(&ada, line, now);
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
fn run(pack: &Arc<WorldPack>, sc: Scenario, first: u64, n: u64) -> Stats {
    const THREADS: u64 = 4;
    let parts: Vec<Stats> = std::thread::scope(|scope| {
        #[expect(
            clippy::needless_collect,
            reason = "every thread starts before the first join"
        )]
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let pack = Arc::clone(pack);
                scope.spawn(move || {
                    let mut acc = Stats::default();
                    let mut seed = first + t;
                    while seed < first + n {
                        acc.add(&trial(&pack, sc, seed));
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

/// The template's weapon swapped for `weapon`, and a leather jerkin worn.
fn geared(p: &mut WorldPack, weapon: &str) {
    let is_weapon = |x: &ProtoId| p.items.get(x).is_some_and(|d| d.slot == Some(Slot::Weapon));
    let mut inv: Vec<ProtoId> = vec![ProtoId::from(weapon), ProtoId::from("leather_jerkin")];
    inv.extend(
        p.player_template
            .inventory
            .iter()
            .filter(|x| !is_weapon(x))
            .cloned(),
    );
    let mut equip: Vec<ProtoId> = vec![ProtoId::from(weapon), ProtoId::from("leather_jerkin")];
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
    geared(p, "shortsword");
}

fn broad(p: &mut WorldPack) {
    geared(p, "broadsword");
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

/// One row of the table, with the band the fast test holds it to, in
/// percent.
struct Row {
    label: String,
    build: Build,
    scenario: Scenario,
    band: Option<(u64, u64)>,
}

fn row(label: &str, build: Build, scenario: Scenario, band: (u64, u64)) -> Row {
    Row {
        label: label.to_owned(),
        build,
        scenario,
        band: Some(band),
    }
}

fn info(label: &str, build: Build, scenario: Scenario) -> Row {
    Row {
        label: label.to_owned(),
        build,
        scenario,
        band: None,
    }
}

/// The rows with a target. The bands are wider than the targets, so the
/// few seeds of the fast test and small pack edits stay inside them; the
/// long run checks the targets themselves.
fn targets() -> Vec<Row> {
    let mut rows = vec![
        row("fresh vs rat1", fresh, rat1(), (62, 88)),
        row("fresh vs rat2", no_rat1, rat2(), (55, 85)),
        row(
            "fresh run, no heal",
            fresh,
            Scenario::RatRun(Heal::None),
            (25, 55),
        ),
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
    ];
    for respawned in [false, true] {
        let w = if respawned {
            "respawned warden"
        } else {
            "warden"
        };
        let at = |s: &str| format!("{s} vs {w}");
        rows.extend([
            row(&at("fresh"), fresh, warden(1, respawned), (0, 12)),
            row(&at("sword x1"), sword, warden(1, respawned), (14, 42)),
            row(&at("sword x2"), sword, warden(2, respawned), (52, 82)),
            row(&at("sword x3"), sword, warden(3, respawned), (78, 100)),
            row(&at("broad x1"), broad, warden(1, respawned), (18, 48)),
            row(&at("broad x2"), broad, warden(2, respawned), (56, 88)),
            row(&at("broad x3"), broad, warden(3, respawned), (82, 100)),
        ]);
    }
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
            run(&Arc::new(p), r.scenario, first, n)
        })
        .collect()
}

fn table(rows: &[Row], stats: &[Stats]) -> String {
    let mut out = String::from(
        "| matchup | win % | escaped % | mean rounds | mean HP left (winners) | wounded on win % | collapsed % | dead % |\n|---|---|---|---|---|---|---|---|\n",
    );
    for (r, s) in rows.iter().zip(stats) {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {} | {} |",
            r.label,
            pct(s.wins, s.trials),
            pct(s.escaped, s.trials),
            decimal(i64::try_from(s.rounds).unwrap(), s.trials, 2),
            decimal(s.hp_left, s.winners, 2),
            pct(s.wounded, s.wins),
            pct(s.collapsed, s.trials),
            pct(s.dead, s.trials),
        );
    }
    out
}

/// The rows outside their band, as lines.
fn misses(rows: &[Row], stats: &[Stats]) -> Vec<String> {
    rows.iter()
        .zip(stats)
        .filter_map(|(r, s)| {
            let (lo, hi) = r.band?;
            let got = s.win_permille();
            (got < lo * 10 || got > hi * 10).then(|| {
                format!(
                    "{}: {}% is outside {lo}-{hi}%",
                    r.label,
                    pct(s.wins, s.trials)
                )
            })
        })
        .collect()
}

fn win(rows: &[Row], stats: &[Stats], label: &str) -> u64 {
    rows.iter()
        .zip(stats)
        .find(|(r, _)| r.label == label)
        .map(|(_, s)| s.win_permille())
        .unwrap()
}

/// The orderings the targets ask for: a heal between the rats helps, a
/// bigger party wins more, and the broadsword is the upper tier. `slack`
/// is in tenths of a percent: a row may fall that far short of the row it
/// should beat. The long run uses none; the fast test, with its few
/// seeds, allows for their noise.
fn order_misses(rows: &[Row], stats: &[Stats], slack: u64) -> Vec<String> {
    let w = |l: &str| win(rows, stats, l);
    let mut out = Vec::new();
    let mut beats = |better: &str, worse: &str| {
        if w(better) + slack <= w(worse) {
            out.push(format!("{better} should beat {worse}"));
        }
    };
    beats("fresh run, bandage between", "fresh run, no heal");
    beats("fresh run, draught between", "fresh run, no heal");
    for foe in ["warden", "respawned warden"] {
        for arm in ["sword", "broad"] {
            beats(&format!("{arm} x2 vs {foe}"), &format!("{arm} x1 vs {foe}"));
            beats(&format!("{arm} x3 vs {foe}"), &format!("{arm} x2 vs {foe}"));
        }
        for n in ["x1", "x2", "x3"] {
            beats(
                &format!("broad {n} vs {foe}"),
                &format!("sword {n} vs {foe}"),
            );
        }
    }
    out
}

/// The seeds per row of the fast test: enough to catch a broken balance,
/// few enough for a normal test run.
const FAST_N: u64 = 400;
/// How far, in tenths of a percent, one row of the fast test may fall
/// short of a row it should beat.
const FAST_SLACK: u64 = 40;

#[test]
fn crypt_v01_holds_its_balance() {
    let pack = WorldPack::crypt_v01().unwrap();
    let rows = targets();
    let stats = measure(&pack, &rows, 1, FAST_N);
    let mut bad = misses(&rows, &stats);
    bad.extend(order_misses(&rows, &stats, FAST_SLACK));
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
    let mut bad = misses(&rows, &stats);
    if only.is_none() {
        bad.extend(order_misses(&rows, &stats, 0));
    }
    for line in &bad {
        println!("MISS {line}");
    }
}
