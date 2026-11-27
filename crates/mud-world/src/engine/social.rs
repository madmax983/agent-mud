//! Speech, the notice board, and skill attempts.

use std::fmt::Write as _;

use mud_rules::success_roll;
use serde_json::json;

use super::items::Lockable;
use super::{Ctx, Done, Emit, ErrorCode, Fail, StateChange, World};
use crate::event::EventKind;
use crate::ids::{EntityId, RoomId, SkillId};
use crate::pack::SkillEffect;
use crate::state::BoardPost;

impl World {
    pub(crate) fn say(&mut self, ctx: &mut Ctx, actor: &EntityId, text: &str) -> Done {
        let me = self.me(actor)?.clone();
        let spec = Emit::new(
            EventKind::Speech,
            Some(me.room.clone()),
            format!("{} says something.", me.name),
        )
        .actor(actor, "say")
        .text(text);
        let seq = self.emit(ctx, spec);
        Ok((format!("You say, \"{text}\""), json!({ "seq": seq })))
    }

    pub(crate) fn emote(&mut self, ctx: &mut Ctx, actor: &EntityId, text: &str) -> Done {
        let me = self.me(actor)?.clone();
        let spec = Emit::new(
            EventKind::Emote,
            Some(me.room.clone()),
            format!("{} does something.", me.name),
        )
        .actor(actor, "emote")
        .text(text);
        let seq = self.emit(ctx, spec);
        Ok((format!("{} {text}", me.name), json!({ "seq": seq })))
    }

    pub(crate) fn tell(&mut self, ctx: &mut Ctx, actor: &EntityId, word: &str, text: &str) -> Done {
        let target = self
            .state
            .entities
            .values()
            .find(|e| {
                &e.id != actor
                    && e.present()
                    && e.kind != crate::pack::EntityKind::Mob
                    && e.matches(word)
            })
            .map(|e| (e.id.clone(), e.name.clone()))
            .ok_or_else(|| {
                Fail::new(
                    ErrorCode::NotOnline,
                    format!("nobody called {word:?} is around"),
                )
            })?;
        let spec = Emit {
            only: Some(vec![actor.clone(), target.0.clone()]),
            ..Emit::new(
                EventKind::Tell,
                None,
                format!("{} sends a private message.", self.me(actor)?.name),
            )
            .actor(actor, "tell")
            .target(&target.0)
            .text(text)
        };
        let seq = self.emit(ctx, spec);
        Ok((
            format!("You tell {}, \"{text}\"", target.1),
            json!({ "seq": seq, "to": target.0 }),
        ))
    }

    fn require_board(&self, actor: &EntityId) -> Result<crate::ids::RoomId, Fail> {
        let room = self.me(actor)?.room.clone();
        if self.pack.rooms.get(&room).is_some_and(|r| r.board) {
            Ok(room)
        } else {
            Err(Fail::new(ErrorCode::NoBoard, "there is no board here"))
        }
    }

    pub(crate) fn board_read(&mut self, ctx: &mut Ctx, actor: &EntityId) -> Done {
        let room = self.require_board(actor)?;
        let posts = self
            .state
            .rooms
            .get(&room)
            .map(|r| r.board.clone())
            .unwrap_or_default();
        let name = self.me(actor)?.name.clone();
        let spec = Emit::new(
            EventKind::BoardRead,
            Some(room),
            format!("{name} studies the notice board."),
        )
        .actor(actor, "board_read");
        self.emit(ctx, spec);
        // The notices are player text: they travel only in `posts`, each in
        // its own `untrusted_text`, never in the narrative. A transport
        // shows them to a human as `<author.name>: <untrusted_text>`.
        let text = match posts.len() {
            0 => "The notice board: bare wood and old nail holes.".to_owned(),
            1 => "The notice board holds 1 notice.".to_owned(),
            n => format!("The notice board holds {n} notices."),
        };
        let rows: Vec<_> = posts
            .iter()
            .map(|p| json!({ "seq": p.seq, "author": p.author, "untrusted_text": p.text, "at_ms": p.at_ms }))
            .collect();
        Ok((text, json!({ "posts": rows })))
    }

    pub(crate) fn board_post(&mut self, ctx: &mut Ctx, actor: &EntityId, text: &str) -> Done {
        let room = self.require_board(actor)?;
        let me = self.me(actor)?.clone();
        let spec = Emit::new(
            EventKind::BoardPost,
            Some(room.clone()),
            format!("{} nails up a notice.", me.name),
        )
        .actor(actor, "board_post")
        .text(text);
        let seq = self.emit(ctx, spec);
        if let Some(r) = self.state.rooms.get_mut(&room) {
            r.board.push(BoardPost {
                seq,
                author: me.actor_ref(),
                text: text.to_owned(),
                at_ms: ctx.now,
            });
            // A board holds the last 20 notices.
            let excess = r.board.len().saturating_sub(20);
            r.board.drain(..excess);
        }
        ctx.changes.push(StateChange::BoardPost { room, seq });
        Ok((
            "You nail your notice to the board.".to_owned(),
            json!({ "seq": seq }),
        ))
    }

    pub(crate) fn attempt(
        &mut self,
        ctx: &mut Ctx,
        actor: &EntityId,
        skill: &str,
        on: Option<&str>,
    ) -> Done {
        let sid = SkillId::new(skill);
        let def = self.pack.skills.get(&sid).cloned().ok_or_else(|| {
            Fail::new(ErrorCode::UnknownSkill, format!("no such skill {skill:?}"))
        })?;
        let me = self.me(actor)?.clone();
        let base = self.skill_level(&me, &sid).unwrap_or(0);
        let mut modifier = 0;
        let mut lockable = None;
        let mut target_entity = None;
        match (def.effect, on) {
            (Some(SkillEffect::PickLock), Some(w)) => {
                let (l, m) = self.pick_target(actor, &me.room, w)?;
                modifier = m;
                lockable = Some(l);
            }
            (Some(SkillEffect::PickLock), None) => {
                return Err(Fail::new(
                    ErrorCode::BadArgs,
                    "usage: attempt lockpicking on <lock>",
                ));
            }
            (_, Some(w)) => {
                target_entity = Some(self.find_here(actor, w).ok_or_else(|| {
                    Fail::new(ErrorCode::NoSuchTarget, format!("you see no {w:?} here"))
                })?);
            }
            (_, None) => {}
        }
        let roll = success_roll(&mut self.dice, base + modifier);
        let ok = roll.outcome.is_success();
        let mut text = format!(
            "You attempt {} (roll {} vs {}): {}.",
            def.name,
            roll.total,
            roll.target,
            if ok { "success" } else { "failure" }
        );
        let mut tags = vec!["attempt".to_owned(), skill.to_owned()];
        if def.effect == Some(SkillEffect::Haggle) {
            tags.push("haggle".to_owned());
        }
        let (more, extra) =
            self.skill_effect(ctx, actor, def.effect, ok, lockable, target_entity.as_ref());
        text.push_str(&more);
        // A hidden success stays quiet.
        let quiet = def.effect == Some(SkillEffect::Hide) && ok;
        if !quiet {
            let narrative = match &target_entity {
                Some(t) => format!(
                    "{} tries {} on {} and {}.",
                    me.name,
                    def.name,
                    self.state
                        .entities
                        .get(t)
                        .map_or("someone", |e| e.name.as_str()),
                    if ok { "succeeds" } else { "fails" }
                ),
                None => format!(
                    "{} tries {} and {}.",
                    me.name,
                    def.name,
                    if ok { "succeeds" } else { "fails" }
                ),
            };
            let mut spec = Emit::new(crate::event::EventKind::Attempt, Some(me.room), narrative)
                .actor(actor, "attempt")
                .tags(tags)
                .data(json!({ "skill": skill, "outcome": roll.outcome, "margin": roll.margin }));
            if let Some(t) = &target_entity {
                spec = spec.target(t);
            }
            self.emit(ctx, spec);
        }
        if def.combat
            || target_entity
                .as_ref()
                .is_some_and(|t| self.state.entities.get(t).is_some_and(|e| e.hostile))
        {
            self.mob_round(ctx, actor);
        }
        Ok((
            text,
            json!({ "roll": roll, "effect": def.effect, "result": extra }),
        ))
    }

    /// The lock `word` names for a lockpicking attempt, and its modifier.
    /// A lock that no pick opens is refused here, before any roll: no
    /// try, no turn and no lucky critical.
    fn pick_target(
        &self,
        actor: &EntityId,
        room: &RoomId,
        word: &str,
    ) -> Result<(Lockable, i32), Fail> {
        let l = self.find_lockable(actor, word).ok_or_else(|| {
            Fail::new(
                ErrorCode::NoSuchTarget,
                format!("there is no lock on {word:?} here"),
            )
        })?;
        match l {
            Lockable::Exit(_, m) | Lockable::Container(_, m) => Ok((l, m)),
            Lockable::Sealed(d) => {
                let lock = self
                    .pack
                    .rooms
                    .get(room)
                    .and_then(|r| r.locks.get(&d))
                    .map_or(word, |l| l.lock.as_str());
                Err(Fail::new(
                    ErrorCode::Refused,
                    format!("no pick can open the {lock}"),
                ))
            }
        }
    }

    /// Applies the engine effect of a skill roll. Returns the extra text
    /// for the actor and any structured result.
    fn skill_effect(
        &mut self,
        ctx: &mut Ctx,
        actor: &EntityId,
        effect: Option<SkillEffect>,
        ok: bool,
        lockable: Option<Lockable>,
        target_entity: Option<&EntityId>,
    ) -> (String, serde_json::Value) {
        let mut text = String::new();
        let mut extra = json!(null);
        let Some(me) = self.state.entities.get(actor).cloned() else {
            return (text, extra);
        };
        match (effect, ok) {
            (Some(SkillEffect::PickLock), true) => match lockable {
                Some(Lockable::Exit(dir, _)) => {
                    self.unlock_exit(ctx, actor, &me.room, dir, "pick");
                    let _ = write!(text, " The lock gives. The way {} is open.", dir.name());
                }
                Some(Lockable::Container(item, _)) => {
                    extra = self.unlock_container(ctx, item);
                    let _ = write!(text, " The {} clicks open.", self.item_name(item));
                }
                Some(Lockable::Sealed(_)) | None => {}
            },
            (Some(SkillEffect::Hide), true) => {
                if let Some(e) = self.state.entities.get_mut(actor) {
                    e.hidden = true;
                }
                ctx.changes.push(StateChange::Status {
                    entity: actor.clone(),
                    status: "hidden".into(),
                    value: true,
                });
                text.push_str(" You melt into the shadows.");
            }
            (Some(SkillEffect::Haggle), _) => {
                if ok
                    && let Some(t) = target_entity
                    && self.state.entities.get(t).is_some_and(|e| e.shop.is_some())
                {
                    if let Some(e) = self.state.entities.get_mut(actor) {
                        e.haggle_with = Some(t.clone());
                    }
                    text.push_str(" You talk the price down: 10% off your next purchase.");
                }
            }
            _ => {}
        }
        (text, extra)
    }
}
