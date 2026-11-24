//! Deterministic minds: cast scripts and the budget fallback.
//!
//! The scripted mind runs the `script` rules of `cast.toml`. It needs no
//! model and no key, so the server runs out of the box, the smoke test is
//! repeatable, and a budget-capped entity still behaves like itself.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use mud_world::pack::{CastDef, ScriptRule, ScriptTrigger, Who};
use mud_world::{EntityId, Event, EventKind};

use crate::mind::{BoxFuture, Mind, MindError, MindReply, MudTools, WakeContext};
use crate::router::Priority;

/// The placeholder a script fills with the triggering entity's name.
const ACTOR_SLOT: &str = concat!("{", "actor", "}");

/// Runs the cast's script rules.
#[derive(Debug, Default)]
pub struct ScriptedMind {
    rules: HashMap<EntityId, Vec<ScriptRule>>,
    fired: Mutex<HashMap<(EntityId, usize), u32>>,
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
        }
    }

    fn take_turn(&self, me: &EntityId, idx: usize, rule: &ScriptRule) -> bool {
        let mut fired = self.fired.lock().unwrap_or_else(PoisonError::into_inner);
        let count = fired.entry((me.clone(), idx)).or_default();
        *count += 1;
        let n = *count;
        drop(fired);
        rule.nth.is_none_or(|want| want == n)
    }

    /// The commands a wake should run, in order.
    fn plan(&self, ctx: &WakeContext, my_room: Option<&str>) -> Vec<String> {
        let Some(rules) = self.rules.get(&ctx.entity) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (idx, rule) in rules.iter().enumerate() {
            match rule.on {
                ScriptTrigger::MacroTick if ctx.macro_tick && !ctx.login => {
                    if rule.nth.is_none_or(|n| n == ctx.macro_ticks) {
                        out.extend(rule.commands.iter().cloned());
                    }
                }
                ScriptTrigger::Login if ctx.login => {
                    if self.take_turn(&ctx.entity, idx, rule) {
                        out.extend(rule.commands.iter().cloned());
                    }
                }
                ScriptTrigger::MacroTick | ScriptTrigger::Login => {}
                _ => {
                    for ev in &ctx.events {
                        if rule_matches_event(rule, ev, &ctx.entity, my_room)
                            && self.take_turn(&ctx.entity, idx, rule)
                        {
                            let actor = ev.actor.as_ref().map_or("you", |a| a.name.as_str());
                            out.extend(rule.commands.iter().map(|c| c.replace(ACTOR_SLOT, actor)));
                            // One answer per rule per wake.
                            break;
                        }
                    }
                }
            }
        }
        out
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
            let plan = self.plan(&ctx, room.as_deref());
            let mut notes = Vec::new();
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
