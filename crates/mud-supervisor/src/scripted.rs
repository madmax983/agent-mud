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
//!
//! A plan persists with its cursor (ADR 0011): the mind stages the plan on
//! its turn, then, before each line, the cursor past that line, so the
//! cursor commits in the transaction of the line's own effect. After a
//! crash the [`PlanMind`] resumes the plan at its durable cursor, and each
//! effectful line takes effect exactly once. The `nth` counters of event
//! and login rules persist too, keyed by a digest of each rule's text.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use mud_script::{EventActor, MindWake, ScriptEvent, ScriptHost};
use mud_world::hooks::{HookOutcome, hook_allowed};
use mud_world::pack::{CastDef, EntityKind, ScriptRule, ScriptTrigger, Who};
use mud_world::side::RuleCountRow;
use mud_world::{Audit, EntityId, Event, EventKind, Game, SideOp, World};

use crate::mind::{BoxFuture, Mind, MindError, MindReply, MudTools, WakeContext};
use crate::persist::plan_step;
use crate::router::{Priority, Resume};
use crate::turing::mix;

/// The placeholder a script fills with the triggering entity's name.
const ACTOR_SLOT: &str = concat!("{", "actor", "}");

/// The nth counters a plan would move: rule index and its new count.
type Turns = Vec<(usize, u32)>;

/// Runs the cast's script rules, and Rhai mind scripts when it has a host.
#[derive(Debug, Default)]
pub struct ScriptedMind {
    rules: HashMap<EntityId, Vec<ScriptRule>>,
    /// The persisted key of each rule, by index: its digest and its copy
    /// number among identical rules.
    keys: HashMap<EntityId, Vec<(u64, u32)>>,
    fired: Mutex<HashMap<(EntityId, usize), u32>>,
    scripts: Option<Scripts>,
}

/// The digest of a rule's canonical text: a rule inserted above another
/// leaves its count alone, and an edited rule counts from 0.
#[must_use]
pub fn rule_digest(rule: &ScriptRule) -> u64 {
    let text = toml::to_string(rule).unwrap_or_default();
    mix(0, &[b"script-rule", text.as_bytes()])
}

/// The keys of a list of rules: each rule's digest, and how many identical
/// rules come before it.
fn rule_keys(rules: &[ScriptRule]) -> Vec<(u64, u32)> {
    let mut seen: HashMap<u64, u32> = HashMap::new();
    rules
        .iter()
        .map(|r| {
            let d = rule_digest(r);
            let dup = seen.entry(d).or_insert(0);
            let key = (d, *dup);
            *dup += 1;
            key
        })
        .collect()
}

/// Runs `lines[start..]` for `tools`' entity, staging before each line the
/// cursor past it on the turn of `turn_id`: the cursor commits with the
/// line's own effect, so a resume never repeats an effect. Returns a note
/// per line and the durable cursor the run reached.
async fn run_plan(
    tools: &MudTools,
    turn_id: &str,
    lines: &[String],
    start: u32,
) -> (Vec<String>, u32) {
    let mut notes = Vec::new();
    let mut cursor = start;
    for (i, line) in lines.iter().enumerate().skip(start as usize) {
        let Ok(at) = u32::try_from(i) else { break };
        let staged = tools.stage(
            turn_id,
            vec![SideOp::PlanCursor {
                entity: tools.entity().clone(),
                wake_id: turn_id.to_owned(),
                cursor: at.saturating_add(1),
            }],
        );
        if let Err(e) = staged {
            tracing::warn!(entity = %tools.entity(), error = %e, "a plan line runs without its cursor");
        }
        let r = tools.command(line).await;
        cursor = plan_step(cursor, at, r.ok && !r.read_only);
        notes.push(format!("{line} -> {}", if r.ok { "ok" } else { "refused" }));
    }
    (notes, cursor)
}

/// Finishes a plan after a restart: runs its lines from its durable cursor
/// through the gate, as the mind that made it would have. It calls no
/// model, so it costs no budget and runs even at the hard cap.
#[derive(Debug)]
pub struct PlanMind {
    plan: Resume,
}

impl PlanMind {
    /// The mind that finishes `plan`.
    #[must_use]
    pub const fn new(plan: Resume) -> Self {
        Self { plan }
    }
}

impl Mind for PlanMind {
    fn kind(&self) -> &'static str {
        "plan"
    }

    fn wake(
        &self,
        _ctx: WakeContext,
        tools: MudTools,
    ) -> BoxFuture<'_, Result<MindReply, MindError>> {
        Box::pin(async move {
            let p = &self.plan;
            let (mut notes, _) = run_plan(&tools, &p.wake_id, &p.lines, p.cursor).await;
            let ran = notes.len();
            notes.insert(0, format!("resumed at line {}", p.cursor + 1));
            Ok(MindReply {
                text: Some(notes.join("; ")),
                usage: crate::mind::Usage::default(),
                commands: u32::try_from(ran).unwrap_or(u32::MAX),
            })
        })
    }
}

/// The Rhai side of a scripted mind. The mind breakers live in the host
/// (in memory: a restart, a reload and a re-arm reopen them), where the
/// observer reads them.
#[derive(Debug)]
struct Scripts {
    host: Arc<ScriptHost>,
    game: Arc<Game>,
    seed: u64,
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
    async fn decide(&self, ctx: &WakeContext, toml_plan: &[String], tools: &MudTools) -> Decision {
        let e = &ctx.entity;
        let since = self.host.reloads();
        let compiled = self.host.current();
        let max = compiled.limits.max_consecutive_faults;
        let faults = self.host.mind_faults(e);
        if !hook_allowed(faults, max) {
            return Decision::Toml(format!("rhai tripped ({faults}/{max}); ran the TOML plan"));
        }
        let schema = self.host.mind_memory_schema(e);
        let (resets, memory) = self.game.script_memory_at(e, schema);
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
                self.host.step_mind(e, since, HookOutcome::Ran);
                // The memory and the booking ride on the wake's turn: they
                // commit with the plan's first effect, or at the close, so
                // a crash before either leaves no half of the decision.
                // An operator's reset during the wake wins over this save.
                if let Some(m) = plan.memory.filter(|m| *m != memory)
                    && !self
                        .game
                        .stage_script_memory_if(e, &ctx.wake_id, schema, resets, m)
                {
                    tracing::info!(entity = %e, "memory reset during the wake; not saved");
                }
                if let Some((minutes, note)) = &plan.wake_me {
                    let booked = tools.wake_me_on_turn(&ctx.wake_id, *minutes, note);
                    if booked["ok"] != true {
                        tracing::info!(entity = %e, answer = %booked, "a mind script's wake was not booked");
                    }
                }
                let n = plan.lines.len();
                Decision::Lines(plan.lines, format!("rhai: {n} lines"))
            }
            Err(message) => {
                // A fault from before a reload counts for nothing.
                let (next, allowed) = self
                    .host
                    .step_mind(e, since, HookOutcome::Faulted)
                    .unwrap_or_else(|| {
                        let now = self.host.mind_faults(e);
                        (now, hook_allowed(now, max))
                    });
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
        let rules: HashMap<EntityId, Vec<ScriptRule>> = cast
            .into_iter()
            .filter(|(_, c)| !c.script.is_empty())
            .map(|(id, c)| (id.clone(), c.script.clone()))
            .collect();
        let keys = rules
            .iter()
            .map(|(id, r)| (id.clone(), rule_keys(r)))
            .collect();
        Self {
            rules,
            keys,
            fired: Mutex::new(HashMap::new()),
            scripts: None,
        }
    }

    /// The same mind, running the Rhai mind scripts of `host` for the
    /// entities that have one. `seed` seeds their dice with `mix`.
    #[must_use]
    pub fn with_scripts(mut self, host: Arc<ScriptHost>, game: Arc<Game>, seed: u64) -> Self {
        self.scripts = Some(Scripts { host, game, seed });
        self
    }

    pub(crate) fn count(&self, me: &EntityId, idx: usize) -> u32 {
        self.fired
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(me.clone(), idx))
            .copied()
            .unwrap_or(0)
    }

    /// Moves the nth counters a plan used, once the plan runs, and returns
    /// them as rows to persist.
    fn commit_turns(&self, me: &EntityId, turns: &Turns) -> Vec<RuleCountRow> {
        let keys = self.keys.get(me);
        let rows = turns
            .iter()
            .filter_map(|&(idx, count)| {
                let &(digest, dup) = keys?.get(idx)?;
                Some(RuleCountRow { digest, dup, count })
            })
            .collect();
        let mut fired = self.fired.lock().unwrap_or_else(PoisonError::into_inner);
        for (idx, n) in turns {
            fired.insert((me.clone(), *idx), *n);
        }
        drop(fired);
        rows
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

    /// A count whose rule is gone or was edited matches no rule: it is
    /// stale, and returned to be dropped.
    fn restore(&self, entity: &EntityId, counts: &[RuleCountRow]) -> Vec<RuleCountRow> {
        let keys = self.keys.get(entity).map_or(&[][..], Vec::as_slice);
        let mut fired = self.fired.lock().unwrap_or_else(PoisonError::into_inner);
        for (idx, (digest, dup)) in keys.iter().enumerate() {
            if let Some(row) = counts.iter().find(|r| r.digest == *digest && r.dup == *dup) {
                fired.insert((entity.clone(), idx), row.count);
            }
        }
        drop(fired);
        counts
            .iter()
            .filter(|r| !keys.contains(&(r.digest, r.dup)))
            .copied()
            .collect()
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
            let mut staged = Vec::new();
            let (plan, why) = match decision {
                Decision::Lines(lines, why) => (lines, why),
                Decision::Toml(why) => {
                    let rows = self.commit_turns(&ctx.entity, &turns);
                    if !rows.is_empty() {
                        staged.push(SideOp::RuleCounts {
                            entity: ctx.entity.clone(),
                            rows,
                        });
                    }
                    (toml_plan, why)
                }
            };
            if !plan.is_empty() {
                staged.push(SideOp::PlanSet {
                    entity: ctx.entity.clone(),
                    wake_id: ctx.wake_id.clone(),
                    lines: plan.clone(),
                    errand: ctx.macro_tick || ctx.login || !ctx.booked.is_empty(),
                    started_ms: ctx.now_ms,
                });
            }
            if !staged.is_empty()
                && let Err(e) = tools.stage(&ctx.wake_id, staged)
            {
                tracing::warn!(entity = %ctx.entity, error = %e, "a plan runs without its record");
            }
            let mut notes = Vec::new();
            if !why.is_empty() {
                notes.push(why);
            }
            let (ran, _) = run_plan(&tools, &ctx.wake_id, &plan, 0).await;
            notes.extend(ran);
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
            you: serde_json::Value::Null,
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
