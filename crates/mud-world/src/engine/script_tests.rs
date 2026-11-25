//! World hooks in the engine, with a plain-Rust `FakeHooks` (no Rhai).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use mud_rules::{DamageDice, DiceRng};
use serde_json::Value;

use super::*;
use crate::hooks::{
    FLAG_CAP, FlagValue, HookId, HookInput, HookLimits, HookReply, MARK_CAP, ScriptEffect,
    VerbInfo, WorldDice, WorldHooks,
};
use crate::state::Holder;

mod rewards;

type Answer = dyn Fn(&HookId, &HookInput, WorldDice) -> HookReply + Send + Sync;

struct FakeHooks {
    version: String,
    hooks: Vec<HookId>,
    verbs: Vec<VerbInfo>,
    answer: Box<Answer>,
    calls: AtomicUsize,
    inputs: Mutex<Vec<HookInput>>,
}

impl std::fmt::Debug for FakeHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FakeHooks")
    }
}

impl WorldHooks for FakeHooks {
    fn version(&self) -> String {
        self.version.clone()
    }
    fn has(&self, hook: &HookId) -> bool {
        self.hooks.contains(hook)
    }
    fn verb(&self, name: &str) -> Option<VerbInfo> {
        self.verbs.iter().find(|v| v.name == name).cloned()
    }
    fn verbs(&self) -> Vec<VerbInfo> {
        self.verbs.clone()
    }
    fn limits(&self) -> HookLimits {
        HookLimits::default()
    }
    fn call(&self, hook: &HookId, input: &HookInput, dice: WorldDice) -> HookReply {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inputs.lock().unwrap().push(input.clone());
        (self.answer)(hook, input, dice)
    }
}

impl FakeHooks {
    fn new(
        hooks: Vec<HookId>,
        answer: impl Fn(&HookId, &HookInput, WorldDice) -> HookReply + Send + Sync + 'static,
    ) -> Self {
        let verbs = hooks
            .iter()
            .filter_map(|h| match h {
                HookId::Verb(v) => Some(VerbInfo {
                    name: v.clone(),
                    usage: v.clone(),
                    rooms: if v == "pry" || v == "hurtme" {
                        vec![RoomId::from("dark_alley"), RoomId::from("cellar_landing")]
                    } else {
                        Vec::new()
                    },
                }),
                _ => None,
            })
            .collect();
        Self {
            version: "fake-1".into(),
            hooks,
            verbs,
            answer: Box::new(answer),
            calls: AtomicUsize::new(0),
            inputs: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

fn effects(fx: Vec<ScriptEffect>) -> impl Fn(&HookId, &HookInput, WorldDice) -> HookReply {
    move |_, _, dice| HookReply {
        outcome: Ok((fx.clone(), dice)),
        log: vec!["logged".into()],
    }
}

fn fault(_: &HookId, _: &HookInput, _: WorldDice) -> HookReply {
    HookReply {
        outcome: Err("boom".into()),
        log: Vec::new(),
    }
}

fn verb(v: &str) -> HookId {
    HookId::Verb(v.into())
}

fn id(s: &str) -> EntityId {
    EntityId::from(s)
}

fn world_with(h: &Arc<FakeHooks>) -> World {
    let mut w = World::new(Arc::new(WorldPack::crypt_v01().unwrap()), 42);
    w.set_hooks(
        Some(Arc::clone(h) as Arc<dyn WorldHooks>),
        1,
        HooksChange::Start,
    );
    w
}

fn in_alley(w: &mut World) -> EntityId {
    let ada = w.login("ada", 1).unwrap();
    let r = w.execute(&ada, "s", 2);
    assert!(r.ok, "{r:?}");
    ada
}

fn run(w: &mut World, who: &EntityId, line: &str) -> CommandResult {
    w.execute(who, line, 1_000)
}

fn faults(w: &mut World) -> Vec<Audit> {
    w.drain_audit()
        .into_iter()
        .filter(|a| matches!(a, Audit::ScriptFault { .. }))
        .collect()
}

// ------------------------------------------------------------ custom verbs

#[test]
fn a_custom_verb_runs_its_hook_and_applies_effects_in_order() {
    let h = Arc::new(FakeHooks::new(
        vec![verb("pry")],
        effects(vec![
            ScriptEffect::Msg("You pry at the grate.".into()),
            ScriptEffect::Msg("It gives.".into()),
            ScriptEffect::Unlock(Direction::D),
        ]),
    ));
    let mut w = world_with(&h);
    let ada = in_alley(&mut w);
    let r = run(&mut w, &ada, "pry hard");
    assert!(r.ok, "{r:?}");
    assert_eq!(r.verb.as_deref(), Some("pry"));
    assert_eq!(r.narrative, "You pry at the grate.\nIt gives.");
    assert_eq!(r.resolution["script"], "verbs.pry");
    assert!(r.state_changes.contains(&StateChange::Unlocked {
        room: RoomId::from("dark_alley"),
        dir: Direction::D
    }));
    assert!(
        w.state.rooms[&RoomId::from("dark_alley")]
            .unlocked
            .contains(&Direction::D)
    );
    assert!(
        w.state.rooms[&RoomId::from("cellar_landing")]
            .unlocked
            .contains(&Direction::U)
    );
    assert_eq!(h.inputs.lock().unwrap()[0].text.as_deref(), Some("hard"));
    // Lock closes both sides again.
    let h2 = Arc::new(FakeHooks::new(
        vec![verb("pry")],
        effects(vec![ScriptEffect::Lock(Direction::D)]),
    ));
    w.set_hooks(Some(h2 as Arc<dyn WorldHooks>), 3, HooksChange::Reload);
    let r = run(&mut w, &ada, "pry");
    assert_eq!(r.narrative, "Nothing happens.");
    assert!(r.state_changes.contains(&StateChange::Locked {
        room: RoomId::from("dark_alley"),
        dir: Direction::D
    }));
    assert!(
        w.state.rooms[&RoomId::from("cellar_landing")]
            .unlocked
            .is_empty()
    );
}

#[test]
fn a_custom_verb_outside_its_rooms_is_not_here() {
    let h = Arc::new(FakeHooks::new(vec![verb("pry")], effects(vec![])));
    let mut w = world_with(&h);
    let ada = w.login("ada", 1).unwrap();
    let r = run(&mut w, &ada, "pry");
    assert_eq!(r.error_code, Some(ErrorCode::NotHere));
    assert_eq!(r.narrative, "you cannot pry here");
    assert_eq!(r.verb.as_deref(), Some("pry"));
    assert_eq!(h.calls(), 0);
}

#[test]
fn custom_verb_refusals_name_the_verb() {
    let h = Arc::new(FakeHooks::new(vec![verb("pry")], fault));
    let mut w = world_with(&h);
    let ada = in_alley(&mut w);
    let r = run(&mut w, &ada, "pry");
    assert_eq!(r.error_code, Some(ErrorCode::ScriptFailed));
    assert_eq!(r.verb.as_deref(), Some("pry"));
    assert_eq!(r.narrative, "Nothing happens.");
    let cmd = Command::Custom {
        name: "pry".into(),
        args: "x".repeat(501),
    };
    let r = w.execute_command(&ada, &cmd, 5);
    assert_eq!(r.verb.as_deref(), Some("pry"));
}

#[test]
fn execute_command_revalidates_custom_commands() {
    let h = Arc::new(FakeHooks::new(vec![verb("pry")], effects(vec![])));
    let mut w = world_with(&h);
    let ada = in_alley(&mut w);
    let custom = |name: &str, args: &str| Command::Custom {
        name: name.into(),
        args: args.into(),
    };
    let r = w.execute_command(&ada, &custom("dance", ""), 5);
    assert_eq!(r.error_code, Some(ErrorCode::UnknownVerb));
    let r = w.execute_command(&ada, &custom("pry", &"a".repeat(501)), 5);
    assert_eq!(r.error_code, Some(ErrorCode::TooLong));
    let r = w.execute_command(&ada, &custom("pry", "a\u{202e}b"), 5);
    assert_eq!(r.error_code, Some(ErrorCode::BadArgs));
    assert_eq!(h.calls(), 0);
    // Without scripts, no custom verb exists at all.
    let mut plain = World::new(Arc::new(WorldPack::crypt_v01().unwrap()), 42);
    let bo = plain.login("bo", 1).unwrap();
    assert_eq!(
        plain.execute(&bo, "pry", 2).error_code,
        Some(ErrorCode::UnknownVerb)
    );
    let r = plain.execute_command(&bo, &custom("pry", ""), 2);
    assert_eq!(r.error_code, Some(ErrorCode::UnknownVerb));
}

#[test]
fn refuse_fails_the_command_with_its_text_only() {
    let h = Arc::new(FakeHooks::new(
        vec![verb("pry")],
        effects(vec![ScriptEffect::Refuse(
            "The warden will not let you.".into(),
        )]),
    ));
    let mut w = world_with(&h);
    let ada = in_alley(&mut w);
    let r = run(&mut w, &ada, "pry");
    assert_eq!(r.error_code, Some(ErrorCode::Refused));
    assert_eq!(r.narrative, "The warden will not let you.");
    assert!(w.state.hook_faults.is_empty(), "a refusal is not a fault");
}

#[test]
fn refuse_with_other_effects_is_a_fault() {
    let h = Arc::new(FakeHooks::new(
        vec![verb("pry")],
        effects(vec![
            ScriptEffect::Refuse("no".into()),
            ScriptEffect::SetFlag("a".into(), FlagValue::Bool(true)),
        ]),
    ));
    let mut w = world_with(&h);
    let ada = in_alley(&mut w);
    w.drain_audit();
    let r = run(&mut w, &ada, "pry");
    assert_eq!(r.error_code, Some(ErrorCode::ScriptFailed));
    assert!(w.state.flags.is_empty());
    assert_eq!(w.state.hook_faults["verbs.pry"], 1);
    let f = faults(&mut w);
    assert!(
        matches!(&f[0], Audit::ScriptFault { message, faults: 1, tripped: false, .. } if message == "refuse() must be the only effect"),
        "{f:?}"
    );
}

// ------------------------------------------------------------ faults and dice

#[test]
fn an_invalid_effect_discards_every_effect() {
    for bad in [
        ScriptEffect::Msg("   ".into()),
        ScriptEffect::Msg("x".repeat(501)),
        ScriptEffect::Narrate("line\nbreak".into()),
        ScriptEffect::Unlock(Direction::N),
        ScriptEffect::SetFlag("Bad Name".into(), FlagValue::Bool(true)),
        ScriptEffect::Mark("m".into(), FlagValue::Text("x".repeat(65))),
        ScriptEffect::ClearFlag(String::new()),
    ] {
        let h = Arc::new(FakeHooks::new(
            vec![verb("pry")],
            effects(vec![
                ScriptEffect::SetFlag("good".into(), FlagValue::Int(1)),
                bad.clone(),
            ]),
        ));
        let mut w = world_with(&h);
        let ada = in_alley(&mut w);
        let r = run(&mut w, &ada, "pry");
        assert_eq!(r.error_code, Some(ErrorCode::ScriptFailed), "{bad:?}");
        assert!(w.state.flags.is_empty(), "{bad:?}");
    }
}

#[test]
fn two_body_effects_are_inadmissible() {
    let h = Arc::new(FakeHooks::new(
        vec![verb("pry")],
        effects(vec![
            ScriptEffect::Heal(DamageDice::new(1, 0)),
            ScriptEffect::Heal(DamageDice::new(1, 0)),
        ]),
    ));
    let mut w = world_with(&h);
    let ada = in_alley(&mut w);
    assert_eq!(
        run(&mut w, &ada, "pry").error_code,
        Some(ErrorCode::ScriptFailed)
    );
}

#[test]
fn event_hook_faults_never_fail_the_command() {
    let h = Arc::new(FakeHooks::new(
        vec![HookId::Speech(RoomId::from("town_square"))],
        fault,
    ));
    let mut w = world_with(&h);
    let ada = w.login("ada", 1).unwrap();
    let r = run(&mut w, &ada, "say hi");
    assert!(r.ok, "{r:?}");
    assert_eq!(r.narrative, "You say, \"hi\"");
    assert_eq!(w.state.hook_faults["rooms.town_square.on_speech"], 1);
    // A refuse in an event hook is a fault too, and still not a failure.
    let h = Arc::new(FakeHooks::new(
        vec![HookId::Speech(RoomId::from("town_square"))],
        effects(vec![ScriptEffect::Refuse("no".into())]),
    ));
    w.set_hooks(Some(h as Arc<dyn WorldHooks>), 2, HooksChange::Reload);
    w.drain_audit();
    assert!(run(&mut w, &ada, "say hi").ok);
    let f = faults(&mut w);
    assert!(
        matches!(&f[0], Audit::ScriptFault { message, .. } if message.contains("only for verb"))
    );
}

#[test]
fn three_faults_trip_a_hook_until_the_scripts_change() {
    let h = Arc::new(FakeHooks::new(vec![verb("pry")], fault));
    let mut w = world_with(&h);
    let ada = in_alley(&mut w);
    w.drain_audit();
    for _ in 0..5 {
        assert_eq!(
            run(&mut w, &ada, "pry").error_code,
            Some(ErrorCode::ScriptFailed)
        );
    }
    assert_eq!(h.calls(), 3, "a tripped hook is not called");
    let f = faults(&mut w);
    assert_eq!(f.len(), 3);
    assert!(matches!(
        &f[2],
        Audit::ScriptFault {
            faults: 3,
            tripped: true,
            ..
        }
    ));
    assert_eq!(w.hook_status()[0].key, "verbs.pry");
    assert!(w.hook_status()[0].tripped);
    // The same version at start keeps the breaker; a new version opens it.
    w.set_hooks(
        Some(Arc::clone(&h) as Arc<dyn WorldHooks>),
        9,
        HooksChange::Start,
    );
    run(&mut w, &ada, "pry");
    assert_eq!(h.calls(), 3);
    let mut h2 = FakeHooks::new(vec![verb("pry")], effects(vec![]));
    h2.version = "fake-2".into();
    w.set_hooks(
        Some(Arc::new(h2) as Arc<dyn WorldHooks>),
        10,
        HooksChange::Start,
    );
    assert!(w.state.hook_faults.is_empty());
    assert!(run(&mut w, &ada, "pry").ok);
}

#[test]
fn reload_with_unchanged_scripts_reopens_tripped_hooks() {
    let h = Arc::new(FakeHooks::new(vec![verb("pry")], fault));
    let mut w = world_with(&h);
    let ada = in_alley(&mut w);
    for _ in 0..3 {
        run(&mut w, &ada, "pry");
    }
    assert_eq!(w.state.hook_faults["verbs.pry"], 3);
    w.drain_fresh();
    w.set_hooks(
        Some(Arc::clone(&h) as Arc<dyn WorldHooks>),
        50,
        HooksChange::Reload,
    );
    assert!(w.state.hook_faults.is_empty());
    let marker = w.drain_fresh();
    assert_eq!(marker.len(), 1);
    assert_eq!(marker[0].data["why"], "reload");
    assert_eq!(marker[0].data["previous"], "fake-1");
    run(&mut w, &ada, "pry");
    assert_eq!(h.calls(), 4);
}

#[test]
fn hook_faults_survive_a_store_round_trip() {
    let h = Arc::new(FakeHooks::new(vec![verb("pry")], fault));
    let mut w = world_with(&h);
    let ada = in_alley(&mut w);
    run(&mut w, &ada, "pry");
    let mut store = crate::store::Store::open_in_memory(1, 0).unwrap();
    store
        .save("crypt-v01", &w.snapshot(), &w.drain_fresh(), &[], 5)
        .unwrap();
    let loaded = store.load(10).unwrap().unwrap();
    assert_eq!(loaded.state.hook_faults["verbs.pry"], 1);
    assert_eq!(loaded.state.scripts_version.as_deref(), Some("fake-1"));
}

fn roll_then(
    fx: Vec<ScriptEffect>,
    n: usize,
) -> impl Fn(&HookId, &HookInput, WorldDice) -> HookReply {
    move |_, _, mut dice| {
        for _ in 0..n {
            dice.d6();
        }
        HookReply {
            outcome: Ok((fx.clone(), dice)),
            log: Vec::new(),
        }
    }
}

#[test]
fn a_panicking_hook_never_loses_the_world_dice() {
    let h = Arc::new(FakeHooks::new(vec![verb("pry")], |_, _, _| {
        panic!("inside the hook")
    }));
    let mut w = world_with(&h);
    let mut twin = World::new(Arc::new(WorldPack::crypt_v01().unwrap()), 42);
    let ada = in_alley(&mut w);
    let ada2 = twin.login("ada", 1).unwrap();
    assert!(twin.execute(&ada2, "s", 2).ok);
    let r = run(&mut w, &ada, "pry");
    assert_eq!(r.error_code, Some(ErrorCode::ScriptFailed));
    assert_eq!(w.snapshot().dice_state, twin.snapshot().dice_state);
    let ours: Vec<u8> = (0..20).map(|_| w.dice.d6()).collect();
    let theirs: Vec<u8> = (0..20).map(|_| twin.dice.d6()).collect();
    assert_eq!(ours, theirs);
    let f = faults(&mut w);
    assert!(matches!(&f[0], Audit::ScriptFault { message, .. } if message == "the hook panicked"));
}

#[test]
fn a_faulted_hook_consumes_no_dice() {
    let h = Arc::new(FakeHooks::new(
        vec![verb("pry")],
        roll_then(
            vec![
                ScriptEffect::Hurt(DamageDice::new(1, 0)),
                ScriptEffect::Hurt(DamageDice::new(1, 0)),
            ],
            3,
        ),
    ));
    let mut w = world_with(&h);
    let ada = in_alley(&mut w);
    let before = w.snapshot().dice_state;
    run(&mut w, &ada, "pry");
    assert_eq!(w.snapshot().dice_state, before);
}

#[test]
fn hook_dice_come_from_the_world_and_are_committed_on_success() {
    let h = Arc::new(FakeHooks::new(
        vec![verb("pry")],
        |_, _, mut dice: WorldDice| {
            let a = dice.d6();
            let b = dice.d6();
            HookReply {
                outcome: Ok((vec![ScriptEffect::Msg(format!("{a}{b}"))], dice)),
                log: Vec::new(),
            }
        },
    ));
    let mut w = world_with(&h);
    let ada = in_alley(&mut w);
    w.fix_dice([2, 5, 3, 4]);
    assert_eq!(run(&mut w, &ada, "pry").narrative, "25");
    assert_eq!(run(&mut w, &ada, "pry").narrative, "34");
}

#[test]
fn a_hook_that_never_rolls_leaves_the_dice_alone() {
    let h = Arc::new(FakeHooks::new(
        vec![verb("pry")],
        effects(vec![ScriptEffect::Msg("hm".into())]),
    ));
    let mut w = world_with(&h);
    let ada = in_alley(&mut w);
    let before = w.snapshot().dice_state;
    assert!(run(&mut w, &ada, "pry").ok);
    assert_eq!(w.snapshot().dice_state, before);
}

// ------------------------------------------------------------ narration, hiding, death

#[test]
fn narration_is_a_system_event_with_no_actor_and_no_echo() {
    let h = Arc::new(FakeHooks::new(
        vec![verb("pry")],
        effects(vec![
            ScriptEffect::Msg("You kneel.".into()),
            ScriptEffect::Narrate("Someone kneels.".into()),
        ]),
    ));
    let mut w = world_with(&h);
    let ada = in_alley(&mut w);
    let bo = w.login("bo", 3).unwrap();
    assert!(w.execute(&bo, "s", 4).ok);
    w.drain_fresh();
    let r = run(&mut w, &ada, "pry");
    let evs = w.drain_fresh();
    assert_eq!(evs.len(), 1);
    let e = &evs[0];
    assert_eq!(e.kind, EventKind::System);
    assert!(e.actor.is_none());
    assert_eq!(e.tags, ["script", "verbs.pry"]);
    assert!(e.reaches(&bo));
    assert!(!e.reaches(&ada), "the actor gets msg, never an echo");
    assert_eq!(r.events, vec![e.seq]);
    assert_eq!(r.narrative, "You kneel.");
}

#[test]
fn a_hidden_mover_sets_off_no_room_narration() {
    let h = Arc::new(FakeHooks::new(
        vec![HookId::Enter(RoomId::from("dark_alley"))],
        effects(vec![
            ScriptEffect::Narrate("Dust falls.".into()),
            ScriptEffect::SetFlag("entered".into(), FlagValue::Bool(true)),
        ]),
    ));
    let mut w = world_with(&h);
    let ada = w.login("ada", 1).unwrap();
    w.fix_dice([1, 1, 1]);
    assert!(run(&mut w, &ada, "attempt stealth").ok);
    w.drain_fresh();
    assert!(run(&mut w, &ada, "s").ok);
    assert_eq!(h.calls(), 0);
    assert!(w.drain_fresh().iter().all(|e| e.kind != EventKind::System));
    assert!(w.state.flags.is_empty());
    // A visible walk sets it off, with where the walker came from.
    let bo = w.login("bo", 5).unwrap();
    let r = run(&mut w, &bo, "s");
    assert_eq!(h.calls(), 1);
    assert!(r.ok);
    assert_eq!(
        h.inputs.lock().unwrap()[0].from,
        Some(RoomId::from("town_square"))
    );
    assert_eq!(w.state.flags["entered"], FlagValue::Bool(true));
}

#[test]
fn a_scripted_kill_stops_the_remaining_effects() {
    let h = Arc::new(FakeHooks::new(
        vec![verb("hurtme")],
        effects(vec![
            ScriptEffect::Msg("The floor gives way.".into()),
            ScriptEffect::Hurt(DamageDice::new(6, 0)),
            ScriptEffect::Narrate("A scream.".into()),
            ScriptEffect::SetFlag("after".into(), FlagValue::Bool(true)),
            ScriptEffect::Msg("Darkness.".into()),
        ]),
    ));
    let mut w = world_with(&h);
    let ada = w.login("ada", 1).unwrap();
    w.state.entities.get_mut(&ada).unwrap().room = RoomId::from("cellar_landing");
    w.state.entities.get_mut(&ada).unwrap().pools.hp = 1;
    w.fix_dice([6; 40]);
    w.drain_audit();
    let r = run(&mut w, &ada, "hurtme");
    assert!(r.ok, "{r:?}");
    assert_eq!(r.narrative, "The floor gives way.\nDarkness.");
    let me = w.entity(&ada).unwrap();
    assert_eq!(
        me.room, w.pack.world.respawn_room,
        "a person wakes at the respawn room"
    );
    assert!(w.state.flags.is_empty());
    let logs: Vec<String> = w
        .drain_audit()
        .into_iter()
        .filter_map(|a| match a {
            Audit::ScriptLog { line, .. } => Some(line),
            _ => None,
        })
        .collect();
    assert!(
        logs.iter().any(|l| l.starts_with("skipped narrate")),
        "{logs:?}"
    );
    assert!(
        logs.iter().any(|l| l.starts_with("skipped set_flag")),
        "{logs:?}"
    );
}

#[test]
fn hurt_is_refused_in_a_safe_room() {
    let h = Arc::new(FakeHooks::new(
        vec![verb("hurtme")],
        effects(vec![ScriptEffect::Hurt(DamageDice::new(1, 0))]),
    ));
    let mut w = world_with(&h);
    let ada = in_alley(&mut w);
    w.drain_audit();
    assert_eq!(
        run(&mut w, &ada, "hurtme").error_code,
        Some(ErrorCode::ScriptFailed)
    );
    let f = faults(&mut w);
    assert!(
        matches!(&f[0], Audit::ScriptFault { message, .. } if message == "hurt() is not allowed in a safe room")
    );
}

#[test]
fn heal_never_exceeds_hp_max() {
    let h = Arc::new(FakeHooks::new(
        vec![verb("pry")],
        effects(vec![ScriptEffect::Heal(DamageDice::new(6, 0))]),
    ));
    let mut w = world_with(&h);
    let ada = in_alley(&mut w);
    {
        let me = w.state.entities.get_mut(&ada).unwrap();
        me.pools.hp = -1;
        me.unconscious = false;
    }
    w.fix_dice([6; 6]);
    let r = run(&mut w, &ada, "pry");
    let me = w.entity(&ada).unwrap();
    assert_eq!(me.pools.hp, me.pools.hp_max);
    assert!(r.state_changes.contains(&StateChange::Hp {
        entity: ada.clone(),
        delta: me.pools.hp_max + 1,
        hp: me.pools.hp_max
    }));
}

#[test]
fn one_hook_cannot_hurt_and_then_heal() {
    let h = Arc::new(FakeHooks::new(
        vec![verb("hurtme")],
        effects(vec![
            ScriptEffect::Hurt(DamageDice::new(1, 0)),
            ScriptEffect::Heal(DamageDice::new(1, 0)),
        ]),
    ));
    let mut w = world_with(&h);
    let ada = w.login("ada", 1).unwrap();
    w.state.entities.get_mut(&ada).unwrap().room = RoomId::from("cellar_landing");
    w.drain_audit();
    let r = run(&mut w, &ada, "hurtme");
    assert_eq!(r.error_code, Some(ErrorCode::ScriptFailed));
    assert!(matches!(
        &faults(&mut w)[0],
        Audit::ScriptFault { message, .. } if message == "at most one heal() or hurt() per hook"
    ));
}

#[test]
fn a_hook_never_runs_for_an_actor_out_cold() {
    // So a script heal never meets an unconscious actor through a hook:
    // the command is refused before the hook is called.
    let h = Arc::new(FakeHooks::new(
        vec![verb("pry")],
        effects(vec![ScriptEffect::Heal(DamageDice::new(6, 0))]),
    ));
    let mut w = world_with(&h);
    let ada = in_alley(&mut w);
    {
        let me = w.state.entities.get_mut(&ada).unwrap();
        me.pools.hp = -2;
        me.unconscious = true;
    }
    let r = run(&mut w, &ada, "pry");
    assert_eq!(r.error_code, Some(ErrorCode::Incapacitated), "{r:?}");
    assert_eq!(h.calls(), 0);
}

#[test]
fn a_script_heal_that_lifts_a_downed_actor_past_0_hp_brings_it_to() {
    // The heal effect itself, on an actor out cold: it brings her to as
    // regen does, with the change, the room line and no actor on it.
    let h = Arc::new(FakeHooks::new(Vec::new(), effects(Vec::new())));
    let mut w = world_with(&h);
    let ada = w.login("ada", 1).unwrap();
    {
        let me = w.state.entities.get_mut(&ada).unwrap();
        me.room = RoomId::from("cellar_landing");
        me.pools.hp = -2;
        me.unconscious = true;
    }
    w.fix_dice([6]);
    let mut ctx = Ctx::default();
    w.heal_actor(&mut ctx, &ada, DamageDice::new(1, 0));
    let me = w.entity(&ada).unwrap();
    assert_eq!(me.pools.hp, 4);
    assert!(!me.unconscious, "brought to, not left out cold at 4 HP");
    assert!(ctx.changes.contains(&StateChange::Status {
        entity: ada.clone(),
        status: "unconscious".into(),
        value: false,
    }));
    let came_to: Vec<_> = w
        .logged()
        .filter(|e| e.tags.iter().any(|t| t == CAME_TO_TAG))
        .collect();
    assert_eq!(came_to.len(), 1, "{came_to:?}");
    assert!(came_to[0].targets(&ada));
    assert!(came_to[0].actor.is_none(), "no actor: her mind is woken");
    // A heal that leaves her at 0 HP or below leaves her out cold.
    let mut w = world_with(&h);
    let ada = w.login("ada", 1).unwrap();
    {
        let me = w.state.entities.get_mut(&ada).unwrap();
        me.pools.hp = -6;
        me.unconscious = true;
    }
    w.fix_dice([6]);
    let mut ctx = Ctx::default();
    w.heal_actor(&mut ctx, &ada, DamageDice::new(1, 0));
    assert!(w.entity(&ada).unwrap().unconscious);
    assert!(!w.logged().any(|e| e.tags.iter().any(|t| t == CAME_TO_TAG)));
}

// ------------------------------------------------------------ flags and marks

#[test]
fn flags_and_marks_past_capacity_decline_the_whole_list_without_a_fault() {
    let h = Arc::new(FakeHooks::new(
        vec![verb("pry")],
        effects(vec![
            ScriptEffect::SetFlag("f0".into(), FlagValue::Int(9)),
            ScriptEffect::SetFlag("one_more".into(), FlagValue::Int(1)),
            ScriptEffect::Mark("one_more".into(), FlagValue::Int(1)),
        ]),
    ));
    let mut w = world_with(&h);
    let ada = in_alley(&mut w);
    for i in 0..FLAG_CAP {
        w.state.flags.insert(format!("f{i}"), FlagValue::Int(0));
    }
    for i in 0..MARK_CAP {
        w.state
            .entities
            .get_mut(&ada)
            .unwrap()
            .marks
            .insert(format!("m{i}"), FlagValue::Int(0));
    }
    let r = run(&mut w, &ada, "pry");
    assert_eq!(r.error_code, Some(ErrorCode::Refused));
    assert_eq!(r.narrative, DECLINED);
    assert!(!w.state.flags.contains_key("one_more"));
    assert_eq!(
        w.state.flags["f0"],
        FlagValue::Int(0),
        "all or nothing: the flag that fits is not set either"
    );
    assert_eq!(w.entity(&ada).unwrap().marks.len(), MARK_CAP);
    assert!(w.state.hook_faults.is_empty(), "a full table is no fault");
    assert!(w.drain_audit().iter().any(|a| matches!(
        a,
        Audit::ScriptLog { line, .. } if line == "declined: flags full (256); one_more would not be kept"
    )));
    assert_eq!(w.state.check(), Vec::<String>::new());
    // With room for the flag, the full mark table declines the list.
    w.state.flags.remove("f255");
    assert_eq!(
        run(&mut w, &ada, "pry").error_code,
        Some(ErrorCode::Refused)
    );
    assert!(w.drain_audit().iter().any(|a| matches!(
        a,
        Audit::ScriptLog { line, .. } if line == "declined: marks full (32); one_more would not be kept"
    )));
    // With room for both, it applies.
    w.state.entities.get_mut(&ada).unwrap().marks.remove("m0");
    assert!(run(&mut w, &ada, "pry").ok);
    assert_eq!(w.state.flags["f0"], FlagValue::Int(9));
    assert_eq!(w.state.flags.len(), FLAG_CAP);
    assert_eq!(w.entity(&ada).unwrap().marks.len(), MARK_CAP);
}

#[test]
fn flag_and_mark_changes_stay_out_of_state_changes_and_every_view() {
    let h = Arc::new(FakeHooks::new(
        vec![verb("pry")],
        effects(vec![
            ScriptEffect::SetFlag("zq_flag".into(), FlagValue::Text("zq_value".into())),
            ScriptEffect::Mark("zq_mark".into(), FlagValue::Bool(true)),
        ]),
    ));
    let mut w = world_with(&h);
    let ada = in_alley(&mut w);
    let r = run(&mut w, &ada, "pry");
    assert_eq!(r.state_changes, Vec::new());
    let views = format!("{}{}{}", w.look_view(&ada), w.sheet_view(&ada), w.who().1);
    assert!(!views.contains("zq_"), "{views}");
    // Clearing works.
    let h2 = Arc::new(FakeHooks::new(
        vec![verb("pry")],
        effects(vec![
            ScriptEffect::ClearFlag("zq_flag".into()),
            ScriptEffect::Unmark("zq_mark".into()),
        ]),
    ));
    w.set_hooks(Some(h2 as Arc<dyn WorldHooks>), 3, HooksChange::Reload);
    run(&mut w, &ada, "pry");
    assert!(w.state.flags.is_empty());
    assert!(w.entity(&ada).unwrap().marks.is_empty());
}

// ------------------------------------------------------------ views

const FORBIDDEN_KEYS: [&str; 8] = [
    "tier",
    "human",
    "human_actor",
    "player_actor",
    "mind",
    "runtime",
    "turing",
    "persistent",
];

fn keys(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                out.push(k.clone());
                keys(x, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| keys(x, out)),
        _ => {}
    }
}

#[test]
fn hook_input_never_carries_tier_human_or_mind() {
    let h = Arc::new(FakeHooks::new(
        vec![HookId::Enter(RoomId::from("rusty_flagon_taproom"))],
        effects(vec![]),
    ));
    let mut w = world_with(&h);
    let ada = w.login("ada", 1).unwrap();
    let kestrel = id("pc_kestrel");
    w.bring_online(&kestrel, 1);
    w.state.entities.get_mut(&kestrel).unwrap().room = RoomId::from("town_square");
    assert!(run(&mut w, &ada, "e").ok);
    assert!(run(&mut w, &kestrel, "e").ok);
    let inputs = h.inputs.lock().unwrap().clone();
    assert_eq!(inputs.len(), 2);
    for input in &inputs {
        let v = serde_json::to_value(input).unwrap();
        let mut ks = Vec::new();
        keys(&v, &mut ks);
        let text = v.to_string();
        for bad in FORBIDDEN_KEYS {
            assert!(!ks.iter().any(|k| k == bad), "{bad} in {text}");
            assert!(!text.contains(&format!("\"{bad}\"")), "{bad} in {text}");
        }
        assert!(input.cast.keys().all(|k| k != &kestrel && k != &ada));
    }
    let (a, k) = (
        serde_json::to_value(&inputs[0].actor).unwrap(),
        serde_json::to_value(&inputs[1].actor).unwrap(),
    );
    let (mut ka, mut kk) = (Vec::new(), Vec::new());
    keys(&a, &mut ka);
    keys(&k, &mut kk);
    let strip = |v: &mut Vec<String>| {
        v.retain(|k| {
            !k.starts_with(char::is_lowercase)
                || [
                    "id",
                    "name",
                    "kind",
                    "room",
                    "hp",
                    "hp_max",
                    "condition",
                    "attrs",
                    "skills",
                    "inventory",
                    "equipped",
                    "currency",
                    "marks",
                ]
                .contains(&k.as_str())
        });
        v.sort();
        v.dedup();
    };
    strip(&mut ka);
    strip(&mut kk);
    assert_eq!(ka, kk, "a human and an agent PC have the same facts");
    assert_eq!(a["kind"], "player");
    assert_eq!(k["kind"], "player");
}

#[test]
fn hook_input_never_carries_player_text() {
    let h = Arc::new(FakeHooks::new(
        vec![HookId::Speech(RoomId::from("town_square"))],
        effects(vec![]),
    ));
    let mut w = world_with(&h);
    let ada = w.login("ada", 1).unwrap();
    run(&mut w, &ada, "say zq-marker vich");
    let input = h.inputs.lock().unwrap()[0].clone();
    assert_eq!(input.text.as_deref(), Some("zq-marker vich"));
    assert!(!serde_json::to_string(&input).unwrap().contains("zq-marker"));
}

#[test]
fn hook_input_lists_are_capped_at_32() {
    let h = Arc::new(FakeHooks::new(vec![verb("pry")], effects(vec![])));
    let mut w = world_with(&h);
    let ada = in_alley(&mut w);
    for i in 0..40 {
        let p = w.login(&format!("p{i:02}"), 3).unwrap();
        w.state.entities.get_mut(&p).unwrap().room = RoomId::from("dark_alley");
        let pack = Arc::clone(&w.pack);
        let item = w.state.new_item(&ProtoId::from("iron_ore"), &pack);
        w.state.attach(
            item,
            &Holder::Room {
                room: RoomId::from("dark_alley"),
            },
        );
    }
    run(&mut w, &ada, "pry");
    let input = h.inputs.lock().unwrap()[0].clone();
    assert_eq!(input.room.present.len(), 32);
    assert_eq!(input.room.items.len(), 32);
    assert!(
        input.room.present.iter().all(|p| p.id != ada),
        "the actor is not in present"
    );
}

#[test]
fn cast_facts_hide_hidden_entities() {
    let h = Arc::new(FakeHooks::new(vec![verb("pry")], effects(vec![])));
    let mut w = world_with(&h);
    let ada = in_alley(&mut w);
    w.state.entities.get_mut(&id("vich")).unwrap().hidden = true;
    w.state
        .entities
        .get_mut(&id("giant_rat_1"))
        .unwrap()
        .dead_until_ms = Some(u64::MAX);
    run(&mut w, &ada, "pry");
    let input = h.inputs.lock().unwrap()[0].clone();
    let vich = &input.cast[&id("vich")];
    assert!(vich.alive && !vich.present && vich.room.is_none());
    let rat = &input.cast[&id("giant_rat_1")];
    assert!(!rat.alive && !rat.present && rat.room.is_none());
    let hale = &input.cast[&id("guard_01")];
    assert!(hale.present && hale.room == Some(RoomId::from("town_square")));
}

#[test]
fn names_in_script_views_are_identifiers() {
    let h = Arc::new(FakeHooks::new(vec![verb("pry")], effects(vec![])));
    let mut w = world_with(&h);
    let ada = in_alley(&mut w);
    run(&mut w, &ada, "pry");
    let input = h.inputs.lock().unwrap()[0].clone();
    // Login names are 2 to 16 of [A-Za-z0-9_]; cast names come from the pack.
    assert!(
        input
            .actor
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    );
    for c in input.cast.values() {
        assert!(
            c.name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ' ')
        );
    }
}

// ------------------------------------------------------------ hook points

#[test]
fn at_most_one_hook_per_command_and_effects_set_off_none() {
    let h = Arc::new(FakeHooks::new(
        vec![
            verb("pry"),
            HookId::Enter(RoomId::from("cellar_landing")),
            HookId::Speech(RoomId::from("dark_alley")),
        ],
        effects(vec![
            ScriptEffect::Unlock(Direction::D),
            ScriptEffect::Narrate("Something stirs.".into()),
        ]),
    ));
    let mut w = world_with(&h);
    let ada = in_alley(&mut w);
    run(&mut w, &ada, "pry");
    assert_eq!(
        h.calls(),
        1,
        "unlocking and narrating set off no other hook"
    );
    // Walking through the opened grate sets off the enter hook once.
    let calls = h.calls();
    let h2 = Arc::new(FakeHooks::new(
        vec![HookId::Enter(RoomId::from("cellar_landing"))],
        effects(vec![]),
    ));
    w.set_hooks(
        Some(Arc::clone(&h2) as Arc<dyn WorldHooks>),
        3,
        HooksChange::Reload,
    );
    assert!(run(&mut w, &ada, "d").ok);
    assert_eq!(h2.calls(), 1);
    assert_eq!(h.calls(), calls);
}

#[test]
fn on_take_runs_only_after_a_successful_take() {
    let h = Arc::new(FakeHooks::new(
        vec![HookId::Take(ProtoId::from("iron_ore"))],
        effects(vec![ScriptEffect::Msg("Heavy.".into())]),
    ));
    let mut w = world_with(&h);
    let ada = w.login("ada", 1).unwrap();
    assert!(!run(&mut w, &ada, "take ore").ok);
    assert_eq!(h.calls(), 0);
    let pack = Arc::clone(&w.pack);
    let item = w.state.new_item(&ProtoId::from("iron_ore"), &pack);
    w.state.attach(
        item,
        &Holder::Room {
            room: RoomId::from("town_square"),
        },
    );
    let r = run(&mut w, &ada, "take ore");
    assert!(r.ok);
    assert_eq!(
        r.narrative,
        "You take the chunk of dwarven iron ore.\nHeavy."
    );
    let input = h.inputs.lock().unwrap()[0].clone();
    assert_eq!(input.item.unwrap().id, item);
}

#[test]
fn on_use_runs_only_for_carried_items_without_an_engine_use() {
    let h = Arc::new(FakeHooks::new(
        vec![HookId::Use(ProtoId::from("bone_charm"))],
        effects(vec![ScriptEffect::Msg("It hums.".into())]),
    ));
    let mut w = world_with(&h);
    let ada = w.login("ada", 1).unwrap();
    let pack = Arc::clone(&w.pack);
    let floor = w.state.new_item(&ProtoId::from("bone_charm"), &pack);
    w.state.attach(
        floor,
        &Holder::Room {
            room: RoomId::from("town_square"),
        },
    );
    assert_eq!(
        run(&mut w, &ada, "use charm").error_code,
        Some(ErrorCode::CannotUse)
    );
    assert_eq!(h.calls(), 0, "on the floor: pick it up first");
    assert!(run(&mut w, &ada, "take charm").ok);
    let r = run(&mut w, &ada, "use charm");
    assert!(r.ok, "{r:?}");
    assert_eq!(r.narrative, "It hums.");
    assert_eq!(r.resolution["script"], "items.bone_charm.on_use");
    assert_eq!(r.verb.as_deref(), Some("use"));
    // An item with an engine use never reaches a script.
    assert!(run(&mut w, &ada, "use torch").ok);
    assert_eq!(h.calls(), 1);
}

#[test]
fn use_on_passes_a_resolved_target_and_refuses_an_unknown_one() {
    let h = Arc::new(FakeHooks::new(
        vec![HookId::Use(ProtoId::from("bone_charm"))],
        effects(vec![ScriptEffect::Msg("It hums.".into())]),
    ));
    let mut w = world_with(&h);
    let ada = w.login("ada", 1).unwrap();
    let pack = Arc::clone(&w.pack);
    let charm = w.state.new_item(&ProtoId::from("bone_charm"), &pack);
    w.state.attach(
        charm,
        &Holder::Entity {
            entity: ada.clone(),
        },
    );
    let r = run(&mut w, &ada, "use charm on nobody");
    assert_eq!(r.error_code, Some(ErrorCode::NoSuchTarget));
    assert_eq!(h.calls(), 0);
    assert!(run(&mut w, &ada, "use charm on hale").ok);
    let input = h.inputs.lock().unwrap()[0].clone();
    let target = input.target.unwrap();
    assert_eq!(target.id, id("guard_01"));
    assert_eq!(target.kind, "npc");
}

#[test]
fn a_custom_verb_target_is_an_entity_here_or_none() {
    let h = Arc::new(FakeHooks::new(vec![verb("bow")], effects(vec![])));
    let mut w = world_with(&h);
    let ada = w.login("ada", 1).unwrap();
    run(&mut w, &ada, "bow hale");
    run(&mut w, &ada, "bow to the moon");
    let inputs = h.inputs.lock().unwrap().clone();
    assert_eq!(
        inputs[0].target.as_ref().map(|t| t.id.clone()),
        Some(id("guard_01"))
    );
    assert!(inputs[1].target.is_none());
}

// ------------------------------------------------------------ markers, look, help

#[test]
fn set_hooks_emits_a_marker_no_entity_perceives() {
    let h = Arc::new(FakeHooks::new(vec![], effects(vec![])));
    let mut w = World::new(Arc::new(WorldPack::crypt_v01().unwrap()), 1);
    w.set_hooks(Some(h as Arc<dyn WorldHooks>), 7, HooksChange::Start);
    let evs = w.drain_fresh();
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0].kind, EventKind::System);
    assert_eq!(evs[0].recipients, Vec::<EntityId>::new());
    assert_eq!(evs[0].narrative, "scripts fake-1 loaded");
    assert_eq!(evs[0].data["previous"], Value::Null);
    assert_eq!(w.state.scripts_version.as_deref(), Some("fake-1"));
    assert!(
        w.drain_audit()
            .iter()
            .any(|a| matches!(a, Audit::ScriptsLoaded { why, .. } if why == "start"))
    );
}

#[test]
fn scripts_disabled_records_version_none() {
    let mut w = World::new(Arc::new(WorldPack::crypt_v01().unwrap()), 1);
    w.set_hooks(None, 7, HooksChange::Start);
    assert_eq!(w.state.scripts_version.as_deref(), Some("none"));
    assert_eq!(w.drain_fresh().len(), 1);
    w.set_hooks(None, 8, HooksChange::Start);
    assert!(
        w.drain_fresh().is_empty(),
        "the same version at start records nothing"
    );
}

#[test]
fn look_and_help_list_the_verbs_usable_here() {
    let h = Arc::new(FakeHooks::new(
        vec![verb("pry"), verb("bow")],
        effects(vec![]),
    ));
    let mut w = world_with(&h);
    let ada = w.login("ada", 1).unwrap();
    assert_eq!(w.look_view(&ada)["verbs"], serde_json::json!(["bow"]));
    let r = run(&mut w, &ada, "look");
    assert!(
        r.narrative.ends_with("You could also: bow."),
        "{}",
        r.narrative
    );
    assert!(run(&mut w, &ada, "s").ok);
    assert_eq!(
        w.look_view(&ada)["verbs"],
        serde_json::json!(["bow", "pry"])
    );
    let help = run(&mut w, &ada, "help").narrative;
    assert!(help.ends_with("here: bow | pry"), "{help}");
    let plain = World::new(Arc::new(WorldPack::crypt_v01().unwrap()), 1);
    assert_eq!(
        plain.look_view(&id("guard_01"))["verbs"],
        serde_json::json!([])
    );
    let plain_help = plain.help_text(&id("guard_01"));
    assert!(plain_help.starts_with(crate::parser::HELP));
    assert!(!plain_help.contains("here:"), "{plain_help}");
    assert!(plain_help.contains("\nrecovery: "), "{plain_help}");
}

// ------------------------------------------------------------ compatibility

#[test]
fn old_snapshots_without_script_fields_load() {
    let w = World::new(Arc::new(WorldPack::crypt_v01().unwrap()), 1);
    let mut v = serde_json::to_value(w.snapshot()).unwrap();
    let obj = v.as_object_mut().unwrap();
    obj.remove("flags");
    obj.remove("hook_faults");
    obj.remove("scripts_version");
    obj.remove("pool_spent");
    obj.remove("minted");
    let back: crate::state::WorldState = serde_json::from_value(v.clone()).unwrap();
    assert!(back.flags.is_empty() && back.hook_faults.is_empty() && back.scripts_version.is_none());
    assert!(back.pool_spent.is_empty() && back.minted.is_empty());
    assert_eq!(back, w.snapshot());
    // A snapshot from before the ledger moved to its own table still
    // loads; its rows are not kept in the state.
    v.as_object_mut()
        .unwrap()
        .insert("mints".into(), serde_json::json!([]));
    let back: crate::state::WorldState = serde_json::from_value(v).unwrap();
    assert_eq!(back, w.snapshot());
}

#[test]
fn a_world_without_hooks_here_plays_exactly_as_before() {
    let hooks = Arc::new(FakeHooks::new(
        vec![HookId::Enter(RoomId::from("ossuary_crypt"))],
        effects(vec![ScriptEffect::Msg("ada1".into())]),
    ));
    let mut plain = World::new(Arc::new(WorldPack::crypt_v01().unwrap()), 9);
    let mut scripted = World::new(Arc::new(WorldPack::crypt_v01().unwrap()), 9);
    scripted.set_hooks(
        Some(Arc::clone(&hooks) as Arc<dyn WorldHooks>),
        0,
        HooksChange::Start,
    );
    scripted.drain_fresh();
    scripted.state.next_seq = plain.state.next_seq;
    scripted.state.scripts_version = None;
    let (ada1, ada2) = (
        plain.login("ada", 1).unwrap(),
        scripted.login("ada", 1).unwrap(),
    );
    for (step, line) in [
        "e",
        "say hi",
        "w",
        "s",
        "take nothing",
        "n",
        "w",
        "buy torch",
        "e",
    ]
    .iter()
    .enumerate()
    {
        let now = 10 + step as u64;
        assert_eq!(
            plain.execute(&ada1, line, now),
            scripted.execute(&ada2, line, now),
            "{line}"
        );
    }
    assert_eq!(plain.snapshot(), scripted.snapshot());
    assert_eq!(hooks.calls(), 0);
}
