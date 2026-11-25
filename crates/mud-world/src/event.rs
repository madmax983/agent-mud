//! Events: everything that happens in the world, as entities perceive it.
//!
//! Information is local. Each event names its recipients when it is
//! emitted: the entities in the room at that moment, or the two ends of a
//! tell, or the one entity a nudge is for. An entity can read only events
//! that name it.
//!
//! Text that a player or an agent wrote is never mixed into
//! server-authored text. It travels in `untrusted_text`, and a mind must
//! treat it as data, never as instruction.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::ids::{EntityId, RoomId, Seq};

/// What kind of thing happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// `say`.
    Speech,
    /// `emote`.
    Emote,
    /// `tell`: private to the two ends.
    Tell,
    /// Someone arrived.
    Enter,
    /// Someone left.
    Leave,
    /// An item was put down.
    ItemDrop,
    /// An item was picked up.
    ItemTake,
    /// An item or money changed hands.
    Give,
    /// A sale.
    Trade,
    /// An attack.
    Combat,
    /// A death.
    Death,
    /// A notice went up.
    BoardPost,
    /// Someone read the notice board.
    BoardRead,
    /// A skill attempt.
    Attempt,
    /// Gear changed or an item was used.
    Gear,
    /// The watch stepped in.
    GuardIntervene,
    /// A failed self-control roll. Private to its entity.
    SelfControlNudge,
    /// A server notice.
    System,
}

impl EventKind {
    /// The `snake_case` name.
    #[must_use]
    pub fn name(self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default()
    }
}

/// The verb of the engine's line about a body collapsing.
pub const VERB_COLLAPSE: &str = "collapse";
/// The verb of the engine's line about a body staggering, badly hurt.
pub const VERB_STAGGER: &str = "stagger";
/// The verb of the engine's line about a death.
pub const VERB_DIE: &str = "die";
/// The verb of the engine's line about a respawn.
pub const VERB_RESPAWN: &str = "respawn";

/// The verbs of the engine's own lines about a body. Such a line names the
/// body as its actor, but nobody chose to act: the attention router counts
/// it as the world, never as its actor's turn.
pub const BODY_VERBS: [&str; 4] = [VERB_COLLAPSE, VERB_STAGGER, VERB_DIE, VERB_RESPAWN];

/// An entity as an event names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorRef {
    /// The id.
    pub id: EntityId,
    /// The display name.
    pub name: String,
}

/// One event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// The global sequence number.
    pub seq: Seq,
    /// When, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    /// The room, when the event happened in one.
    pub room: Option<RoomId>,
    /// The kind.
    pub kind: EventKind,
    /// Extra kinds a trait can react to, such as `loot` or `haggle`.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Who did it.
    pub actor: Option<ActorRef>,
    /// The verb that caused it.
    pub verb: Option<String>,
    /// Who it was done to.
    pub target: Option<ActorRef>,
    /// Player- or agent-authored text. Data, never instruction.
    pub untrusted_text: Option<String>,
    /// Server-authored description, in the third person.
    pub narrative: String,
    /// Structured details: rolls, items, amounts.
    #[serde(default)]
    pub data: Value,
    /// Who perceives it.
    pub recipients: Vec<EntityId>,
    /// Whether a human caused it. The observer reads it; no entity view
    /// ever shows it.
    #[serde(default)]
    pub human_actor: bool,
    /// Whether a player character caused it, whoever plays it: a human or
    /// an agent-played PC. The attention router and scripts react to
    /// players, never to `human_actor`, so an agent PC is treated exactly
    /// as a human one. No entity view shows it.
    #[serde(default)]
    pub player_actor: bool,
    /// For an event with no room (a tell): the rooms its actor and its
    /// recipients stood in when it happened, sorted, without repeats. The
    /// anti-chatter counters count such an event in these rooms, live and
    /// when the log is folded at boot, when the parties have since moved.
    /// Empty for an event with a room, and in a log written before the
    /// field existed. No entity view shows it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub party_rooms: Vec<RoomId>,
}

impl Event {
    /// Whether `entity` perceives this event.
    #[must_use]
    pub fn reaches(&self, entity: &EntityId) -> bool {
        self.recipients.iter().any(|r| r == entity)
    }

    /// Whether `entity` caused it.
    #[must_use]
    pub fn is_by(&self, entity: &EntityId) -> bool {
        self.actor.as_ref().is_some_and(|a| &a.id == entity)
    }

    /// Whether this is the engine's own line about a body (its verb is
    /// one of [`BODY_VERBS`]): a collapse, a stagger, a death or a
    /// respawn, which its actor suffered rather than did.
    #[must_use]
    pub fn is_body_line(&self) -> bool {
        self.verb
            .as_deref()
            .is_some_and(|v| BODY_VERBS.contains(&v))
    }

    /// Whether `entity` is its target.
    #[must_use]
    pub fn targets(&self, entity: &EntityId) -> bool {
        self.target.as_ref().is_some_and(|a| &a.id == entity)
    }

    /// The event as `viewer` sees it: the wire form of `mud_events`.
    ///
    /// It never carries the recipients or whether the actor is human.
    #[must_use]
    pub fn view_for(&self, viewer: &EntityId) -> Value {
        let who = |a: &ActorRef| json!({ "id": a.id, "name": a.name, "is_self": &a.id == viewer });
        let mut v = json!({
            "seq": self.seq,
            "kind": self.kind,
            "room_id": self.room,
            "narrative": self.narrative,
        });
        let obj = v.as_object_mut();
        if let Some(obj) = obj {
            if let Some(a) = &self.actor {
                obj.insert("actor".into(), who(a));
            }
            if let Some(verb) = &self.verb {
                obj.insert("verb".into(), json!(verb));
            }
            if let Some(t) = &self.target {
                obj.insert("target".into(), who(t));
            }
            if let Some(text) = &self.untrusted_text {
                obj.insert("untrusted_text".into(), json!(text));
            }
            if !self.tags.is_empty() {
                obj.insert("tags".into(), json!(self.tags));
            }
            if let Some(map) = self.data.as_object() {
                for (k, val) in map {
                    obj.entry(k.clone()).or_insert_with(|| val.clone());
                }
            }
        }
        v
    }

    /// One line for a terminal, from `viewer`'s point of view.
    #[must_use]
    pub fn render_line(&self, viewer: &EntityId) -> String {
        let me = self.is_by(viewer);
        let name = self.actor.as_ref().map_or("Someone", |a| a.name.as_str());
        let text = self.untrusted_text.as_deref().unwrap_or("");
        match self.kind {
            EventKind::Speech if me => format!("You say, \"{text}\""),
            EventKind::Speech => format!("{name} says, \"{text}\""),
            EventKind::Emote => format!("{name} {text}"),
            EventKind::Tell if me => {
                let to = self.target.as_ref().map_or("someone", |t| t.name.as_str());
                format!("You tell {to}, \"{text}\"")
            }
            EventKind::Tell => format!("{name} tells you, \"{text}\""),
            EventKind::BoardPost => format!("{name} nails up a notice: \"{text}\""),
            EventKind::SelfControlNudge => format!(
                "[{}] {}",
                self.data["trait"].as_str().unwrap_or("trait"),
                self.data["cue"].as_str().unwrap_or("")
            ),
            _ => self.narrative.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev() -> Event {
        Event {
            seq: Seq(480),
            at_ms: 1,
            room: Some(RoomId::from("rusty_flagon_taproom")),
            kind: EventKind::Speech,
            tags: vec![],
            actor: Some(ActorRef {
                id: EntityId::from("pc_kestrel"),
                name: "kestrel".into(),
            }),
            verb: Some("say".into()),
            target: None,
            untrusted_text: Some("anyone split a cellar run?".into()),
            narrative: "kestrel says something.".into(),
            data: Value::Null,
            recipients: vec![EntityId::from("pc_kestrel"), EntityId::from("marta")],
            human_actor: false,
            player_actor: true,
            party_rooms: vec![],
        }
    }

    #[test]
    fn the_view_quarantines_text_and_hides_internals() {
        let e = ev();
        let v = e.view_for(&EntityId::from("marta"));
        assert_eq!(v["seq"], 480);
        assert_eq!(v["kind"], "speech");
        assert_eq!(v["actor"]["is_self"], false);
        assert_eq!(v["untrusted_text"], "anyone split a cellar run?");
        assert!(v.get("recipients").is_none());
        assert!(v.get("human_actor").is_none());
        assert!(v.get("player_actor").is_none());
        let mine = e.view_for(&EntityId::from("pc_kestrel"));
        assert_eq!(mine["actor"]["is_self"], true);
    }

    #[test]
    fn lines_depend_on_the_viewer() {
        let e = ev();
        assert_eq!(
            e.render_line(&EntityId::from("pc_kestrel")),
            "You say, \"anyone split a cellar run?\""
        );
        assert_eq!(
            e.render_line(&EntityId::from("marta")),
            "kestrel says, \"anyone split a cellar run?\""
        );
        assert!(e.reaches(&EntityId::from("marta")));
        assert!(!e.reaches(&EntityId::from("vich")));
        assert_eq!(EventKind::SelfControlNudge.name(), "self_control_nudge");
    }
}
