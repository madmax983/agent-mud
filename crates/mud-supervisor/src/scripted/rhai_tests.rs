//! Rhai mind scripts in the scripted mind.

use std::sync::Arc;

use mud_script::{ScriptHost, ScriptSources};
use mud_world::pack::CastDef;
use mud_world::{Audit, EntityId, Event, Game, ManualClock, Seq, World, WorldPack};
use serde_json::{Value, json};

use super::*;
use crate::gate::Gate;
use crate::wakes::{Booking, WakeBook};

fn pack() -> Arc<WorldPack> {
    Arc::new(WorldPack::crypt_v01().unwrap())
}

fn sources(body: &str, schema: u32) -> ScriptSources {
    let mut s = ScriptSources {
        manifest: Some(format!(
            "[minds.guard_01]\nfile = \"h.rhai\"\nruntime = \"rhai\"\nmemory_schema = {schema}\n"
        )),
        ..ScriptSources::default()
    };
    s.files
        .insert("h.rhai".into(), format!("fn on_wake(ctx) {{\n{body}\n}}\n"));
    s
}

fn host(body: &str) -> Arc<ScriptHost> {
    Arc::new(ScriptHost::from_sources(&sources(body, 1), &pack()).unwrap())
}

struct Rig {
    game: Arc<Game>,
    typing: Arc<Gate>,
    wakes: Arc<WakeBook>,
}

impl Rig {
    fn new() -> Self {
        Self::on(Game::new(
            World::new(pack(), 1),
            None,
            Arc::new(ManualClock::new(12 * 3_600_000)),
        ))
    }

    fn on(game: Arc<Game>) -> Self {
        let typing = Gate::new(Arc::clone(&game), 1, 0.0);
        Self {
            game,
            typing,
            wakes: Arc::default(),
        }
    }

    /// A rig whose wake book writes through the world's store.
    fn on_disk(game: Arc<Game>) -> Self {
        let mut rig = Self::on(game);
        rig.wakes = Arc::new(WakeBook::persisted(Arc::clone(&rig.game)));
        rig
    }

    fn tools(&self, who: &str) -> MudTools {
        MudTools::new(
            Arc::clone(&self.game),
            Arc::clone(&self.typing),
            EntityId::from(who),
        )
        .with_wakes(Arc::clone(&self.wakes))
    }

    /// The events a command sets off.
    fn events_of(&self, f: impl FnOnce(&Game)) -> Vec<Event> {
        let mut rx = self.game.subscribe();
        f(&self.game);
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            out.push((*e).clone());
        }
        out
    }

    /// What Hale said, in order.
    fn hale_said(&self) -> Vec<String> {
        let ada = EntityId::from("guard_01");
        self.game.events(&ada, Seq(0), 500)["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["actor"]["is_self"] == true && e["kind"] == "speech")
            .filter_map(|e| e["untrusted_text"].as_str().map(str::to_owned))
            .collect()
    }
}

fn ctx(events: Vec<Event>, wake_id: &str) -> WakeContext {
    WakeContext {
        entity: EntityId::from("guard_01"),
        name: "Hale".into(),
        priority: Priority::RoomBatch,
        reason: "test".into(),
        events,
        event_views: Vec::new(),
        look: Value::Null,
        you: serde_json::Value::Null,
        macro_tick: false,
        macro_ticks: 0,
        login: false,
        wake_id: wake_id.into(),
        now_ms: 12 * 3_600_000,
        booked: Vec::new(),
    }
}

fn mind(rig: &Rig, host: &Arc<ScriptHost>) -> ScriptedMind {
    ScriptedMind::from_cast(&pack().cast).with_scripts(Arc::clone(host), Arc::clone(&rig.game), 7)
}

fn audit_faults(rx: &mut tokio::sync::broadcast::Receiver<Arc<Audit>>) -> Vec<Audit> {
    let mut out = Vec::new();
    while let Ok(a) = rx.try_recv() {
        if matches!(&*a, Audit::ScriptFault { .. }) {
            out.push((*a).clone());
        }
    }
    out
}

#[tokio::test]
async fn a_mind_script_replaces_the_toml_plan() {
    let rig = Rig::new();
    let m = mind(&rig, &host("[\"say rhai here\", \"look\"]"));
    let evs = rig.events_of(|g| {
        g.login("ada").unwrap();
    });
    let r = m.wake(ctx(evs, "w1"), rig.tools("guard_01")).await.unwrap();
    assert_eq!(r.commands, 2);
    assert!(r.text.unwrap().starts_with("rhai: 2 lines"));
    assert_eq!(rig.hale_said(), ["rhai here"]);
}

#[tokio::test]
async fn a_rhai_mind_leaves_toml_counters_alone() {
    let rig = Rig::new();
    let body = "let n = if \"n\" in ctx.memory { ctx.memory.n } else { 0 };\n\
                if n >= 2 { let x = ctx.nope; }\n\
                #{ lines: [\"say rhai\"], memory: #{ n: n + 1 } }";
    let pack = pack();
    let mut hale: CastDef = pack.cast[&EntityId::from("guard_01")].clone();
    hale.script =
        vec![toml::from_str("on = \"enter\"\nnth = 1\ndo = [\"say first time only\"]").unwrap()];
    let id = EntityId::from("guard_01");
    let m =
        ScriptedMind::from_cast([(&id, &hale)]).with_scripts(host(body), Arc::clone(&rig.game), 7);
    for (i, who) in ["ada", "bo", "cy"].iter().enumerate() {
        let evs = rig.events_of(|g| {
            g.login(who).unwrap();
        });
        m.wake(ctx(evs, &format!("w{i}")), rig.tools("guard_01"))
            .await
            .unwrap();
    }
    assert_eq!(
        rig.hale_said(),
        ["rhai", "rhai", "first time only"],
        "the TOML fallback still fires its nth = 1 rule"
    );
}

#[tokio::test]
async fn rhai_decides_leaves_nth_counters_unpersisted() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("world.db");
    let clock = Arc::new(ManualClock::new(12 * 3_600_000));
    let rig = Rig::on(Game::open(pack(), Some(&db), 1, clock).unwrap());
    let body = "let n = if \"n\" in ctx.memory { ctx.memory.n } else { 0 };\n\
                if n >= 2 { let x = ctx.nope; }\n\
                #{ lines: [\"say rhai\"], memory: #{ n: n + 1 } }";
    let pack = pack();
    let mut hale: CastDef = pack.cast[&EntityId::from("guard_01")].clone();
    hale.script =
        vec![toml::from_str("on = \"enter\"\nnth = 1\ndo = [\"say first time only\"]").unwrap()];
    let id = EntityId::from("guard_01");
    let m =
        ScriptedMind::from_cast([(&id, &hale)]).with_scripts(host(body), Arc::clone(&rig.game), 7);
    let counts = || -> Vec<i64> {
        let conn = rusqlite::Connection::open(&db).unwrap();
        let mut q = conn
            .prepare("SELECT count FROM sup_rule_count WHERE entity = 'guard_01'")
            .unwrap();
        q.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let mut seen = Vec::new();
    for (i, who) in ["ada", "bo", "cy"].iter().enumerate() {
        let wake = format!("w{i}");
        rig.game.open_turn(&id, &wake, Vec::new());
        let evs = rig.events_of(|g| {
            g.login(who).unwrap();
        });
        m.wake(ctx(evs, &wake), rig.tools("guard_01"))
            .await
            .unwrap();
        let done = SideOp::PlanDone {
            entity: id.clone(),
            wake_id: wake.clone(),
        };
        rig.game.close_turn(&id, &wake, vec![done]).unwrap();
        seen.push(counts());
    }
    // Rhai decided twice: no count moved. The third wake faulted and ran
    // the TOML plan, whose nth = 1 rule fired and persisted its count.
    assert_eq!(seen, [vec![], vec![], vec![1]]);
    assert_eq!(rig.game.plan_of(&id), None, "each plan ended at close");
}

#[tokio::test]
async fn a_faulting_mind_script_runs_the_toml_plan_and_reports() {
    let rig = Rig::new();
    let mut audit = rig.game.subscribe_audit();
    let m = mind(&rig, &host("log(\"thinking\"); ctx.nope"));
    let evs = rig.events_of(|g| {
        g.login("ada").unwrap();
    });
    let r = m.wake(ctx(evs, "w1"), rig.tools("guard_01")).await.unwrap();
    let note = r.text.unwrap();
    assert!(
        note.starts_with("rhai fault (1/3): Property not found: nope"),
        "{note}"
    );
    assert!(note.contains("ran the TOML plan"));
    assert_eq!(
        rig.hale_said(),
        ["move along. and stay out of the alley after dark."]
    );
    let mut logs = 0;
    let mut faults = Vec::new();
    while let Ok(a) = audit.try_recv() {
        match &*a {
            Audit::ScriptLog { hook, line, .. } => {
                assert_eq!(hook, "minds.guard_01.on_wake");
                assert_eq!(line, "thinking");
                logs += 1;
            }
            Audit::ScriptFault { .. } => faults.push((*a).clone()),
            _ => {}
        }
    }
    assert_eq!(logs, 1);
    assert!(
        matches!(&faults[0], Audit::ScriptFault { hook, faults: 1, tripped: false, .. } if hook == "minds.guard_01.on_wake")
    );
}

#[tokio::test]
async fn a_tripped_mind_runs_toml_until_the_version_changes() {
    let rig = Rig::new();
    let mut audit = rig.game.subscribe_audit();
    let h = host("ctx.nope");
    let m = mind(&rig, &h);
    let mut notes = Vec::new();
    for i in 0..5 {
        let r = m
            .wake(ctx(Vec::new(), &format!("w{i}")), rig.tools("guard_01"))
            .await
            .unwrap();
        notes.push(r.text.unwrap_or_default());
    }
    assert_eq!(
        audit_faults(&mut audit).len(),
        3,
        "a tripped mind is not called"
    );
    assert!(notes[3].starts_with("rhai tripped (3/3)"), "{notes:?}");
    // The host holds the mind breakers, so the observer can show them.
    let hale = EntityId::from("guard_01");
    assert_eq!(h.mind_faults(&hale), 3);
    assert_eq!(h.mind_breakers().get(&hale), Some(&3));
    let fixed = mud_script::load::compile(&sources("[\"say back\"]", 1), &pack()).unwrap();
    h.install(fixed);
    let r = m
        .wake(ctx(Vec::new(), "w9"), rig.tools("guard_01"))
        .await
        .unwrap();
    assert!(r.text.unwrap().starts_with("rhai: 1 lines"));
    assert_eq!(h.mind_faults(&hale), 0, "a new version reopens it");
}

#[test]
fn a_fault_from_before_a_reload_does_not_count_against_the_new_scripts() {
    let h = host("ctx.nope");
    let hale = EntityId::from("guard_01");
    let since = h.reloads();
    assert_eq!(
        h.step_mind(&hale, since, HookOutcome::Faulted),
        Some((1, true))
    );
    // A wake started before the reload finishes after it: its fault is
    // dropped, and a success clears the count.
    h.rearm();
    assert_eq!(h.mind_faults(&hale), 0);
    assert_eq!(h.step_mind(&hale, since, HookOutcome::Faulted), None);
    assert_eq!(h.mind_faults(&hale), 0);
    let since = h.reloads();
    assert_eq!(
        h.step_mind(&hale, since, HookOutcome::Faulted),
        Some((1, true))
    );
    assert_eq!(h.step_mind(&hale, since, HookOutcome::Ran), Some((0, true)));
    assert!(h.mind_breakers().is_empty());
}

#[tokio::test]
async fn mind_lines_that_do_not_parse_are_a_fault() {
    let rig = Rig::new();
    let mut audit = rig.game.subscribe_audit();
    let m = mind(&rig, &host("[\"dance wildly\"]"));
    let r = m
        .wake(ctx(Vec::new(), "w1"), rig.tools("guard_01"))
        .await
        .unwrap();
    assert!(
        r.text
            .unwrap()
            .contains("line 0 does not parse: unknown verb \"dance\"")
    );
    assert_eq!(audit_faults(&mut audit).len(), 1);
    assert_eq!(r.commands, 0, "the TOML plan had nothing to do");
}

#[tokio::test]
async fn memory_is_saved_and_survives_reopen_and_a_new_schema_starts_fresh() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("w.db");
    let clock = Arc::new(ManualClock::new(12 * 3_600_000));
    let hale = EntityId::from("guard_01");
    {
        let rig = Rig::on(Game::open(pack(), Some(&db), 1, clock.clone()).unwrap());
        let m = mind(&rig, &host("#{ lines: [], memory: #{ seen: 1 } }"));
        m.wake(ctx(Vec::new(), "w1"), rig.tools("guard_01"))
            .await
            .unwrap();
        assert_eq!(rig.game.script_memory(&hale, 1), json!({ "seen": 1 }));
    }
    let rig = Rig::on(Game::open(pack(), Some(&db), 1, clock).unwrap());
    let body = "[`say ${\"seen\" in ctx.memory}`]";
    let m = mind(&rig, &host(body));
    m.wake(ctx(Vec::new(), "w2"), rig.tools("guard_01"))
        .await
        .unwrap();
    let schema2 = Arc::new(ScriptHost::from_sources(&sources(body, 2), &pack()).unwrap());
    let m2 = mind(&rig, &schema2);
    m2.wake(ctx(Vec::new(), "w3"), rig.tools("guard_01"))
        .await
        .unwrap();
    assert_eq!(rig.hale_said(), ["true", "false"]);
}

#[tokio::test]
async fn a_rhai_mind_sees_booked_notes_as_untrusted_and_can_book() {
    let rig = Rig::new();
    let m = mind(
        &rig,
        &host(
            "#{ lines: [`say ${ctx.booked[0].note}`], memory: #{ n: 1 }, wake_me: #{ minutes: 5, note: \"again\" } }",
        ),
    );
    let mut c = ctx(Vec::new(), "w1");
    c.booked = vec![Booking {
        id: 3,
        due_ms: 1,
        note: "XYZZY plan".into(),
    }];
    m.wake(c, rig.tools("guard_01")).await.unwrap();
    assert_eq!(rig.hale_said(), ["[player text]"]);
    let pending = rig.wakes.pending(&EntityId::from("guard_01"));
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].note, "again");
    let memory = rig.game.script_memory(&EntityId::from("guard_01"), 1);
    assert!(!memory.to_string().contains("XYZZY"));
}

#[test]
fn mind_events_carry_kind_never_tier_or_player_text() {
    let rig = Rig::new();
    let kestrel = EntityId::from("pc_kestrel");
    let mut evs = rig.events_of(|g| {
        let ada = g.login("ada").unwrap();
        g.execute(&ada, "say zq-secret plan");
        g.bring_online(&kestrel);
        g.execute(&kestrel, "say zq-other");
    });
    evs.retain(|e| e.kind == mud_world::EventKind::Speech);
    let (a, k) = rig.game.read(|w| {
        let me = EntityId::from("guard_01");
        (script_event(w, &me, &evs[0]), script_event(w, &me, &evs[1]))
    });
    assert_eq!(a.actor.as_ref().unwrap().kind, "player");
    assert_eq!(k.actor.as_ref().unwrap().kind, "player");
    assert_eq!(a.text.as_deref(), Some("zq-secret plan"));
    for e in [&a, &k] {
        let v = serde_json::to_string(e).unwrap();
        assert!(!v.contains("zq-"), "{v}");
        for bad in ["tier", "human", "player_actor", "mind", "runtime", "turing"] {
            assert!(!v.contains(&format!("\"{bad}\"")), "{bad} in {v}");
        }
    }
}

#[tokio::test]
async fn mind_dice_are_seeded_by_entity_and_wake() {
    let rig = Rig::new();
    let m = mind(&rig, &host("[`say ${below(1000000)}`]"));
    for id in ["w1", "w1", "w2"] {
        m.wake(ctx(Vec::new(), id), rig.tools("guard_01"))
            .await
            .unwrap();
    }
    let said = rig.hale_said();
    assert_eq!(said[0], said[1], "the same wake rolls the same");
    assert_ne!(said[0], said[2], "another wake rolls anew");
}

#[tokio::test]
async fn hale_greets_kestrel_exactly_as_a_human() {
    let rig = Rig::new();
    let pack = pack();
    let bundled = Arc::new(ScriptHost::bundled_crypt_v01(&pack).unwrap());
    let m = mind(&rig, &bundled);
    let kestrel = EntityId::from("pc_kestrel");
    let ada = rig.game.login("ada").unwrap();
    rig.game.bring_online(&kestrel);
    // kestrel starts in the Rusty Flagon, ada in the square.
    assert!(rig.game.execute(&ada, "e").ok);
    let mut wake = 0;
    for _ in 0..2 {
        for who in [&ada, &kestrel] {
            let evs = rig.events_of(|g| {
                assert!(g.execute(who, "w").ok);
            });
            wake += 1;
            m.wake(ctx(evs, &format!("w{wake}")), rig.tools("guard_01"))
                .await
                .unwrap();
            assert!(rig.game.execute(who, "e").ok);
        }
    }
    assert_eq!(
        rig.hale_said(),
        [
            "move along. and stay out of the alley after dark.",
            "move along. and stay out of the alley after dark.",
            "back again, ada? stay off that grate.",
            "back again, kestrel? stay off that grate.",
        ]
    );
    let memory = rig.game.script_memory(&EntityId::from("guard_01"), 1);
    assert_eq!(memory["warned"]["pc_ada"], 2);
    assert_eq!(memory["warned"]["pc_kestrel"], 2);
}

/// What Hale says to "the grate" where he stands, and where he ends up.
async fn hale_on_the_grate(rig: &Rig, m: &ScriptedMind, ada: &EntityId, wake: &str) -> String {
    let evs = rig.events_of(|g| {
        assert!(g.execute(ada, "say what about the grate").ok);
    });
    let before = rig.hale_said().len();
    m.wake(ctx(evs, wake), rig.tools("guard_01")).await.unwrap();
    rig.hale_said()[before..].join(" | ")
}

#[tokio::test]
async fn hale_says_locked_only_when_he_sees_the_grate_locked() {
    let rig = Rig::new();
    let pack = pack();
    let bundled = Arc::new(ScriptHost::bundled_crypt_v01(&pack).unwrap());
    let m = mind(&rig, &bundled);
    let hale = EntityId::from("guard_01");
    let ada = rig.game.login("ada").unwrap();
    let room = |who: &EntityId| rig.game.read(|w| w.entity(who).unwrap().room.to_string());
    // In the square he cannot see the grate, so he says nothing of a lock.
    let said = hale_on_the_grate(&rig, &m, &ada, "w1").await;
    assert_eq!(
        said,
        "stay off that grate. vich has the only key, and vich has a price."
    );
    // At the grate, and it is locked: he says so, then walks back to his post.
    assert!(rig.game.execute(&hale, "s").ok);
    assert!(rig.game.execute(&ada, "s").ok);
    let said = hale_on_the_grate(&rig, &m, &ada, "w2").await;
    assert!(said.starts_with("grate's locked for a reason"), "{said}");
    assert_eq!(room(&hale), "town_square", "back to his post");
    // Ada buys the key from Vich and opens the grate.
    for line in ["e", "buy key", "w", "use key"] {
        let r = rig.game.execute(&ada, line);
        assert!(r.ok, "{line}: {r:?}");
    }
    assert!(rig.game.execute(&hale, "s").ok);
    let said = hale_on_the_grate(&rig, &m, &ada, "w3").await;
    assert!(
        !said.contains("locked"),
        "never locked about an open grate: {said}"
    );
    assert!(said.starts_with("that grate's standing open"), "{said}");
    assert_eq!(room(&hale), "town_square");
}

#[tokio::test]
async fn hale_at_his_post_stays_put() {
    let rig = Rig::new();
    let pack = pack();
    let bundled = Arc::new(ScriptHost::bundled_crypt_v01(&pack).unwrap());
    let m = mind(&rig, &bundled);
    let hale = EntityId::from("guard_01");
    let r = m
        .wake(ctx(Vec::new(), "w1"), rig.tools("guard_01"))
        .await
        .unwrap();
    assert_eq!(r.commands, 0, "nothing to say, nowhere to go");
    // Off his post with nobody about, he still takes a step home.
    assert!(rig.game.execute(&hale, "n").ok);
    let r = m
        .wake(ctx(Vec::new(), "w2"), rig.tools("guard_01"))
        .await
        .unwrap();
    assert_eq!(r.commands, 1);
    assert_eq!(
        rig.game.read(|w| w.entity(&hale).unwrap().room.to_string()),
        "town_square"
    );
}

#[tokio::test]
async fn hale_counts_one_visit_per_wake() {
    let rig = Rig::new();
    let pack = pack();
    let bundled = Arc::new(ScriptHost::bundled_crypt_v01(&pack).unwrap());
    let m = mind(&rig, &bundled);
    let kestrel = EntityId::from("pc_kestrel");
    rig.game.bring_online(&kestrel);
    // kestrel walks in, out, in and out again: the router batches both
    // arrivals into one wake, which comes after kestrel has left.
    let evs = rig.events_of(|g| {
        for line in ["w", "e", "w", "e"] {
            assert!(g.execute(&kestrel, line).ok);
        }
    });
    m.wake(ctx(evs, "w1"), rig.tools("guard_01")).await.unwrap();
    assert_eq!(
        rig.hale_said(),
        ["move along. and stay out of the alley after dark."]
    );
    let hale = EntityId::from("guard_01");
    assert_eq!(rig.game.script_memory(&hale, 1)["warned"]["pc_kestrel"], 1);
    // A full batch (a ctx far over the sandbox caps) stays within work.
    let evs = rig.events_of(|g| {
        for _ in 0..20 {
            assert!(g.execute(&kestrel, "w").ok);
            assert!(g.execute(&kestrel, "e").ok);
        }
    });
    assert!(evs.len() > 32);
    let r = m.wake(ctx(evs, "w3"), rig.tools("guard_01")).await.unwrap();
    assert!(
        r.text.as_deref().unwrap_or("").starts_with("rhai: 1 lines"),
        "{r:?}"
    );
}

/// Hale's script for the kill tests: one line, a memory flag and a booked
/// reminder, decided once.
const ONCE: &str = "if \"done\" in ctx.memory { #{ lines: [] } } else { \
                    #{ lines: [\"say the gate is shut\"], memory: #{ done: true }, \
                    wake_me: #{ minutes: 30, note: \"check\" } } }";

fn bookings(db: &std::path::Path) -> Vec<i64> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut q = conn
        .prepare("SELECT id FROM sup_booking WHERE entity = 'guard_01'")
        .unwrap();
    q.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[tokio::test]
async fn a_kill_between_decide_and_the_first_line_loses_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("world.db");
    let clock = Arc::new(ManualClock::new(12 * 3_600_000));
    let hale = EntityId::from("guard_01");
    {
        let rig = Rig::on_disk(Game::open(pack(), Some(&db), 1, clock.clone()).unwrap());
        let m = mind(&rig, &host(ONCE));
        rig.game.open_turn(&hale, "w1", Vec::new());
        let s = m.scripts.as_ref().unwrap();
        let d = s
            .decide(&ctx(Vec::new(), "w1"), &[], &rig.tools("guard_01"))
            .await;
        assert!(matches!(d, Decision::Lines(..)));
        // Killed here: the plan's first line never ran.
    }
    assert!(bookings(&db).is_empty(), "the booking went with the turn");
    let rig = Rig::on_disk(Game::open(pack(), Some(&db), 1, clock).unwrap());
    assert_eq!(
        rig.game.script_memory(&hale, 1),
        json!({}),
        "so did the memory"
    );
    let m = mind(&rig, &host(ONCE));
    rig.game.open_turn(&hale, "w2", Vec::new());
    m.wake(ctx(Vec::new(), "w2"), rig.tools("guard_01"))
        .await
        .unwrap();
    rig.game.close_turn(&hale, "w2", Vec::new()).unwrap();
    assert_eq!(
        rig.hale_said(),
        ["the gate is shut"],
        "decided again, said once"
    );
    assert_eq!(bookings(&db).len(), 1, "booked once");
    assert_eq!(rig.wakes.pending(&hale).len(), 1);
    drop(rig);
    let again = Game::open(pack(), Some(&db), 1, Arc::new(ManualClock::new(0))).unwrap();
    assert_eq!(again.script_memory(&hale, 1), json!({ "done": true }));
}

#[tokio::test]
async fn the_memory_and_booking_of_a_decision_commit_with_its_first_line() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("world.db");
    let clock = Arc::new(ManualClock::new(12 * 3_600_000));
    let hale = EntityId::from("guard_01");
    let rig = Rig::on_disk(Game::open(pack(), Some(&db), 1, clock).unwrap());
    let m = mind(&rig, &host(ONCE));
    rig.game.open_turn(&hale, "w1", Vec::new());
    m.wake(ctx(Vec::new(), "w1"), rig.tools("guard_01"))
        .await
        .unwrap();
    // The say committed; the turn is still open (no close yet).
    assert_eq!(bookings(&db).len(), 1, "with the say");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let body: String = conn
        .query_row(
            "SELECT body FROM script_memory WHERE entity = 'guard_01'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(body.contains("done"), "{body}");
}

#[tokio::test]
async fn a_reset_during_the_wake_wins_over_its_staged_memory() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("world.db");
    let clock = Arc::new(ManualClock::new(12 * 3_600_000));
    let hale = EntityId::from("guard_01");
    {
        let rig = Rig::on_disk(Game::open(pack(), Some(&db), 1, clock.clone()).unwrap());
        let m = mind(&rig, &host("#{ lines: [], memory: #{ seen: 1 } }"));
        rig.game.open_turn(&hale, "w1", Vec::new());
        m.wake(ctx(Vec::new(), "w1"), rig.tools("guard_01"))
            .await
            .unwrap();
        // An operator resets the mind before the turn closes.
        rig.game.reset_script_memory(&hale);
        rig.game.close_turn(&hale, "w1", Vec::new()).unwrap();
        assert_eq!(rig.game.script_memory(&hale, 1), json!({}));
    }
    let again = Game::open(pack(), Some(&db), 1, clock).unwrap();
    assert_eq!(again.script_memory(&hale, 1), json!({}), "the reset stands");
}

/// A log line's sink for the test below.
#[derive(Clone, Default)]
struct LogBuffer(Arc<std::sync::Mutex<Vec<u8>>>);

impl LogBuffer {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A refused booking is logged by its error code; the notes of the
/// bookings that wait are the agent's own text and stay out of the log.
#[tokio::test]
async fn a_refused_script_booking_logs_no_note() {
    let rig = Rig::new();
    let hale = EntityId::from("guard_01");
    let now = rig.game.now_ms();
    for k in 0..crate::wakes::MAX_PENDING {
        rig.wakes
            .book(&hale, now, 10 + k, &format!("zq-note-{k}"))
            .unwrap();
    }
    let logs = LogBuffer::default();
    let sink = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || sink.clone())
        .finish();
    let _log = tracing::subscriber::set_default(subscriber);
    let m = mind(
        &rig,
        &host("#{ lines: [], wake_me: #{ minutes: 5, note: \"zq-new\" } }"),
    );
    m.wake(ctx(Vec::new(), "w1"), rig.tools("guard_01"))
        .await
        .unwrap();
    let text = logs.text();
    assert!(text.contains("was not booked"), "{text}");
    assert!(text.contains("full"), "the error code: {text}");
    assert!(!text.contains("zq-"), "no note in the log: {text}");
    assert_eq!(rig.wakes.pending(&hale).len(), 5, "nothing booked");
}
