//! Movement and combat: move, flee, attack, defend, wounds and death.

use std::sync::Arc;

use mud_rules::{
    AttackInput, DamageType, DefenseKind, WoundState, basic_dodge, defense_from_skill,
    resolve_attack, success_roll,
};
use serde_json::{Value, json};

use super::{Ctx, Done, Emit, ErrorCode, Fail, StateChange, World};
use crate::event::EventKind;
use crate::ids::{EntityId, RoomId, SkillId};
use crate::pack::{Direction, EntityKind, Slot, WeaponDamage};
use crate::state::{Entity, Holder};

/// How long an attack counts as provocation for `no_first_strike`, in ms.
pub const PROVOKED_MS: u64 = 300_000;

impl World {
    /// The weapon an entity fights with: damage, type and skill.
    fn weapon(&self, e: &Entity) -> (WeaponDamage, DamageType, SkillId) {
        e.equipped
            .get(&Slot::Weapon)
            .and_then(|i| self.state.items.get(i))
            .and_then(|it| self.pack.items.get(&it.proto))
            .and_then(|d| {
                let (dmg, kind) = d.weapon_damage()?;
                Some((dmg, kind, d.skill.clone()?))
            })
            .unwrap_or_else(|| {
                (
                    WeaponDamage::Thrust(-1),
                    DamageType::Crushing,
                    SkillId::from("brawling"),
                )
            })
    }

    pub(crate) fn armor_dr(&self, e: &Entity) -> i32 {
        e.dr + e
            .equipped
            .values()
            .filter_map(|i| self.state.items.get(i))
            .filter_map(|it| self.pack.items.get(&it.proto))
            .map(|d| d.dr)
            .sum::<i32>()
    }

    fn best_defense(&self, e: &Entity) -> Option<(DefenseKind, i32)> {
        if e.unconscious {
            return None;
        }
        let bonus = if e.defending { 2 } else { 0 };
        let dodge = basic_dodge(&e.attrs, e.pools.wound_state());
        let (_, _, skill) = self.weapon(e);
        let parry = self
            .pack
            .skills
            .get(&skill)
            .filter(|s| s.combat)
            .and_then(|_| self.skill_level(e, &skill))
            .map(defense_from_skill);
        match parry {
            Some(p) if p > dodge => Some((DefenseKind::Parry, p + bonus)),
            _ => Some((DefenseKind::Dodge, dodge + bonus)),
        }
    }

    /// The live hostile mobs in a room, other than `except`.
    fn hostiles(&self, room: &RoomId, except: &EntityId) -> Vec<EntityId> {
        self.present_in(room)
            .filter(|e| e.hostile && e.can_act() && &e.id != except)
            .map(|e| e.id.clone())
            .collect()
    }

    /// One attack from `attacker` on `target`, with all its consequences.
    pub(crate) fn strike(
        &mut self,
        ctx: &mut Ctx,
        attacker: &EntityId,
        target: &EntityId,
    ) -> Value {
        let (Some(a), Some(t)) = (
            self.state.entities.get(attacker).cloned(),
            self.state.entities.get(target).cloned(),
        ) else {
            return Value::Null;
        };
        let (dmg, kind, skill) = self.weapon(&a);
        let input = AttackInput {
            attack_skill: self.skill_level(&a, &skill).unwrap_or(a.attrs.dx - 4),
            defense: self.best_defense(&t),
            damage: dmg.dice(a.attrs.st),
            kind,
            dr: self.armor_dr(&t),
        };
        let res = resolve_attack(&mut self.dice, &input);
        if let Some(te) = self.state.entities.get_mut(target) {
            te.last_attacked_by = Some((attacker.clone(), ctx.now));
        }
        let narrative = if res.hit {
            format!(
                "{} hits {} ({} {}).",
                a.name,
                t.name,
                res.injury,
                kind.tag()
            )
        } else if let Some((d, _)) = res.defense {
            let verb = match d {
                DefenseKind::Dodge => "dodges",
                DefenseKind::Parry => "parries",
                DefenseKind::Block => "blocks",
            };
            format!("{} attacks {}, who {verb}.", a.name, t.name)
        } else {
            format!("{} attacks {} and misses.", a.name, t.name)
        };
        let resolution = json!({ "attack": res, "attacker": attacker, "target": target });
        let spec = Emit::new(EventKind::Combat, Some(a.room), narrative)
            .actor(attacker, "attack")
            .target(target)
            .tags(["combat".to_owned()])
            .data(json!({ "resolution": res }));
        self.emit(ctx, spec);
        if res.hit && res.injury > 0 {
            self.wound(ctx, target, res.injury, attacker);
        }
        resolution
    }

    /// Applies injury: HP, consciousness and survival rolls, death.
    pub(crate) fn wound(&mut self, ctx: &mut Ctx, target: &EntityId, injury: i32, by: &EntityId) {
        let Some(t) = self.state.entities.get_mut(target) else {
            return;
        };
        let before = t.pools.wound_state();
        t.pools.hp -= injury;
        let hp = t.pools.hp;
        let after = t.pools.wound_state();
        let (ht, hp_max) = (t.attrs.ht, t.pools.hp_max);
        ctx.changes.push(StateChange::Hp {
            entity: target.clone(),
            delta: -injury,
            hp,
        });
        let mut dead = after == WoundState::Dead;
        if !dead && after >= WoundState::Dying {
            let r = success_roll(&mut self.dice, ht);
            dead = !r.outcome.is_success();
        }
        if dead {
            self.kill(ctx, target, by);
            return;
        }
        if after >= WoundState::Reeling && !self.state.entities[target].unconscious {
            let penalty = (-hp) / hp_max.max(1);
            let r = success_roll(&mut self.dice, ht - penalty);
            if !r.outcome.is_success() {
                if let Some(t) = self.state.entities.get_mut(target) {
                    t.unconscious = true;
                }
                ctx.changes.push(StateChange::Status {
                    entity: target.clone(),
                    status: "unconscious".into(),
                    value: true,
                });
                let t = &self.state.entities[target];
                let spec = Emit::new(
                    EventKind::Combat,
                    Some(t.room.clone()),
                    format!("{} collapses.", t.name),
                )
                .actor(target, "collapse")
                .tags(["combat".to_owned()]);
                self.emit(ctx, spec);
            }
        } else if before < WoundState::Staggered && after == WoundState::Staggered {
            let t = &self.state.entities[target];
            let spec = Emit::new(
                EventKind::Combat,
                Some(t.room.clone()),
                format!("{} staggers, badly hurt.", t.name),
            )
            .actor(target, "stagger")
            .tags(["combat".to_owned()]);
            self.emit(ctx, spec);
        }
    }

    fn kill(&mut self, ctx: &mut Ctx, victim: &EntityId, by: &EntityId) {
        let Some(v) = self.state.entities.get(victim).cloned() else {
            return;
        };
        let spec = Emit::new(
            EventKind::Death,
            Some(v.room.clone()),
            format!("{} dies.", v.name),
        )
        .actor(victim, "die")
        .target(by)
        .tags(["combat".to_owned(), "death".to_owned()]);
        self.emit(ctx, spec);
        ctx.changes.push(StateChange::Status {
            entity: victim.clone(),
            status: "dead".into(),
            value: true,
        });
        if let Some(e) = self.state.entities.get_mut(victim) {
            e.deaths = e.deaths.saturating_add(1);
        }
        if v.kind == EntityKind::Mob {
            for item in v.inventory.clone() {
                let hidden = self
                    .item_proto(item)
                    .and_then(|p| self.pack.items.get(&p))
                    .is_some_and(|d| d.hidden);
                if !hidden {
                    self.relocate_item(
                        ctx,
                        item,
                        Holder::Room {
                            room: v.room.clone(),
                        },
                    );
                }
            }
            let delay = v
                .respawn_minutes
                .map_or(u64::MAX, |m| ctx.now.saturating_add(u64::from(m) * 60_000));
            if let Some(e) = self.state.entities.get_mut(victim) {
                e.dead_until_ms = Some(delay);
                e.unconscious = false;
            }
            return;
        }
        // People wake at the respawn room with their gear, with the share
        // of their HP the pack gives back (`[death]`), and maybe without
        // their purse, which stays where they fell.
        if self.pack.death.drop_purse {
            self.drop_purse(ctx, victim, &v.room);
        }
        let to = self.pack.world.respawn_room.clone();
        let frac = self.pack.death.clone();
        if let Some(e) = self.state.entities.get_mut(victim) {
            e.pools = mud_rules::Pools::full(&e.attrs);
            e.pools.hp = frac.respawn_hp(e.pools.hp_max);
            e.unconscious = false;
            e.room = to.clone();
            ctx.changes.push(StateChange::Hp {
                entity: victim.clone(),
                delta: e.pools.hp - v.pools.hp,
                hp: e.pools.hp,
            });
        }
        ctx.changes.push(StateChange::Moved {
            entity: victim.clone(),
            from: v.room.clone(),
            to: to.clone(),
        });
        let spec = Emit::new(
            EventKind::Enter,
            Some(to),
            format!(
                "{} staggers in, pale and shaking, like someone who has seen the other side.",
                v.name
            ),
        )
        .actor(victim, "respawn");
        self.emit(ctx, spec);
    }

    /// Moves everything `victim` carries in its purse into one purse item
    /// (`[death] purse_item`) on the floor of `room`. The coins move, so
    /// the world's money stays what it was.
    fn drop_purse(&mut self, ctx: &mut Ctx, victim: &EntityId, room: &RoomId) {
        let (Some(proto), Some(n)) = (
            self.pack.death.purse_item.clone(),
            self.state
                .entities
                .get(victim)
                .map(|e| e.currency)
                .filter(|n| *n > 0),
        ) else {
            return;
        };
        let pack = Arc::clone(&self.pack);
        let purse = self.state.new_item(&proto, &pack);
        if let Some(it) = self.state.items.get_mut(&purse) {
            it.currency = Some(n);
        }
        self.change_currency(ctx, victim, -n);
        let to = Holder::Room { room: room.clone() };
        self.state.attach(purse, &to);
        ctx.changes.push(StateChange::Item {
            item: purse,
            proto,
            from: None,
            to: Some(to),
        });
        let (name, purse_name) = (
            self.state
                .entities
                .get(victim)
                .map_or_else(String::new, |e| e.name.clone()),
            self.item_name(purse),
        );
        let spec = Emit::new(
            EventKind::ItemDrop,
            Some(room.clone()),
            format!("{name}'s {purse_name} falls where {name} fell."),
        )
        .target(victim)
        .tags(["loot".to_owned()])
        .data(json!({ "amount": n }));
        self.emit(ctx, spec);
    }

    /// Each hostile mob in the victim's room attacks it once.
    pub(crate) fn mob_round(&mut self, ctx: &mut Ctx, victim: &EntityId) -> Vec<Value> {
        let Some(v) = self.state.entities.get(victim) else {
            return Vec::new();
        };
        if v.kind == EntityKind::Mob || !v.present() {
            return Vec::new();
        }
        let room = v.room.clone();
        let mut out = Vec::new();
        for mob in self.hostiles(&room, victim) {
            let still_here = self
                .state
                .entities
                .get(victim)
                .is_some_and(|v| v.present() && v.room == room && v.dead_until_ms.is_none());
            if !still_here {
                break;
            }
            out.push(self.strike(ctx, &mob, victim));
        }
        out
    }

    pub(crate) fn walk(
        &mut self,
        ctx: &mut Ctx,
        actor: &EntityId,
        dir: Direction,
        fleeing: bool,
    ) -> Done {
        let me = self.me(actor)?.clone();
        let def = self
            .pack
            .rooms
            .get(&me.room)
            .ok_or_else(|| Fail::new(ErrorCode::NoExit, "you are nowhere"))?;
        let to =
            def.exits.get(&dir).cloned().ok_or_else(|| {
                Fail::new(ErrorCode::NoExit, format!("you cannot go {}", dir.name()))
            })?;
        if !fleeing && me.kind != EntityKind::Mob && !self.hostiles(&me.room, actor).is_empty() {
            return Err(Fail::new(
                ErrorCode::InCombat,
                "something hostile blocks you; flee instead",
            ));
        }
        let opened = if self.exit_is_locked(&me.room, dir) {
            let lock = def
                .locks
                .get(&dir)
                .map(|l| l.lock.clone())
                .unwrap_or_default();
            let key = me.inventory.iter().copied().find(|i| {
                self.item_proto(*i)
                    .and_then(|p| self.pack.items.get(&p))
                    .and_then(|d| d.key.clone())
                    .is_some_and(|k| k == lock)
            });
            if key.is_none() {
                let what = def
                    .locks
                    .get(&dir)
                    .and_then(|l| l.describe.clone())
                    .unwrap_or_else(|| "It is locked.".into());
                return Err(Fail::new(ErrorCode::ExitLocked, what));
            }
            self.unlock_exit(ctx, actor, &me.room.clone(), dir, "key");
            Some(dir)
        } else {
            None
        };
        if !me.hidden {
            let spec = Emit::new(
                EventKind::Leave,
                Some(me.room.clone()),
                format!("{} {}.", me.name, dir.leaving()),
            )
            .actor(actor, if fleeing { "flee" } else { "move" })
            .data(json!({ "dir": dir.short() }));
            self.emit(ctx, spec);
        }
        if let Some(e) = self.state.entities.get_mut(actor) {
            e.room = to.clone();
        }
        ctx.changes.push(StateChange::Moved {
            entity: actor.clone(),
            from: me.room.clone(),
            to: to.clone(),
        });
        if !me.hidden {
            let from_dir = dir.opposite();
            let spec = Emit::new(
                EventKind::Enter,
                Some(to),
                format!("{} {}.", me.name, from_dir.arriving_from()),
            )
            .actor(actor, if fleeing { "flee" } else { "move" })
            .data(json!({ "from": from_dir.short() }));
            self.emit(ctx, spec);
        }
        let ambush = if me.hidden {
            Vec::new()
        } else {
            self.mob_round(ctx, actor)
        };
        let (mut text, view) = self.look(actor);
        if let Some(d) = opened {
            text = format!("You unlock the way {} with your key.\n{text}", d.name());
        }
        Ok((text, json!({ "look": view, "ambush": ambush })))
    }

    pub(crate) fn flee(&mut self, ctx: &mut Ctx, actor: &EntityId, dir: Option<Direction>) -> Done {
        let me = self.me(actor)?.clone();
        let def = self.pack.rooms.get(&me.room).cloned();
        let dir = match dir {
            Some(d) => d,
            None => def
                .as_ref()
                .and_then(|d| {
                    d.exits
                        .keys()
                        .copied()
                        .find(|x| !self.exit_is_locked(&me.room, *x))
                })
                .ok_or_else(|| Fail::new(ErrorCode::NoExit, "there is nowhere to run"))?,
        };
        if self.hostiles(&me.room, actor).is_empty() {
            return self.walk(ctx, actor, dir, true);
        }
        let level = me.attrs.dx
            + if self.lit(&me.room) {
                0
            } else {
                self.pack.world.darkness_penalty
            };
        let roll = success_roll(&mut self.dice, level);
        if roll.outcome.is_success() {
            let (text, mut v) = self.walk(ctx, actor, dir, true)?;
            v["flee_roll"] = json!(roll);
            return Ok((format!("You break away!\n{text}"), v));
        }
        let spec = Emit::new(
            EventKind::Combat,
            Some(me.room.clone()),
            format!("{} tries to flee and stumbles.", me.name),
        )
        .actor(actor, "flee")
        .tags(["combat".to_owned()]);
        self.emit(ctx, spec);
        let attacks = self.mob_round(ctx, actor);
        Ok((
            "You try to break away, but you are cut off!".to_owned(),
            json!({ "flee_roll": roll, "attacks": attacks }),
        ))
    }

    pub(crate) fn attack(&mut self, ctx: &mut Ctx, actor: &EntityId, word: &str) -> Done {
        let me = self.me(actor)?.clone();
        let target = self
            .find_here(actor, word)
            .filter(|t| t != actor)
            .ok_or_else(|| {
                Fail::new(ErrorCode::NoSuchTarget, format!("you see no {word:?} here"))
            })?;
        let t = self.state.entities[&target].clone();
        let safe = self.pack.rooms.get(&me.room).is_some_and(|r| r.safe);
        if safe {
            let guard_here = self
                .present_in(&me.room)
                .find(|e| e.guard && e.can_act())
                .map(|e| e.name.clone());
            let narrative = guard_here.map_or_else(
                || format!("{} reaches for a weapon. A whistle shrills somewhere close: the watch is coming.", me.name),
                |g| format!("{g} steps between {} and {}: \"Not in my town.\"", me.name, t.name),
            );
            let guards: Vec<EntityId> = self
                .state
                .entities
                .values()
                .filter(|e| e.guard && e.present())
                .map(|e| e.id.clone())
                .collect();
            let spec = Emit {
                extra: guards,
                ..Emit::new(EventKind::GuardIntervene, Some(me.room), narrative.clone())
                    .actor(actor, "attack")
                    .target(&target)
                    .tags(["combat".to_owned()])
            };
            self.emit(ctx, spec);
            return Err(Fail::new(ErrorCode::SafeZone, narrative));
        }
        let pacifist = me.traits.iter().any(|tr| {
            self.pack
                .traits
                .get(&tr.name)
                .and_then(|d| d.policy.as_deref())
                == Some("no_first_strike")
        });
        if pacifist {
            let provoked = me.last_attacked_by.as_ref().is_some_and(|(who, at)| {
                *who == target && ctx.now.saturating_sub(*at) < PROVOKED_MS
            });
            if !provoked {
                return Err(Fail::new(
                    ErrorCode::PolicyNoFirstStrike,
                    "you will not start a fight",
                ));
            }
        }
        let first = self.strike(ctx, actor, &target);
        let mut answers = Vec::new();
        let target_alive = self
            .state
            .entities
            .get(&target)
            .is_some_and(|e| e.can_act() && e.room == me.room);
        if target_alive && t.kind == EntityKind::Mob && !t.hostile {
            answers.push(self.strike(ctx, &target, actor));
        }
        answers.extend(self.mob_round(ctx, actor));
        let narrative = self.combat_text(ctx);
        Ok((narrative, json!({ "attack": first, "answers": answers })))
    }

    pub(crate) fn defend(&mut self, ctx: &mut Ctx, actor: &EntityId) -> (String, Value) {
        if let Some(e) = self.state.entities.get_mut(actor) {
            e.defending = true;
        }
        ctx.changes.push(StateChange::Status {
            entity: actor.clone(),
            status: "defending".into(),
            value: true,
        });
        let attacks = self.mob_round(ctx, actor);
        let mut text = "You fall back into a guarded stance.".to_owned();
        let more = self.combat_text(ctx);
        if !more.is_empty() {
            text = format!("{text}\n{more}");
        }
        (text, json!({ "attacks": attacks }))
    }

    /// The narratives of the combat events of this command, in order.
    fn combat_text(&self, ctx: &Ctx) -> String {
        self.log
            .iter()
            .filter(|e| ctx.events.contains(&e.seq))
            .filter(|e| {
                matches!(
                    e.kind,
                    EventKind::Combat | EventKind::Death | EventKind::ItemDrop
                )
            })
            .map(|e| e.narrative.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }
}
