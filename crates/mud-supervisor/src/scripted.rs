//! Deterministic minds: cast scripts, Rhai mind scripts, and the budget
//! fallback.
//!
//! The scripted mind runs the `script` rules of `cast.toml`. It needs no
//! model and no key, so the server runs out of the box, the smoke test is
//! repeatable, and a budget-capped entity still behaves like itself.
//!
//! With a script host ([`ScriptedMind::with_scripts`]), an entity that has
//! a Rhai mind script runs it instead: the script sees the TOML plan and
//! decides. A fault, or a tripped breaker, runs the TOML plan, so TOML is
//! the safety net. Either way the lines go through the typing gate.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use mud_script::{EventActor, MindWake, ScriptEvent, ScriptHost};
use mud_world::hooks::{HookOutcome, hook_allowed, hook_step};
use mud_world::pack::{CastDef, EntityKind, ScriptRule, ScriptTrigger, Who};
use mud_world::{Audit, EntityId, Event, EventKind, Game, World};

use crate::mind::{BoxFuture, Mind, MindError, MindReply, MudTools, WakeContext};
use crate::router::Priority;
use crate::turing::mix;

/// The placeholder a script fills with the triggering entity's name.
const ACTOR_SLOT: &str = concat!("{", "actor", "}");

/// The nth counters a plan would move: rule index and its new count.
type Turns = Vec<(usize, u32)>;

/// Runs the cast's script rules, and Rhai mind scripts when it has a host.
#[derive(Debug, Default)]
pub struct ScriptedMind {
    rules: HashMap<EntityId, Vec<ScriptRule>>,
    fired: Mutex<HashMap<(EntityId, usize), u32>>,
    scripts: Option<Scripts>,
}

/// The Rhai side of a scripted mind.
#[derive(Debug)]
struct Scripts {
    host: Arc<ScriptHost>,
    game: Arc<Game>,
    seed: u64,
    /// Consecutive faults per mind. In memory: a restart reopens them.
    breakers: Mutex<HashMap<EntityId, u8>>,
    /// The version and reload count the breakers belong to.
    seen: Mutex<(String, u64)>,
}

/// What the Rhai side decided.
enum Decision {
    /// The script's lines replace the TOML plan.
    Lines(Vec<String>, String),
    /// Run the TOML plan; the note says why.
    Toml(String),
}

const fn kind_word(k: EntityKind) -> &'static str {
    match k {
        EntityKind::Player => "player",
        EntityKind::Npc => "npc",
        EntityKind::Mob => "mob",
    }
}

const fn priority_word(p: Priority) -> &'static str {
    match p {
        Priority::MacroTick => "macro_tick",
        Priority::Booked => "booked",
        Priority::Ambient => "ambient",
        Priority::RoomBatch => "room_batch",
        Priority::Immediate => "immediate",
    }
}

/// An event as a mind script sees it: kinds from the world, never a tier,
/// and player text only as text the script cannot quote.
fn script_event(w: &World, me: &EntityId, ev: &Event) -> ScriptEvent {
    let who = |a: &mud_world::ActorRef| EventActor {
        id: a.id.clone(),
        name: a.name.clone(),
        kind: w
            .entity(&a.id)
            .map_or("npc", |e| kind_word(e.kind))
            .to_owned(),
    };
    ScriptEvent {
        seq: ev.seq.0,
        kind: ev.kind.name(),
        verb: ev.verb.clone(),
        tags: ev.tags.clone(),
        room: ev.room.as_ref().map(ToString::to_string),
        actor: ev.actor.as_ref().map(who),
        target: ev.target.as_ref().map(who),
        is_self: ev.is_by(me),
        at_me: ev.targets(me),
        text: ev.untrusted_text.clone(),
    }
}

impl Scripts {
    fn breakers(&self) -> std::sync::MutexGuard<'_, HashMap<EntityId, u8>> {
        self.breakers.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Resets every mind breaker when the scripts changed or were reloaded.
    fn follow_reloads(&self) {
        let now = (self.host.version(), self.host.reloads());
        let changed = {
            let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
            let changed = *seen != now;
            *seen = now;
            changed
        };
        if changed {
            self.breakers().clear();
        }
    }

    async fn decide(&self, ctx: &WakeContext, toml_plan: &[String], tools: &MudTools) -> Decision {
        self.follow_reloads();
        let e = &ctx.entity;
        let compiled = self.host.current();
        let max = compiled.limits.max_consecutive_faults;
        let faults = self.breakers().get(e).copied().unwrap_or(0);
        if !hook_allowed(faults, max) {
            return Decision::Toml(format!("rhai tripped ({faults}/{max}); ran the TOML plan"));
        }
        let schema = self.host.mind_memory_schema(e);
        let memory = self.game.script_memory(e, schema);
        let (me, here, events) = self.game.read(|w| {
            let me = w.actor_facts(e).map_or(serde_json::Value::Null, |mut f| {
                f.marks = None;
                serde_json::to_value(f).unwrap_or(serde_json::Value::Null)
            });
            let events: Vec<ScriptEvent> =
                ctx.events.iter().map(|ev| script_event(w, e, ev)).collect();
            (me, w.look_view(e), events)
        });
        let wake = MindWake {
            entity: e.clone(),
            priority: priority_word(ctx.priority),
            macro_tick: ctx.macro_tick,
            macro_ticks: ctx.macro_ticks,
            login: ctx.login,
            now_ms: ctx.now_ms,
            seed: mix(
                self.seed,
                &[e.as_str().as_bytes(), ctx.wake_id.as_bytes(), b"rhai"],
            ),
            me,
            here,
            events,
            booked: ctx
                .booked
                .iter()
                .map(|b| (b.id, b.due_ms, b.note.clone()))
                .collect(),
            toml_plan: toml_plan.to_vec(),
            memory: memory.clone(),
        };
        // Mind CPU never runs on an async worker.
        let reply = tokio::task::spawn_blocking(move || compiled.on_wake(&wake))
            .await
            .map_err(|e| format!("the mind script task failed: {e}"));
        let hook = format!("minds.{e}.on_wake");
        let now = self.game.now_ms();
        let (outcome, log) = match reply {
            Ok(r) => (r.outcome, r.log),
            Err(msg) => (Err(msg), Vec::new()),
        };
        for line in log {
            self.game.publish_audit(Audit::ScriptLog {
                hook: hook.clone(),
                entity: e.clone(),
                line,
                at_ms: now,
            });
        }
        match outcome {
            Ok(plan) => {
                self.breakers().remove(e);
                if let Some(m) = plan.memory.filter(|m| *m != memory) {
                    self.game.save_script_memory(e, schema, m);
                }
                if let Some((minutes, note)) = &plan.wake_me {
                    let booked = tools.wake_me(*minutes, note);
                    if booked["ok"] != true {
                        tracing::info!(entity = %e, answer = %booked, "a mind script's wake was not booked");
                    }
                }
                let n = plan.lines.len();
                Decision::Lines(plan.lines, format!("rhai: {n} lines"))
            }
            Err(message) => {
                let (next, allowed) = hook_step(faults, HookOutcome::Faulted, max);
                self.breakers().insert(e.clone(), next);
                self.game.publish_audit(Audit::ScriptFault {
                    hook,
                    entity: e.clone(),
                    message: message.chars().take(300).collect(),
                    faults: next,
                    tripped: !allowed,
                    at_ms: now,
                });
                Decision::Toml(format!(
                    "rhai fault ({next}/{max}): {message}; ran the TOML plan"
                ))
            }
        }
    }
}

fn rule_matches_event(rule: &ScriptRule, ev: &Event, me: &EntityId, my_room: Option<&str>) -> bool {
    if ev.is_by(me) {
        return false;
    }
    let kind_ok = match rule.on {
        ScriptTrigger::Enter => ev.kind == EventKind::Enter,
        ScriptTrigger::Speech => ev.kind == EventKind::Speech,
        ScriptTrigger::Tell => ev.kind == EventKind::Tell && ev.targets(me),
        ScriptTrigger::Attacked => ev.kind == EventKind::Combat && ev.targets(me),
        ScriptTrigger::Nudge => ev.kind == EventKind::SelfControlNudge,
        ScriptTrigger::MacroTick | ScriptTrigger::Login => false,
    };
    if !kind_ok {
        return false;
    }
    let who_ok = match rule.who {
        Who::Any => true,
        Who::Human => ev.player_actor,
        Who::Agent => !ev.player_actor,
    };
    let room_ok = rule
        .room
        .as_ref()
        .is_none_or(|r| ev.room.as_ref() == Some(r) && my_room == Some(r.as_str()));
    let words_ok = rule.keywords.is_empty() || {
        let text = ev
            .untrusted_text
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase();
        rule.keywords
            .iter()
            .any(|k| text.contains(&k.to_ascii_lowercase()))
    };
    who_ok && room_ok && words_ok
}

impl ScriptedMind {
    /// A mind for the scripts of a cast.
    #[must_use]
    pub fn from_cast<'a>(cast: impl IntoIterator<Item = (&'a EntityId, &'a CastDef)>) -> Self {
        Self {
            rules: cast
                .into_iter()
                .filter(|(_, c)| !c.script.is_empty())
                .map(|(id, c)| (id.clone(), c.script.clone()))
                .collect(),
            fired: Mutex::new(HashMap::new()),
            scripts: None,
        }
    }

    /// The same mind, running the Rhai mind scripts of `host` for the
    /// entities that have one. `seed` seeds their dice with `mix`.
    #[must_use]
    pub fn with_scripts(mut self, host: Arc<ScriptHost>, game: Arc<Game>, seed: u64) -> Self {
        let version = (host.version(), host.reloads());
        self.scripts = Some(Scripts {
            host,
            game,
            seed,
            breakers: Mutex::new(HashMap::new()),
            seen: Mutex::new(version),
        });
        self
    }

    fn count(&self, me: &EntityId, idx: usize) -> u32 {
        self.fired
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(me.clone(), idx))
            .copied()
            .unwrap_or(0)
    }

    /// Moves the nth counters a plan used, once the plan runs.
    fn commit_turns(&self, me: &EntityId, turns: &Turns) {
        let mut fired = self.fired.lock().unwrap_or_else(PoisonError::into_inner);
        for (idx, n) in turns {
            fired.insert((me.clone(), *idx), *n);
        }
    }

    /// The commands a wake should run, in order, and the counters they
    /// would move. Reads the counters without changing them.
    fn plan_dry(&self, ctx: &WakeContext, my_room: Option<&str>) -> (Vec<String>, Turns) {
        let Some(rules) = self.rules.get(&ctx.entity) else {
            return (Vec::new(), Vec::new());
        };
        let mut out = Vec::new();
        let mut turns = Vec::new();
        for (idx, rule) in rules.iter().enumerate() {
            match rule.on {
                ScriptTrigger::MacroTick if ctx.macro_tick && !ctx.login => {
                    if rule.nth.is_none_or(|n| n == ctx.macro_ticks) {
                        out.extend(rule.commands.iter().cloned());
                    }
                }
                ScriptTrigger::Login if ctx.login => {
                    let n = self.count(&ctx.entity, idx) + 1;
                    turns.push((idx, n));
                    if rule.nth.is_none_or(|want| want == n) {
                        out.extend(rule.commands.iter().cloned());
                    }
                }
                ScriptTrigger::MacroTick | ScriptTrigger::Login => {}
                _ => {
                    let mut n = self.count(&ctx.entity, idx);
                    for ev in &ctx.events {
                        if !rule_matches_event(rule, ev, &ctx.entity, my_room) {
                            continue;
                        }
                        n += 1;
                        if rule.nth.is_none_or(|want| want == n) {
                            let actor = ev.actor.as_ref().map_or("you", |a| a.name.as_str());
                            out.extend(rule.commands.iter().map(|c| c.replace(ACTOR_SLOT, actor)));
                            // One answer per rule per wake.
                            break;
                        }
                    }
                    if n != self.count(&ctx.entity, idx) {
                        turns.push((idx, n));
                    }
                }
            }
        }
        (out, turns)
    }
}

impl Mind for ScriptedMind {
    fn kind(&self) -> &'static str {
        "scripted"
    }

    fn wake(
        &self,
        ctx: WakeContext,
        tools: MudTools,
    ) -> BoxFuture<'_, Result<MindReply, MindError>> {
        Box::pin(async move {
            let room = tools.look()["room_id"].as_str().map(str::to_owned);
            let (toml_plan, turns) = self.plan_dry(&ctx, room.as_deref());
            let scripted = self
                .scripts
                .as_ref()
                .filter(|s| s.host.has_mind(&ctx.entity));
            let decision = match scripted {
                Some(s) => s.decide(&ctx, &toml_plan, &tools).await,
                None => Decision::Toml(String::new()),
            };
            let (plan, why) = match decision {
                Decision::Lines(lines, why) => (lines, why),
                Decision::Toml(why) => {
                    self.commit_turns(&ctx.entity, &turns);
                    (toml_plan, why)
                }
            };
            let mut notes = Vec::new();
            if !why.is_empty() {
                notes.push(why);
            }
            for line in &plan {
                let r = tools.command(line).await;
                notes.push(format!("{line} -> {}", if r.ok { "ok" } else { "refused" }));
            }
            Ok(MindReply {
                text: (!notes.is_empty()).then(|| notes.join("; ")),
                usage: crate::mind::Usage::default(),
                commands: u32::try_from(plan.len()).unwrap_or(u32::MAX),
            })
        })
    }
}

/// The budget fallback: canned flavor for direct interactions, and
/// nothing else. It never calls a model.
#[derive(Debug, Default)]
pub struct FallbackMind {
    lines: HashMap<EntityId, (String, String)>,
}

impl FallbackMind {
    /// The fallback lines of a cast.
    #[must_use]
    pub fn from_cast<'a>(cast: impl IntoIterator<Item = (&'a EntityId, &'a CastDef)>) -> Self {
        Self {
            lines: cast
                .into_iter()
                .filter(|(_, c)| !c.fallback.is_empty())
                .map(|(id, c)| (id.clone(), (c.name.clone(), c.fallback.clone())))
                .collect(),
        }
    }

    /// The emote for an entity: its fallback line without its own name.
    #[must_use]
    pub fn emote_for(&self, entity: &EntityId) -> Option<String> {
        let (name, line) = self.lines.get(entity)?;
        let rest = line
            .strip_prefix(name.as_str())
            .map_or(line.as_str(), str::trim_start);
        Some(format!("emote {rest}"))
    }
}

impl Mind for FallbackMind {
    fn kind(&self) -> &'static str {
        "fallback"
    }

    fn wake(
        &self,
        ctx: WakeContext,
        tools: MudTools,
    ) -> BoxFuture<'_, Result<MindReply, MindError>> {
        Box::pin(async move {
            if ctx.priority != Priority::Immediate {
                return Ok(MindReply::default());
            }
            let Some(line) = self.emote_for(&ctx.entity) else {
                return Ok(MindReply::default());
            };
            let r = tools.command(&line).await;
            Ok(MindReply {
                text: Some(format!(
                    "fallback: {line} ({})",
                    if r.ok { "ok" } else { "refused" }
                )),
                usage: crate::mind::Usage::default(),
                commands: 1,
            })
        })
    }
}

#[cfg(test)]
mod rhai_tests;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mud_world::{Game, ManualClock, World, WorldPack};
    use serde_json::Value;

    use super::*;

    #[test]
    fn a_human_rule_answers_any_player_alike() {
        let rule: ScriptRule =
            toml::from_str("on = \"enter\"\nwho = \"human\"\ndo = [\"say hi\"]").unwrap();
        let alias: ScriptRule =
            toml::from_str("on = \"enter\"\nwho = \"player\"\ndo = [\"say hi\"]").unwrap();
        assert_eq!(rule.who, alias.who, "`player` is a synonym");
        let mut ev: Event = serde_json::from_value(serde_json::json!({
            "seq": 1, "at_ms": 0, "room": "rusty_flagon_taproom", "kind": "enter",
            "tags": [], "actor": { "id": "pc_kestrel", "name": "kestrel" },
            "narrative": "kestrel arrives.", "data": null, "recipients": [],
            "human_actor": false, "player_actor": true
        }))
        .unwrap();
        let me = EntityId::from("marta");
        assert!(
            rule_matches_event(&rule, &ev, &me, Some("rusty_flagon_taproom")),
            "an agent-played PC sets off a human rule, as a human does"
        );
        ev.player_actor = false;
        assert!(!rule_matches_event(
            &rule,
            &ev,
            &me,
            Some("rusty_flagon_taproom")
        ));
    }
    use crate::gate::Gate;

    fn setup() -> (Arc<Game>, Arc<Gate>, Arc<WorldPack>) {
        let pack = Arc::new(WorldPack::crypt_v01().unwrap());
        let game = Game::new(
            World::new(Arc::clone(&pack), 1),
            None,
            Arc::new(ManualClock::new(1_000)),
        );
        let typing = Gate::new(Arc::clone(&game), 1, 0.0);
        (game, typing, pack)
    }

    fn ctx(entity: &str, priority: Priority, events: Vec<Event>) -> WakeContext {
        WakeContext {
            entity: EntityId::from(entity),
            name: entity.to_owned(),
            priority,
            reason: "test".into(),
            events,
            event_views: Vec::new(),
            look: Value::Null,
            macro_tick: priority == Priority::MacroTick,
            macro_ticks: 1,
            login: false,
            wake_id: "w".into(),
            now_ms: 0,
            booked: Vec::new(),
        }
    }

    #[tokio::test]
    async fn torin_posts_the_bounty_on_the_first_macro_tick_only() {
        let (game, typing, pack) = setup();
        let mind = ScriptedMind::from_cast(&pack.cast);
        let tools = MudTools::new(Arc::clone(&game), typing, EntityId::from("torin"));
        let r = mind
            .wake(ctx("torin", Priority::MacroTick, vec![]), tools.clone())
            .await
            .unwrap();
        assert_eq!(r.commands, 5, "{r:?}");
        let board = game.read(|w| {
            w.state().rooms[&mud_world::RoomId::from("notice_market")]
                .board
                .clone()
        });
        assert_eq!(board.len(), 1);
        assert!(board[0].text.contains("15s a chunk"));
        assert_eq!(
            game.read(|w| w.entity(&EntityId::from("torin")).unwrap().room.to_string()),
            "smithy"
        );
        let mut second = ctx("torin", Priority::MacroTick, vec![]);
        second.macro_ticks = 2;
        let r = mind.wake(second, tools).await.unwrap();
        assert_eq!(r.commands, 0);
    }

    #[tokio::test]
    async fn kestrel_recruits_a_human_who_walks_in() {
        let (game, typing, pack) = setup();
        let k = EntityId::from("pc_kestrel");
        game.bring_online(&k);
        let mind = ScriptedMind::from_cast(&pack.cast);
        let ada = game.login("ada").unwrap();
        let mut rx = game.subscribe();
        game.execute(&ada, "e");
        let mut evs = Vec::new();
        while let Ok(e) = rx.try_recv() {
            evs.push((*e).clone());
        }
        let r = mind
            .wake(
                ctx("pc_kestrel", Priority::RoomBatch, evs),
                MudTools::new(Arc::clone(&game), typing, k.clone()),
            )
            .await
            .unwrap();
        assert_eq!(r.commands, 1);
        let heard = game.events(&ada, mud_world::Seq(0), 50);
        let said: Vec<&str> = heard["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["untrusted_text"].as_str())
            .collect();
        assert!(
            said.iter()
                .any(|t| t.contains("yo ada") && t.contains("50/50")),
            "{said:?}"
        );
    }

    #[tokio::test]
    async fn the_fallback_answers_only_direct_interactions() {
        let (game, typing, pack) = setup();
        let fb = FallbackMind::from_cast(&pack.cast);
        assert_eq!(
            fb.emote_for(&EntityId::from("torin")).as_deref(),
            Some("emote grunts and keeps hammering.")
        );
        let tools = MudTools::new(Arc::clone(&game), typing, EntityId::from("torin"));
        let r = fb
            .wake(ctx("torin", Priority::Ambient, vec![]), tools.clone())
            .await
            .unwrap();
        assert_eq!(r.commands, 0);
        let r = fb
            .wake(ctx("torin", Priority::Immediate, vec![]), tools)
            .await
            .unwrap();
        assert_eq!(r.commands, 1);
        assert_eq!(r.usage.tokens(), 0);
    }
}
