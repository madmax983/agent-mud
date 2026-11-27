//! The dry run: every hook once, and every mind twice, on probe inputs.
//!
//! `mud check`, `mud serve` and every reload run it, so a typo in a
//! property name, a wrong return shape or an inadmissible effect list is
//! found before a player meets it.

use std::sync::Arc;

use mud_world::hooks::{HookId, WorldDice, tally, tally_admissible, tally_reason};
use mud_world::{EntityId, World, WorldPack};
use serde_json::{Value, json};

use crate::host::Compiled;
use crate::mind::{EventActor, MindWake, ScriptEvent};

/// The game clock of a dry run: 2023-11-14, 22:13 UTC.
pub const DRY_RUN_NOW_MS: u64 = 1_700_000_000_000;

fn hook_room(world: &World, hook: &HookId, c: &Compiled) -> mud_world::RoomId {
    match hook {
        HookId::Enter(r) | HookId::Speech(r) => r.clone(),
        HookId::Verb(v) => c
            .verbs
            .get(v)
            .and_then(|i| i.rooms.first().cloned())
            .unwrap_or_else(|| world.pack().world.start_room.clone()),
        HookId::Take(_) | HookId::Use(_) => world.pack().world.start_room.clone(),
    }
}

impl Compiled {
    /// Runs every hook once and every mind twice. Returns each problem as
    /// `dry run {hook}: {message}`.
    #[must_use]
    pub fn dry_run(&self, pack: &Arc<WorldPack>) -> Vec<String> {
        let mut world = World::new(Arc::clone(pack), 0);
        let probe = world
            .login("probe", DRY_RUN_NOW_MS)
            .unwrap_or_else(|_| EntityId::from("pc_probe"));
        let mut problems = Vec::new();
        for hook in self.hooks.keys() {
            let room = hook_room(&world, hook, self);
            let input = world.probe_input(&room, hook, DRY_RUN_NOW_MS);
            let reply = self.call_hook(hook, &input, WorldDice::fixed([3, 3, 3]));
            let checked = reply.outcome.and_then(|(fx, _)| {
                let t = tally(&fx);
                let max = self.limits.max_effects;
                if !tally_admissible(t, max, hook.is_command()) {
                    return Err(tally_reason(t, max, hook.is_command()));
                }
                // A fresh world declines nothing a sound hook asks: its
                // pools are full and the probe holds no marks. It carries
                // only the template kit, so a use_up() of anything else
                // declines, and the problem says to check carries() first.
                world
                    .check_effects(&room, &probe, &fx, DRY_RUN_NOW_MS)
                    .map_err(|e| e.to_string())
            });
            if let Err(e) = checked {
                problems.push(format!("dry run {}: {e}", hook.key()));
            }
        }
        for id in self.minds.keys() {
            let mut me = world.actor_facts(id).map_or(Value::Null, |mut f| {
                f.marks = None;
                serde_json::to_value(f).unwrap_or(Value::Null)
            });
            // As the supervisor gives it: the mind's own soft post.
            if let Some(obj) = me.as_object_mut() {
                obj.insert("post".into(), world.post_view(id));
            }
            let here = world.look_view(id);
            let room = here["room_id"].as_str().map(str::to_owned);
            let who = EventActor {
                id: probe.clone(),
                name: "probe".into(),
                kind: "player".into(),
            };
            let event = |seq: u64, kind: &str, verb: &str| ScriptEvent {
                seq,
                kind: kind.into(),
                verb: Some(verb.into()),
                tags: Vec::new(),
                room: room.clone(),
                actor: Some(who.clone()),
                target: None,
                is_self: false,
                at_me: false,
                text: (kind == "speech").then(String::new),
            };
            let wakes = [
                MindWake {
                    entity: id.clone(),
                    priority: "macro_tick",
                    macro_tick: true,
                    macro_ticks: 1,
                    login: false,
                    now_ms: DRY_RUN_NOW_MS,
                    seed: 1,
                    me: me.clone(),
                    here: here.clone(),
                    events: Vec::new(),
                    booked: Vec::new(),
                    toml_plan: Vec::new(),
                    memory: json!({}),
                },
                MindWake {
                    entity: id.clone(),
                    priority: "room_batch",
                    macro_tick: false,
                    macro_ticks: 1,
                    login: false,
                    now_ms: DRY_RUN_NOW_MS,
                    seed: 2,
                    me,
                    here,
                    events: vec![event(1, "enter", "move"), event(2, "speech", "say")],
                    booked: vec![(1, DRY_RUN_NOW_MS, String::new())],
                    toml_plan: Vec::new(),
                    memory: json!({}),
                },
            ];
            for w in &wakes {
                if let Err(e) = self.on_wake(w).outcome {
                    problems.push(format!("dry run minds.{id}.on_wake: {e}"));
                    break;
                }
            }
        }
        problems
    }
}
