//! Mind scripts: a wake in, command lines out.
//!
//! The lines go through the supervisor's typing gate, the parser and
//! `World::execute`, as any mind's do. A script sees `kind` only, never a
//! tier or a mind; player text, and the notes of booked wakes, reach it
//! only as `Untrusted`.

use mud_world::EntityId;
use mud_world::hooks::WorldDice;
use mud_world::parser::{MAX_TEXT_CHARS, forbidden_in_text, parse_with};
use rhai::{Array, Dynamic, Map};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::convert::{from_dynamic, to_dynamic};
use crate::host::Compiled;
use crate::sandbox::{self, CallKind, CallState};
use crate::untrusted::Untrusted;
use crate::view::View;

/// The longest delay a mind script may book, in minutes (seven days).
pub const WAKE_MINUTES_MAX: i64 = 10_080;
/// The longest note of a booked wake.
pub const WAKE_NOTE_MAX: usize = 280;

/// Who an event names, as a mind script sees them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventActor {
    /// The id.
    pub id: EntityId,
    /// The name.
    pub name: String,
    /// `player`, `npc` or `mob`.
    pub kind: String,
}

/// One event, as a mind script sees it. Never its narrative or data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptEvent {
    /// The sequence number.
    pub seq: u64,
    /// The kind, such as `enter` or `speech`.
    pub kind: String,
    /// The verb that caused it.
    pub verb: Option<String>,
    /// Its tags.
    pub tags: Vec<String>,
    /// Where.
    pub room: Option<String>,
    /// Who did it.
    pub actor: Option<EventActor>,
    /// Who it was done to.
    pub target: Option<EventActor>,
    /// Whether the mind's own entity did it.
    pub is_self: bool,
    /// Whether it was done to the mind's own entity.
    pub at_me: bool,
    /// Player text, if any: the script gets it as `Untrusted`.
    #[serde(skip)]
    pub text: Option<String>,
}

/// One wake of a mind script.
#[derive(Debug, Clone)]
pub struct MindWake {
    /// Whose.
    pub entity: EntityId,
    /// `immediate`, `room_batch`, `ambient`, `macro_tick` or `booked`.
    pub priority: &'static str,
    /// Whether it delivers a macro-tick.
    pub macro_tick: bool,
    /// How many macro-ticks so far.
    pub macro_ticks: u32,
    /// Whether it is a login.
    pub login: bool,
    /// The game clock.
    pub now_ms: u64,
    /// The seed of the mind's dice.
    pub seed: u64,
    /// The entity's facts, without marks.
    pub me: Value,
    /// `look_view` of the entity.
    pub here: Value,
    /// The events of the wake.
    pub events: Vec<ScriptEvent>,
    /// Booked wakes that came due: id, due time, note.
    pub booked: Vec<(u64, u64, String)>,
    /// The lines the TOML rules would run.
    pub toml_plan: Vec<String>,
    /// The mind's memory.
    pub memory: Value,
}

/// What a mind script decided.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MindPlan {
    /// The command lines, in order.
    pub lines: Vec<String>,
    /// The new memory, if it returned one.
    pub memory: Option<Value>,
    /// A wake to book: minutes and a note.
    pub wake_me: Option<(u32, String)>,
}

/// What a mind call answered.
#[derive(Debug, Clone)]
pub struct MindReply {
    /// The plan, or why the call faulted.
    pub outcome: Result<MindPlan, String>,
    /// The script's log.
    pub log: Vec<String>,
}

fn type_name(d: &Dynamic) -> String {
    d.type_name()
        .rsplit("::")
        .next()
        .unwrap_or("value")
        .to_owned()
}

fn wake_ctx(w: &MindWake, config: &Value) -> Result<Dynamic, String> {
    let mut next_id = 0u32;
    let mut untrusted = |t: &str| {
        next_id += 1;
        Dynamic::from(Untrusted::new(next_id, t))
    };
    let mut events = Array::new();
    for e in w.events.iter().take(32) {
        let v = serde_json::to_value(e).map_err(|e| e.to_string())?;
        let mut m: Map = to_dynamic(&v)?.cast();
        m.insert("text".into(), untrusted(e.text.as_deref().unwrap_or("")));
        events.push(Dynamic::from_map(m));
    }
    let mut booked = Array::new();
    for (id, due, note) in &w.booked {
        let mut m = Map::new();
        m.insert(
            "id".into(),
            Dynamic::from_int(i64::try_from(*id).unwrap_or(i64::MAX)),
        );
        m.insert(
            "due_ms".into(),
            Dynamic::from_int(i64::try_from(*due).unwrap_or(i64::MAX)),
        );
        m.insert("note".into(), untrusted(note));
        booked.push(Dynamic::from_map(m));
    }
    let memory = if w.memory.is_object() {
        to_dynamic(&w.memory)?
    } else {
        Dynamic::from_map(Map::new())
    };
    let mut m = Map::new();
    m.insert(
        "hook".into(),
        Dynamic::from(format!("minds.{}.on_wake", w.entity)),
    );
    m.insert("priority".into(), Dynamic::from(w.priority.to_owned()));
    m.insert("macro_tick".into(), Dynamic::from_bool(w.macro_tick));
    m.insert(
        "macro_ticks".into(),
        Dynamic::from_int(i64::from(w.macro_ticks)),
    );
    m.insert("login".into(), Dynamic::from_bool(w.login));
    m.insert(
        "now_ms".into(),
        Dynamic::from_int(i64::try_from(w.now_ms).unwrap_or(i64::MAX)),
    );
    m.insert("me".into(), Dynamic::from(View::new(w.me.clone())));
    m.insert("here".into(), Dynamic::from(View::new(w.here.clone())));
    m.insert("events".into(), Dynamic::from_array(events));
    m.insert("booked".into(), Dynamic::from_array(booked));
    m.insert(
        "toml_plan".into(),
        Dynamic::from_array(
            w.toml_plan
                .iter()
                .map(|l| Dynamic::from(l.clone()))
                .collect(),
        ),
    );
    m.insert("memory".into(), memory);
    m.insert("config".into(), Dynamic::from(View::new(config.clone())));
    Ok(Dynamic::from_map(m))
}

impl Compiled {
    /// Whether `v` is a custom verb of this version.
    #[must_use]
    pub fn is_verb(&self, v: &str) -> bool {
        self.verbs.contains_key(v)
    }

    fn check_lines(&self, list: Array) -> Result<Vec<String>, String> {
        let max = usize::from(self.limits.max_lines_per_wake);
        if list.len() > max {
            return Err(format!("more than {max} lines in one wake"));
        }
        let mut out = Vec::new();
        for (i, d) in list.into_iter().enumerate() {
            if !d.is_string() {
                return Err(format!("line {i} is a {}, not a string", type_name(&d)));
            }
            let line = d.into_string().unwrap_or_default();
            if line.chars().count() > MAX_TEXT_CHARS {
                return Err(format!("line {i} is over {MAX_TEXT_CHARS} characters"));
            }
            if line.chars().any(forbidden_in_text) {
                return Err(format!("line {i} has a control or direction character"));
            }
            parse_with(&line, &|v| self.is_verb(v))
                .map_err(|e| format!("line {i} does not parse: {e}"))?;
            out.push(line);
        }
        Ok(out)
    }

    fn plan_of(&self, v: Dynamic) -> Result<MindPlan, String> {
        if v.is_unit() {
            return Ok(MindPlan::default());
        }
        if v.is_array() {
            return Ok(MindPlan {
                lines: self.check_lines(v.cast())?,
                ..MindPlan::default()
            });
        }
        if !v.is_map() {
            return Err(format!(
                "on_wake returned a {}; return lines, or a map of lines, memory and wake_me",
                type_name(&v)
            ));
        }
        let m: Map = v.cast();
        let mut plan = MindPlan::default();
        for (k, x) in m {
            match k.as_str() {
                "lines" => {
                    if !x.is_array() {
                        return Err(format!("lines is a {}, not an array", type_name(&x)));
                    }
                    plan.lines = self.check_lines(x.cast())?;
                }
                "memory" => plan.memory = Some(from_dynamic(&x)?),
                "wake_me" => {
                    if x.is_unit() {
                        continue;
                    }
                    if !x.is_map() {
                        return Err("wake_me is #{ minutes, note }".to_owned());
                    }
                    let w: Map = x.cast();
                    let minutes = w
                        .get("minutes")
                        .and_then(|d| d.as_int().ok())
                        .filter(|n| (1..=WAKE_MINUTES_MAX).contains(n))
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or_else(|| {
                            format!("wake_me.minutes must be 1 to {WAKE_MINUTES_MAX}")
                        })?;
                    let note = w
                        .get("note")
                        .filter(|d| d.is_string())
                        .and_then(|d| d.clone().into_string().ok())
                        .filter(|n| n.chars().count() <= WAKE_NOTE_MAX)
                        .ok_or_else(|| {
                            format!("wake_me.note must be a string of at most {WAKE_NOTE_MAX} characters")
                        })?;
                    plan.wake_me = Some((minutes, note));
                }
                other => return Err(format!("on_wake returned unknown key {other:?}")),
            }
        }
        Ok(plan)
    }

    /// Runs the mind script of `w.entity` for one wake.
    #[must_use]
    pub fn on_wake(&self, w: &MindWake) -> MindReply {
        let Some(entry) = self.minds.get(&w.entity) else {
            return MindReply {
                outcome: Err(format!("{} has no mind script", w.entity)),
                log: Vec::new(),
            };
        };
        let ctx = match wake_ctx(w, &entry.config) {
            Ok(c) => c,
            Err(e) => {
                return MindReply {
                    outcome: Err(e),
                    log: Vec::new(),
                };
            }
        };
        let state = CallState::new(CallKind::Mind, WorldDice::seeded(w.seed), &self.limits);
        let out = sandbox::call(&self.engine, &entry.ast, "on_wake", ctx, state);
        MindReply {
            log: out.state.finished_log(),
            outcome: out.result.and_then(|v| self.plan_of(v)),
        }
    }
}
