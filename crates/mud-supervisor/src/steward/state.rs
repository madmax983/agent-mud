//! The Steward's row in the world store (`sup_steward`, ADR 0015).
//!
//! The supervisor never reads the body; the Steward owns its meaning. The
//! body is JSON under [`ROW_SCHEMA`]: a body of another schema, or one
//! that does not parse, is replaced by a fresh row, and the next goal
//! listing rebuilds the goal part.
//!
//! Every write raises the row's `rev`, and the store keeps the highest
//! ([`mud_world::side::rev_join`]). A write that comes before an owner
//! action (an intent, an accept reservation) goes through `Game::act`
//! (`synchronous = FULL`); the rest through `Game::persist`, whose loss
//! the next pass repairs from the listings.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::budget::DAY_MS;

/// The schema of the row body this code writes.
pub const ROW_SCHEMA: u32 = 1;

/// The most declined digests a row keeps; the oldest go first.
pub const DECLINED_CAP: usize = 200;

/// What the Steward remembers of one goal, by the game clock only (lg's
/// own times are never compared with it).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GoalSeen {
    /// When the Steward first listed it.
    pub first_seen_ms: u64,
    /// How many notes lg listed for it.
    pub note_count: usize,
    /// lg's stamp of its newest note: compared only with itself, to see a
    /// new note when lg lists a fixed number of them.
    pub note_mark: i64,
    /// When its notes last changed.
    pub note_change_ms: u64,
    /// Why it waits, as last published, so each reason is published once.
    pub waiting: Option<String>,
    /// Whether the agent was told it waits on the ballot cap.
    pub cap_noted: bool,
}

/// A day count of accepts: reserved when a `decide accept` is about to be
/// sent, refunded when a human decided the goal first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Accepts {
    /// The game-clock day.
    pub day: u64,
    /// Accepts reserved that day.
    pub reserved: u32,
    /// Of those, refunded.
    pub refunded: u32,
}

impl Accepts {
    /// The live accepts of the day of `now_ms`.
    #[must_use]
    pub const fn today(&self, now_ms: u64) -> u32 {
        if self.day == now_ms / DAY_MS {
            self.reserved.saturating_sub(self.refunded)
        } else {
            0
        }
    }

    /// Reserves one accept on the day of `now_ms`.
    pub const fn reserve(&mut self, now_ms: u64) {
        let day = now_ms / DAY_MS;
        if self.day != day {
            *self = Self {
                day,
                reserved: 0,
                refunded: 0,
            };
        }
        self.reserved = self.reserved.saturating_add(1);
    }

    /// Gives one accept back, on the day it was reserved.
    pub const fn refund(&mut self, now_ms: u64) {
        if self.day == now_ms / DAY_MS && self.refunded < self.reserved {
            self.refunded += 1;
        }
    }
}

/// Council ballots cast on one game-clock day.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ballots {
    /// The game-clock day.
    pub day: u64,
    /// Ballots that day (each is `council_members` votes).
    pub count: u32,
}

impl Ballots {
    /// The ballots cast on the day of `now_ms`.
    #[must_use]
    pub const fn today(&self, now_ms: u64) -> u32 {
        if self.day == now_ms / DAY_MS {
            self.count
        } else {
            0
        }
    }

    /// Counts one ballot on the day of `now_ms`.
    pub const fn cast(&mut self, now_ms: u64) {
        let day = now_ms / DAY_MS;
        if self.day != day {
            *self = Self { day, count: 0 };
        }
        self.count = self.count.saturating_add(1);
    }
}

/// What a declined digest was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeclinedKind {
    /// A goal title.
    Goal,
}

/// A declined text, by digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Declined {
    /// Its digest.
    pub digest: u64,
    /// What it was.
    pub kind: DeclinedKind,
}

/// The owner action an intent stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IntentAction {
    /// `decide accept` of a proposed goal.
    Accept {
        /// The goal.
        goal: i64,
    },
    /// `decide decline` of a proposed goal.
    Decline {
        /// The goal.
        goal: i64,
    },
    /// `decide drop_goal` of an active goal.
    DropGoal {
        /// The goal.
        goal: i64,
    },
}

impl IntentAction {
    /// The goal it decides.
    #[must_use]
    pub const fn goal(&self) -> i64 {
        match self {
            Self::Accept { goal } | Self::Decline { goal } | Self::DropGoal { goal } => *goal,
        }
    }

    /// A short name, for the observer.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Accept { .. } => "accept",
            Self::Decline { .. } => "decline",
            Self::DropGoal { .. } => "drop_goal",
        }
    }
}

/// An owner action written before it is sent: the journal that makes a
/// crash or a lost answer end in exactly one action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    /// Its number in this row.
    pub no: u64,
    /// The action.
    pub action: IntentAction,
    /// When it was last sent, by the game clock.
    pub sent_ms: u64,
    /// How many times it was sent.
    pub tries: u32,
}

/// The Steward's row of one entity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StewardRow {
    /// The goals the last `plans` listed, by id.
    pub goals: BTreeMap<i64, GoalSeen>,
    /// Accepts reserved and refunded today.
    pub accepts: Accepts,
    /// Ballots cast today.
    pub ballots: Ballots,
    /// Declined titles, newest last, at most [`DECLINED_CAP`].
    pub declined: Vec<Declined>,
    /// The open intent, if any.
    pub intent: Option<Intent>,
    /// The number of the last intent.
    pub intents: u64,
    /// Actions wait until then after an intent was stuck.
    pub stuck_until_ms: Option<u64>,
    /// Wakes that ended well since the last refine.
    pub wakes_since_refine: u32,
}

impl StewardRow {
    /// Whether a goal title of `digest` was declined.
    #[must_use]
    pub fn was_declined(&self, digest: u64) -> bool {
        self.declined
            .iter()
            .any(|d| d.digest == digest && d.kind == DeclinedKind::Goal)
    }

    /// Adds a declined title; the oldest go past [`DECLINED_CAP`].
    pub fn decline(&mut self, digest: u64) {
        if self.was_declined(digest) {
            return;
        }
        self.declined.push(Declined {
            digest,
            kind: DeclinedKind::Goal,
        });
        let over = self.declined.len().saturating_sub(DECLINED_CAP);
        self.declined.drain(..over);
    }

    /// Opens an intent, sent at `now_ms`.
    pub const fn open(&mut self, action: IntentAction, now_ms: u64) {
        self.intents = self.intents.saturating_add(1);
        self.intent = Some(Intent {
            no: self.intents,
            action,
            sent_ms: now_ms,
            tries: 1,
        });
    }

    /// The body as stored.
    #[must_use]
    pub fn body(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }

    /// The row a stored body holds, if it is of [`ROW_SCHEMA`] and parses.
    #[must_use]
    pub fn from_body(schema: u32, body: &serde_json::Value) -> Option<Self> {
        if schema != ROW_SCHEMA || !body.is_object() {
            return None;
        }
        serde_json::from_value(body.clone()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_counts_roll_with_the_game_clock() {
        let mut a = Accepts::default();
        a.reserve(10);
        a.reserve(20);
        assert_eq!(a.today(30), 2);
        a.refund(40);
        assert_eq!(a.today(40), 1);
        assert_eq!(a.today(DAY_MS + 1), 0, "a new day");
        a.refund(DAY_MS + 1);
        assert_eq!(a.refunded, 1, "a refund on another day is ignored");
        a.reserve(DAY_MS + 5);
        assert_eq!((a.day, a.reserved, a.refunded), (1, 1, 0));
        let mut b = Ballots::default();
        b.cast(1);
        b.cast(2);
        assert_eq!(b.today(3), 2);
        assert_eq!(b.today(DAY_MS), 0);
        b.cast(DAY_MS);
        assert_eq!(b.today(DAY_MS), 1);
    }

    #[test]
    fn a_body_round_trips_and_another_schema_is_refused() {
        let mut row = StewardRow::default();
        row.open(IntentAction::Accept { goal: 4 }, 9);
        row.decline(7);
        let body = row.body();
        assert_eq!(body["intent"]["action"]["kind"], "accept");
        assert_eq!(StewardRow::from_body(ROW_SCHEMA, &body), Some(row));
        assert_eq!(StewardRow::from_body(9, &body), None);
        assert_eq!(
            StewardRow::from_body(ROW_SCHEMA, &serde_json::Value::Null),
            None
        );
        assert_eq!(
            StewardRow::from_body(ROW_SCHEMA, &serde_json::json!({ "goals": 3 })),
            None
        );
    }
}
