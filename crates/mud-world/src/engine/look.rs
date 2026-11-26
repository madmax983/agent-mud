//! Perception: look, exits, examine, inventory, sheet, who.

use std::fmt::Write as _;

use mud_rules::{WoundState, basic_dodge};
use serde_json::{Value, json};

use super::{Done, ErrorCode, Fail, World};
use crate::ids::{EntityId, RoomId};
use crate::pack::{Direction, EntityKind};
use crate::state::Entity;

pub const fn wound_word(w: WoundState) -> &'static str {
    match w {
        WoundState::Healthy => "unhurt",
        WoundState::Staggered => "badly hurt",
        WoundState::Reeling => "reeling",
        WoundState::Dying => "dying",
        WoundState::Dead => "dead",
    }
}

pub const fn kind_word(k: EntityKind) -> &'static str {
    match k {
        EntityKind::Player => "player",
        EntityKind::Npc => "npc",
        EntityKind::Mob => "mob",
    }
}

impl World {
    fn exit_locked(&self, room: &RoomId, dir: Direction) -> bool {
        self.pack
            .rooms
            .get(room)
            .is_some_and(|r| r.locks.contains_key(&dir))
            && !self
                .state
                .rooms
                .get(room)
                .is_some_and(|s| s.unlocked.contains(&dir))
    }

    pub(crate) fn exit_is_locked(&self, room: &RoomId, dir: Direction) -> bool {
        self.exit_locked(room, dir)
    }

    /// The room as `viewer` sees it: the wire form of `mud_look`.
    #[must_use]
    pub fn look_view(&self, viewer: &EntityId) -> Value {
        let Some(me) = self.state.entities.get(viewer) else {
            return Value::Null;
        };
        let room = &me.room;
        let Some(def) = self.pack.rooms.get(room) else {
            return Value::Null;
        };
        let lit = self.lit(room);
        let exits: Vec<Value> = def
            .exits
            .keys()
            .map(|d| json!({ "dir": d.short(), "name": d.name(), "locked": self.exit_locked(room, *d) }))
            .collect();
        let entities: Vec<Value> = if lit {
            self.present_in(room)
                .filter(|e| &e.id != viewer && !e.hidden)
                .map(|e| {
                    json!({
                        "id": e.id,
                        "name": e.name,
                        "kind": kind_word(e.kind),
                        "condition": if e.unconscious { "unconscious" } else { wound_word(e.pools.wound_state()) },
                        "short": e.short,
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        let items: Vec<Value> = if lit {
            self.state.rooms.get(room).map_or_else(Vec::new, |r| {
                r.items
                    .iter()
                    .map(|i| json!({ "id": i, "name": self.item_name(*i) }))
                    .collect()
            })
        } else {
            Vec::new()
        };
        json!({
            "room_id": room,
            "title": def.title,
            "description": if lit { def.description.clone() } else { "It is pitch dark. You can make out almost nothing.".to_owned() },
            "light_level": if !def.dark { "lit" } else if lit { "lamplit" } else { "dark" },
            "exits": exits,
            "entities": entities,
            "items": items,
            "board": def.board,
            "safe": def.safe,
            "verbs": self.verbs_here(viewer),
        })
    }

    pub(crate) fn look(&self, viewer: &EntityId) -> (String, Value) {
        let v = self.look_view(viewer);
        let mut out = format!(
            "{}\n{}\n",
            v["title"].as_str().unwrap_or(""),
            v["description"].as_str().unwrap_or("")
        );
        if v["board"].as_bool() == Some(true) {
            out.push_str("A notice board stands here. (board read)\n");
        }
        let _ = writeln!(out, "Exits: {}", exits_line(&v));
        for e in v["entities"].as_array().into_iter().flatten() {
            let short = e["short"].as_str().unwrap_or("");
            let name = e["name"].as_str().unwrap_or("");
            let cond = e["condition"].as_str().unwrap_or("");
            let line = if short.is_empty() {
                format!("{name} is here.")
            } else {
                short.to_owned()
            };
            if cond == "unhurt" {
                let _ = writeln!(out, "{line}");
            } else {
                let _ = writeln!(out, "{line} ({cond})");
            }
        }
        let items: Vec<&str> = v["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| i["name"].as_str())
            .collect();
        if !items.is_empty() {
            let _ = writeln!(out, "You see: {}.", items.join(", "));
        }
        let verbs: Vec<&str> = v["verbs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        if !verbs.is_empty() {
            let _ = writeln!(out, "You could also: {}.", verbs.join(", "));
        }
        (out.trim_end().to_owned(), v)
    }

    pub(crate) fn exits(&self, viewer: &EntityId) -> (String, Value) {
        let v = self.look_view(viewer);
        (format!("Exits: {}", exits_line(&v)), v["exits"].clone())
    }

    pub(crate) fn examine(&self, viewer: &EntityId, target: &str) -> Done {
        let me = self.me(viewer)?;
        if !self.lit(&me.room) {
            return Err(Fail::new(
                ErrorCode::NoSuchTarget,
                "it is too dark to make anything out",
            ));
        }
        if let Some(id) = self.find_here(viewer, target) {
            let e = &self.state.entities[&id];
            let gear: Vec<String> = e
                .equipped
                .values()
                .filter(|i| {
                    self.state
                        .items
                        .get(i)
                        .and_then(|it| self.pack.items.get(&it.proto))
                        .is_some_and(|d| !d.hidden)
                })
                .map(|i| self.item_name(*i))
                .collect();
            let cond = if e.unconscious {
                "unconscious"
            } else {
                wound_word(e.pools.wound_state())
            };
            let mut text = format!("{}\n{} looks {cond}.", e.description, e.name);
            if !gear.is_empty() {
                let _ = write!(text, " Gear: {}.", gear.join(", "));
            }
            return Ok((
                text,
                json!({ "entity": { "id": e.id, "name": e.name, "kind": kind_word(e.kind), "condition": cond, "gear": gear, "description": e.description } }),
            ));
        }
        let here: Vec<_> = self
            .state
            .rooms
            .get(&me.room)
            .map(|r| r.items.clone())
            .unwrap_or_default()
            .into_iter()
            .chain(me.inventory.iter().copied())
            .collect();
        if let Some(item) = self.find_item(&here, target) {
            let it = &self.state.items[&item];
            let def = &self.pack.items[&it.proto];
            let mut text = def.description.clone();
            if def.container {
                if it.locked {
                    text.push_str(" It is locked.");
                } else if it.contents.is_empty() {
                    text.push_str(" It is empty.");
                } else {
                    let names: Vec<String> =
                        it.contents.iter().map(|i| self.item_name(*i)).collect();
                    let _ = write!(text, " Inside: {}.", names.join(", "));
                }
            }
            return Ok((
                text,
                json!({ "item": { "id": item, "proto": it.proto, "name": def.name, "value": def.value, "locked": it.locked, "description": def.description } }),
            ));
        }
        if let Ok(dir) = target.parse::<Direction>()
            && let Some(def) = self.pack.rooms.get(&me.room)
            && def.exits.contains_key(&dir)
        {
            let lock = def.locks.get(&dir);
            let locked = self.exit_locked(&me.room, dir);
            let text = match (lock, locked) {
                (Some(l), true) => l.describe.clone().unwrap_or_else(|| "It is locked.".into()),
                (Some(_), false) => "The lock hangs open.".to_owned(),
                _ => format!("The way {} is open.", dir.name()),
            };
            return Ok((
                text,
                json!({ "exit": { "dir": dir.short(), "locked": locked } }),
            ));
        }
        if target == "board" && self.pack.rooms.get(&me.room).is_some_and(|r| r.board) {
            return Ok((
                "A tall oak board shingled with notices. (board read)".into(),
                json!({ "board": true }),
            ));
        }
        Err(Fail::new(
            ErrorCode::NoSuchTarget,
            format!("you see no {target:?} here"),
        ))
    }

    pub(crate) fn inventory(&self, viewer: &EntityId) -> (String, Value) {
        let Some(me) = self.state.entities.get(viewer) else {
            return (String::new(), Value::Null);
        };
        let items = self.carried_view(me);
        let mut out = String::from("You carry:");
        let visible: Vec<&Value> = items.iter().collect();
        if visible.is_empty() {
            out.push_str(" nothing.");
        }
        for i in visible {
            let _ = write!(out, "\n  {}", i["name"].as_str().unwrap_or(""));
            if let Some(slot) = i["equipped"].as_str() {
                let _ = write!(out, " ({slot})");
            }
        }
        let cur = &self.pack.economy.currency;
        let _ = write!(out, "\nPurse: {}{}", me.currency, cur.short);
        (out, json!({ "inventory": items, "currency": me.currency }))
    }

    fn carried_view(&self, me: &Entity) -> Vec<Value> {
        me.inventory
            .iter()
            .filter_map(|i| {
                let it = self.state.items.get(i)?;
                let def = self.pack.items.get(&it.proto)?;
                if def.hidden {
                    return None;
                }
                let slot = me
                    .equipped
                    .iter()
                    .find(|(_, x)| *x == i)
                    .map(|(s, _)| serde_json::to_value(s).unwrap_or(Value::Null));
                Some(json!({ "id": i, "proto": it.proto, "name": def.name, "equipped": slot, "stock": it.stock }))
            })
            .collect()
    }

    /// The character sheet of `viewer`: the wire form of `mud_sheet`.
    #[must_use]
    pub fn sheet_view(&self, viewer: &EntityId) -> Value {
        let Some(me) = self.state.entities.get(viewer) else {
            return Value::Null;
        };
        let wound = me.pools.wound_state();
        let armor_dr: i32 = me
            .equipped
            .values()
            .filter_map(|i| self.state.items.get(i))
            .filter_map(|it| self.pack.items.get(&it.proto))
            .map(|d| d.dr)
            .sum();
        let skills: serde_json::Map<String, Value> = me
            .skills
            .keys()
            .filter_map(|s| Some((s.to_string(), json!(self.skill_level(me, s)?))))
            .collect();
        let traits: Vec<Value> = me
            .traits
            .iter()
            .map(|t| json!({ "name": t.name, "self_control": t.control }))
            .collect();
        let equipped: serde_json::Map<String, Value> = me
            .equipped
            .iter()
            .filter(|(_, i)| {
                self.state
                    .items
                    .get(i)
                    .and_then(|it| self.pack.items.get(&it.proto))
                    .is_some_and(|d| !d.hidden)
            })
            .map(|(s, i)| {
                (
                    serde_json::to_value(s)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default(),
                    json!(self.item_name(*i)),
                )
            })
            .collect();
        let mut v = json!({
            "id": me.id,
            "name": me.name,
            "attrs": me.attrs,
            "pools": me.pools,
            "wound_state": wound,
            "unconscious": me.unconscious,
            "dodge": basic_dodge(&me.attrs, wound),
            "dr": me.dr + armor_dr,
            "currency": me.currency,
            "currency_name": self.pack.economy.currency.name,
            "skills": skills,
            "traits": traits,
            "equipped": equipped,
            "inventory": self.carried_view(me),
            "room_id": me.room,
        });
        if let (Some(shop), Some(obj)) = (self.shop_view(me), v.as_object_mut()) {
            obj.insert("shop".into(), shop);
        }
        let post = self.post_view(&me.id);
        if let (false, Some(obj)) = (post.is_null(), v.as_object_mut()) {
            obj.insert("post".into(), post);
        }
        v
    }

    /// An entity's soft post (`post` in cast.toml), as it sees it on its
    /// own sheet: the rooms, whether it stands at its post, and, away from
    /// it, the room it goes back to and the way there. `null` for an
    /// entity with no post.
    #[must_use]
    pub fn post_view(&self, id: &EntityId) -> Value {
        let (Some(me), Some(post)) = (
            self.state.entities.get(id),
            self.pack.cast.get(id).and_then(|c| c.post.as_ref()),
        ) else {
            return Value::Null;
        };
        let at_post = post.rooms.contains(&me.room);
        let mut v = json!({ "rooms": post.rooms, "at_post": at_post });
        if !at_post
            && let Some((room, way)) = self.way_to(&me.room, &post.rooms)
            && let Some(obj) = v.as_object_mut()
        {
            let title = self.pack.rooms.get(&room).map(|r| r.title.clone());
            obj.insert("return_to".into(), json!(room));
            obj.insert("return_title".into(), json!(title));
            obj.insert(
                "way".into(),
                json!(way.iter().map(|d| d.short()).collect::<Vec<_>>()),
            );
        }
        v
    }

    /// The nearest of `targets` from `from` by the pack's exits, and the
    /// shortest way there, first exit first (breadth first, exits in
    /// display order, so the answer is the same every time). Locks are
    /// not counted: the way is a direction, not a promise. `None` when no
    /// target can be reached; an empty way when `from` is one.
    #[must_use]
    pub fn way_to(&self, from: &RoomId, targets: &[RoomId]) -> Option<(RoomId, Vec<Direction>)> {
        let mut seen = std::collections::BTreeSet::from([from.clone()]);
        let mut queue = std::collections::VecDeque::from([(from.clone(), Vec::new())]);
        while let Some((room, way)) = queue.pop_front() {
            if targets.contains(&room) {
                return Some((room, way));
            }
            let Some(def) = self.pack.rooms.get(&room) else {
                continue;
            };
            for d in Direction::ALL {
                if let Some(next) = def.exits.get(&d)
                    && seen.insert(next.clone())
                {
                    let mut w = way.clone();
                    w.push(d);
                    queue.push_back((next.clone(), w));
                }
            }
        }
        None
    }

    pub(crate) fn sheet(&self, viewer: &EntityId) -> (String, Value) {
        let v = self.sheet_view(viewer);
        let a = &v["attrs"];
        let p = &v["pools"];
        let mut out = format!(
            "{}  ST {} DX {} IQ {} HT {}\nHP {}/{}  FP {}/{}  ({})  Dodge {}  DR {}\n",
            v["name"].as_str().unwrap_or(""),
            a["st"],
            a["dx"],
            a["iq"],
            a["ht"],
            p["hp"],
            p["hp_max"],
            p["fp"],
            p["fp_max"],
            v["wound_state"].as_str().unwrap_or(""),
            v["dodge"],
            v["dr"],
        );
        let skills: Vec<String> = v["skills"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, val)| format!("{k}-{val}"))
            .collect();
        let _ = writeln!(
            out,
            "Skills: {}",
            if skills.is_empty() {
                "none".into()
            } else {
                skills.join(", ")
            }
        );
        let traits: Vec<String> = v["traits"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|t| {
                let name = t["name"].as_str().unwrap_or("");
                t["self_control"]
                    .as_u64()
                    .map_or_else(|| name.to_owned(), |n| format!("{name}({n})"))
            })
            .collect();
        if !traits.is_empty() {
            let _ = writeln!(out, "Traits: {}", traits.join(", "));
        }
        let _ = write!(
            out,
            "Purse: {}{}",
            v["currency"], self.pack.economy.currency.short
        );
        if let Some(rows) = v["shop"]["sells"].as_array() {
            let short = &self.pack.economy.currency.short;
            let _ = write!(out, "\nYour shop sells:");
            for r in rows {
                let _ = write!(
                    out,
                    "\n  {} - {}{short} (x{})",
                    r["name"].as_str().unwrap_or(""),
                    r["price"],
                    r["count"]
                );
            }
        }
        (out, v)
    }

    pub(crate) fn who(&self) -> (String, Value) {
        let mut names: Vec<String> = self
            .state
            .entities
            .values()
            .filter(|e| e.kind == EntityKind::Player && e.online)
            .map(|e| e.name.clone())
            .collect();
        // By name, never by id: the id order would set agent PCs apart.
        names.sort_by(|a, b| {
            a.to_lowercase()
                .cmp(&b.to_lowercase())
                .then_with(|| a.cmp(b))
        });
        let text = if names.is_empty() {
            "Nobody is online.".to_owned()
        } else {
            format!("Online: {}", names.join(", "))
        };
        (text, json!({ "online": names }))
    }
}

fn exits_line(v: &Value) -> String {
    let parts: Vec<String> = v["exits"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            let name = e["name"].as_str().unwrap_or("");
            if e["locked"].as_bool() == Some(true) {
                format!("{name} (locked)")
            } else {
                name.to_owned()
            }
        })
        .collect();
    if parts.is_empty() {
        "none".to_owned()
    } else {
        parts.join(", ")
    }
}
