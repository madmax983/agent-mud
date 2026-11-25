//! The Steward's row in the world store (`sup_steward`, ADR 0015).
//!
//! The supervisor never reads the body; the Steward owns its meaning. The
//! body is JSON under [`ROW_SCHEMA`]: a body of another schema, or one
//! that does not parse, is replaced by a fresh row, and the next goal
//! listing rebuilds the goal part.
//!
//! Every write raises the row's `rev`, and the store keeps the highest
//! ([`mud_world::side::rev_join`]). A write that comes before an owner
//! action (an intent, an accept reservation), a refine pause and a veto go
//! through `Game::act` (`synchronous = FULL`); the rest through
//! `Game::persist`, whose loss the next pass repairs from the listings.
//!
//! The overlay part ([`OverlayRow`]) is the Steward's view of lg's active
//! overlay version. A fresh row has no pointer, so first contact treats
//! whatever lg has active as a human's (`preexisting`) and pauses refine
//! until the operator resumes it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::budget::DAY_MS;

/// The schema of the row body this code writes.
pub const ROW_SCHEMA: u32 = 1;

/// The most declined digests a row keeps; the oldest go first.
pub const DECLINED_CAP: usize = 200;

/// The most overlay versions of its own the Steward remembers; the oldest
/// go first, never the baseline or the active one.
pub const AUTHORED_CAP: usize = 32;

/// The most activates given up on that a row remembers.
pub const LATE_CAP: usize = 8;

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

/// A day count of accepts, reserved when a `decide accept` is about to be
/// sent.
///
/// A reservation is never given back. lg lists only proposed and active
/// goals, so a goal that a human declined and one that our accept made
/// active and the agent then finished both vanish from the listing; and a
/// `decide` that got no answer may still run from lg's queue. Either may
/// be a live accept, so the cap errs on the safe side.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Accepts {
    /// The game-clock day.
    pub day: u64,
    /// Accepts reserved that day.
    pub reserved: u32,
}

impl Accepts {
    /// The accepts reserved on the day of `now_ms`.
    #[must_use]
    pub const fn today(&self, now_ms: u64) -> u32 {
        if self.day == now_ms / DAY_MS {
            self.reserved
        } else {
            0
        }
    }

    /// Reserves one accept on the day of `now_ms`.
    pub const fn reserve(&mut self, now_ms: u64) {
        let day = now_ms / DAY_MS;
        if self.day != day {
            *self = Self { day, reserved: 0 };
        }
        self.reserved = self.reserved.saturating_add(1);
    }
}

/// An accept whose intent was given up as stuck. Its `decide` may still
/// run from lg's queue, so its reservation stays, and the next listing
/// that shows the goal active settles it as accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StuckAccept {
    /// The goal.
    pub goal: i64,
    /// The game-clock day its accept was reserved.
    pub day: u64,
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
    /// A free-text overlay the council declined (ADR 0016).
    Overlay,
}

/// A declined text, by digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Declined {
    /// Its digest.
    pub digest: u64,
    /// What it was.
    pub kind: DeclinedKind,
}

/// Where an overlay the Steward puts comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OverlaySource {
    /// The empty baseline, `{}`.
    Baseline,
    /// Operator menu items, by id, in menu order.
    Menu {
        /// The items.
        ids: Vec<String>,
    },
    /// Free-text lines a unanimous council approved (ADR 0016), without
    /// the preamble: what a later candidate is compared with.
    Free {
        /// The approved lines.
        lines: Vec<String>,
    },
}

impl OverlaySource {
    /// Whether it is a text (not the empty baseline).
    #[must_use]
    pub const fn is_text(&self) -> bool {
        !matches!(self, Self::Baseline)
    }

    /// A short line for the observer: the menu ids, `free`, or
    /// `baseline`.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Baseline => "baseline".to_owned(),
            Self::Menu { ids } => ids.join(", "),
            Self::Free { .. } => "free".to_owned(),
        }
    }

    /// The approved lines, when it is a free text.
    #[must_use]
    pub fn free_lines(&self) -> Option<&[String]> {
        match self {
            Self::Free { lines } => Some(lines),
            Self::Baseline | Self::Menu { .. } => None,
        }
    }
}

/// Why the Steward activates a version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Purpose {
    /// The empty baseline, at first contact.
    Baseline,
    /// Back to the baseline, from a text of its own.
    Clear,
    /// A text of its own, from the baseline.
    Text {
        /// The text's digest.
        digest: u64,
        /// Where it came from.
        source: OverlaySource,
        /// The guardrails text, as put.
        text: String,
    },
    /// Back to a version someone else chose, undoing its own move.
    Restore,
}

impl Purpose {
    /// A short name, for the observer.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Clear => "clear",
            Self::Text { .. } => "text",
            Self::Restore => "restore",
        }
    }
}

/// The owner action an intent stands for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// `overlay_put` of an overlay of its own. A lost answer is found
    /// again by digest; a put may be sent again (a duplicate version is
    /// harmless).
    Put {
        /// The digest of its guardrails text (empty for the baseline).
        digest: u64,
        /// Where it came from.
        source: OverlaySource,
    },
    /// `overlay_activate`. Never sent again: a lost answer is settled
    /// from lg's pointer.
    Activate {
        /// The version.
        version: u64,
        /// The version lg should report as `previous`.
        expected_previous: Option<u64>,
        /// Why.
        purpose: Purpose,
    },
    /// `refine_propose`, under a claim.
    Propose,
}

impl IntentAction {
    /// The goal it decides, if it is a goal action.
    #[must_use]
    pub const fn goal(&self) -> Option<i64> {
        match self {
            Self::Accept { goal } | Self::Decline { goal } | Self::DropGoal { goal } => Some(*goal),
            Self::Put { .. } | Self::Activate { .. } | Self::Propose => None,
        }
    }

    /// Whether it acts on an overlay (refine).
    #[must_use]
    pub const fn is_overlay(&self) -> bool {
        self.goal().is_none()
    }

    /// A short name, for the observer.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Accept { .. } => "accept",
            Self::Decline { .. } => "decline",
            Self::DropGoal { .. } => "drop_goal",
            Self::Put { .. } => "overlay_put",
            Self::Activate { .. } => "overlay_activate",
            Self::Propose => "refine_propose",
        }
    }
}

/// An owner action written before it is sent: the journal that makes a
/// crash or a lost answer end in exactly one action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// One overlay version the Steward put.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Authored {
    /// lg's version.
    pub version: u64,
    /// The digest of its guardrails text.
    pub digest: u64,
    /// Where it came from.
    pub source: OverlaySource,
    /// The guardrails text as put (`""` for the baseline): what lg runs,
    /// even after the operator edits the menu.
    #[serde(default)]
    pub text: String,
}

/// An activate whose answer never came and that had not applied when
/// last checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Late {
    /// The version.
    pub version: u64,
    /// When it was given up on, by the game clock.
    pub at_ms: u64,
}

/// Why refine is paused for an entity, and since when. Only the operator
/// ends a pause (`resume_refine`, at a later boot).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pause {
    /// When, by the game clock.
    pub at_ms: u64,
    /// The boot it was recorded in.
    pub boot: u64,
    /// `rolled_back`, `preexisting` or `unconfirmed`.
    pub code: String,
}

/// The Steward's view of an agent's overlays (ADR 0015).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlayRow {
    /// The highest version seen in `overlays`.
    pub known_max: Option<u64>,
    /// lg's active version as the Steward last confirmed it. Set from
    /// first contact on; any other version lg shows is someone else's move.
    pub pointer: Option<u64>,
    /// The Steward's empty `{}` version, once active.
    pub baseline: Option<u64>,
    /// The Steward's own puts, oldest first, at most [`AUTHORED_CAP`].
    pub authored: Vec<Authored>,
    /// Activates whose answer never came and that had not applied when
    /// last checked, at most [`LATE_CAP`]. Each lives until the daemon
    /// restarts or its window ends; a move to it after that is a human's.
    pub late: Vec<Late>,
    /// The Steward's text at `pointer`, if any.
    pub active: Option<Authored>,
    /// A text put and not yet activated: the next pass activates it.
    pub pending: Option<Authored>,
    /// Refine is paused.
    pub paused: Option<Pause>,
    /// When the last propose ran, by the game clock.
    pub last_refine_ms: Option<u64>,
    /// The version lg's Roll back would activate, as far as the Steward
    /// knows: where lg was before the Steward's last confirmed move.
    /// `None` after a move of anyone else's or one never answered.
    pub rollback_to: Option<u64>,
}

impl OverlayRow {
    /// The Steward's own entry for `version`, if it put it.
    #[must_use]
    pub fn authored(&self, version: u64) -> Option<&Authored> {
        self.authored.iter().find(|a| a.version == version)
    }

    /// Whether `version` is an empty overlay of the Steward's.
    #[must_use]
    pub fn is_empty_version(&self, version: u64) -> bool {
        self.baseline == Some(version)
            || self
                .authored(version)
                .is_some_and(|a| a.source == OverlaySource::Baseline)
    }

    /// The text entry of the Steward's at `version`, if any: what `active`
    /// becomes when lg's pointer lands there.
    #[must_use]
    pub fn text_at(&self, version: Option<u64>) -> Option<Authored> {
        version
            .and_then(|v| self.authored(v))
            .filter(|a| a.source.is_text())
            .cloned()
    }

    /// Records a put of its own; past [`AUTHORED_CAP`] the oldest go,
    /// never the baseline, the active or the pending one.
    pub fn record(&mut self, entry: Authored) {
        self.known_max = Some(
            self.known_max
                .map_or(entry.version, |k| k.max(entry.version)),
        );
        if self.authored(entry.version).is_some() {
            return;
        }
        self.authored.push(entry);
        while self.authored.len() > AUTHORED_CAP {
            let keep = |a: &Authored| {
                Some(a.version) == self.baseline
                    || self.active.as_ref().is_some_and(|x| x.version == a.version)
                    || self
                        .pending
                        .as_ref()
                        .is_some_and(|x| x.version == a.version)
            };
            let Some(i) = self.authored.iter().position(|a| !keep(a)) else {
                break;
            };
            self.authored.remove(i);
        }
    }

    /// Remembers an activate given up on at `now_ms`; past [`LATE_CAP`]
    /// the oldest go.
    pub fn add_late(&mut self, version: u64, now_ms: u64) {
        self.late.retain(|l| l.version != version);
        self.late.push(Late {
            version,
            at_ms: now_ms,
        });
        let over = self.late.len().saturating_sub(LATE_CAP);
        self.late.drain(..over);
    }

    /// Whether an activate of `version` was given up on.
    #[must_use]
    pub fn is_late(&self, version: u64) -> bool {
        self.late.iter().any(|l| l.version == version)
    }

    /// Forgets `version` as late.
    pub fn drop_late(&mut self, version: u64) {
        self.late.retain(|l| l.version != version);
    }

    /// Forgets every late activate given up on `window_ms` or more before
    /// `now_ms`. Returns whether one went.
    pub fn expire_late(&mut self, now_ms: u64, window_ms: u64) -> bool {
        let before = self.late.len();
        self.late
            .retain(|l| now_ms.saturating_sub(l.at_ms) < window_ms);
        self.late.len() != before
    }

    /// Every version the row holds as lg's: the pointer, and the ones the
    /// Steward put (with whether lg must list it as the MUD's).
    #[must_use]
    pub fn recorded(&self) -> Vec<(u64, bool)> {
        let mut out: Vec<(u64, bool)> = self.pointer.map(|p| (p, false)).into_iter().collect();
        out.extend(self.baseline.map(|b| (b, true)));
        out.extend(self.authored.iter().map(|a| (a.version, true)));
        out.extend(self.active.iter().map(|a| (a.version, true)));
        out.extend(self.pending.iter().map(|a| (a.version, true)));
        out
    }

    /// Forgets lg's store: its overlay store was reset, so no version
    /// number the row holds means what it did. Keeps the pause (only the
    /// operator ends it) and the time of the last propose.
    pub fn forget_store(&mut self) {
        *self = Self {
            paused: self.paused.take(),
            last_refine_ms: self.last_refine_ms,
            ..Self::default()
        };
    }

    /// Raises `known_max` to `version`.
    pub fn see(&mut self, version: u64) {
        self.known_max = Some(self.known_max.map_or(version, |k| k.max(version)));
    }
}

/// The Steward's row of one entity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StewardRow {
    /// The goals the last `plans` listed, by id.
    pub goals: BTreeMap<i64, GoalSeen>,
    /// Accepts reserved today.
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
    /// A stuck accept that lg may still apply.
    pub stuck_accept: Option<StuckAccept>,
    /// Wakes that ended well since the last refine.
    pub wakes_since_refine: u32,
    /// Overlay text digests a human moved away from, or the Steward
    /// revoked. Never evicted: a vetoed text is never put again.
    pub vetoed: Vec<u64>,
    /// The overlays.
    pub overlay: OverlayRow,
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
        self.push_declined(digest, DeclinedKind::Goal);
    }

    /// Whether the council declined a free text of `digest`.
    #[must_use]
    pub fn overlay_declined(&self, digest: u64) -> bool {
        self.declined
            .iter()
            .any(|d| d.digest == digest && d.kind == DeclinedKind::Overlay)
    }

    /// Adds a free text the council declined; the oldest go past
    /// [`DECLINED_CAP`]. A vetoed text is kept apart ([`Self::veto`]), so
    /// no stream of declines pushes it out.
    pub fn decline_overlay(&mut self, digest: u64) {
        self.push_declined(digest, DeclinedKind::Overlay);
    }

    fn push_declined(&mut self, digest: u64, kind: DeclinedKind) {
        if self
            .declined
            .iter()
            .any(|d| d.digest == digest && d.kind == kind)
        {
            return;
        }
        self.declined.push(Declined { digest, kind });
        let over = self.declined.len().saturating_sub(DECLINED_CAP);
        self.declined.drain(..over);
    }

    /// Whether an overlay text of `digest` was vetoed.
    #[must_use]
    pub fn is_vetoed(&self, digest: u64) -> bool {
        self.vetoed.contains(&digest)
    }

    /// Vetoes an overlay text: it is never put again.
    pub fn veto(&mut self, digest: u64) {
        if !self.is_vetoed(digest) {
            self.vetoed.push(digest);
        }
    }

    /// Opens an intent, sent at `now_ms`.
    pub fn open(&mut self, action: IntentAction, now_ms: u64) {
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
        assert_eq!(a.today(DAY_MS + 1), 0, "a new day");
        a.reserve(DAY_MS + 5);
        assert_eq!((a.day, a.reserved), (1, 1));
        let mut b = Ballots::default();
        b.cast(1);
        b.cast(2);
        assert_eq!(b.today(3), 2);
        assert_eq!(b.today(DAY_MS), 0);
        b.cast(DAY_MS);
        assert_eq!(b.today(DAY_MS), 1);
    }

    #[test]
    fn the_overlay_row_keeps_its_caps_and_never_drops_the_baseline() {
        let mut ov = OverlayRow {
            baseline: Some(1),
            ..OverlayRow::default()
        };
        let menu = |v| Authored {
            version: v,
            digest: v * 10,
            source: OverlaySource::Menu {
                ids: vec!["brevity".into()],
            },
            text: "- Keep it short.".into(),
        };
        ov.record(Authored {
            version: 1,
            digest: 0,
            source: OverlaySource::Baseline,
            text: String::new(),
        });
        ov.active = Some(menu(2));
        for v in 2..60 {
            ov.record(menu(v));
        }
        assert_eq!(ov.authored.len(), AUTHORED_CAP);
        assert!(ov.authored(1).is_some(), "the baseline stays");
        assert!(ov.authored(2).is_some(), "the active one stays");
        assert!(ov.authored(3).is_none(), "the oldest other goes");
        assert_eq!(ov.known_max, Some(59));
        assert!(ov.is_empty_version(1) && !ov.is_empty_version(2));
        assert_eq!(ov.text_at(Some(2)), Some(menu(2)));
        assert_eq!(ov.text_at(Some(1)), None, "the baseline is no text");
        assert_eq!(ov.text_at(None), None);
        for v in 0..20 {
            ov.add_late(v, 100 + v);
            ov.add_late(v, 100 + v);
        }
        assert_eq!(
            ov.late.iter().map(|l| l.version).collect::<Vec<_>>(),
            (12..20).collect::<Vec<_>>()
        );
        assert!(ov.is_late(15) && !ov.is_late(3));
        ov.drop_late(15);
        assert!(!ov.is_late(15));
        assert!(!ov.expire_late(150, 100), "none is 100 ms old");
        assert!(ov.expire_late(216, 100), "112 to 116 are");
        assert_eq!(
            ov.late.iter().map(|l| l.version).collect::<Vec<_>>(),
            [17, 18, 19]
        );
        ov.see(5);
        assert_eq!(ov.known_max, Some(59));
        let mut row = StewardRow::default();
        row.veto(7);
        row.veto(7);
        assert_eq!(row.vetoed, [7]);
        assert!(row.is_vetoed(7) && !row.is_vetoed(8));
        assert_eq!(OverlaySource::Baseline.label(), "baseline");
        let free = OverlaySource::Free {
            lines: vec!["Keep trades short.".into()],
        };
        assert_eq!(free.label(), "free");
        assert!(free.is_text());
        assert_eq!(
            free.free_lines(),
            Some(&["Keep trades short.".to_owned()][..])
        );
        assert_eq!(OverlaySource::Baseline.free_lines(), None);
        row.decline_overlay(9);
        row.decline_overlay(9);
        row.decline(9);
        assert!(row.overlay_declined(9) && row.was_declined(9));
        assert!(!row.overlay_declined(7));
        assert_eq!(row.declined.len(), 2, "one per kind");
        assert_eq!(menu(2).source.label(), "brevity");
        assert_eq!(
            [
                Purpose::Baseline,
                Purpose::Clear,
                Purpose::Restore,
                Purpose::Text {
                    digest: 1,
                    source: OverlaySource::Baseline,
                    text: String::new(),
                }
            ]
            .map(|p| p.name()),
            ["baseline", "clear", "restore", "text"]
        );
    }

    #[test]
    fn a_forgotten_store_keeps_only_the_pause_and_the_last_propose() {
        let mut ov = OverlayRow {
            known_max: Some(9),
            pointer: Some(3),
            baseline: Some(1),
            last_refine_ms: Some(5),
            rollback_to: Some(1),
            paused: Some(Pause {
                at_ms: 1,
                boot: 2,
                code: "rolled_back".into(),
            }),
            ..OverlayRow::default()
        };
        ov.add_late(4, 1);
        assert_eq!(ov.recorded(), [(3, false), (1, true)]);
        let paused = ov.paused.clone();
        ov.forget_store();
        assert_eq!(
            ov,
            OverlayRow {
                paused,
                last_refine_ms: Some(5),
                ..OverlayRow::default()
            }
        );
        assert_eq!(ov.recorded(), []);
    }

    #[test]
    fn an_overlay_intent_round_trips() {
        let mut row = StewardRow::default();
        row.open(
            IntentAction::Activate {
                version: 4,
                expected_previous: Some(1),
                purpose: Purpose::Text {
                    digest: 9,
                    source: OverlaySource::Menu {
                        ids: vec!["pacing".into()],
                    },
                    text: "- Pace.".into(),
                },
            },
            5,
        );
        row.overlay.paused = Some(Pause {
            at_ms: 1,
            boot: 2,
            code: "rolled_back".into(),
        });
        row.veto(3);
        let body = row.body();
        assert_eq!(body["intent"]["action"]["kind"], "activate");
        assert_eq!(StewardRow::from_body(ROW_SCHEMA, &body), Some(row.clone()));
        let action = row.intent.unwrap().action;
        assert_eq!((action.goal(), action.is_overlay()), (None, true));
        assert_eq!(action.name(), "overlay_activate");
        for (a, name) in [
            (IntentAction::Propose, "refine_propose"),
            (
                IntentAction::Put {
                    digest: 1,
                    source: OverlaySource::Baseline,
                },
                "overlay_put",
            ),
        ] {
            assert_eq!(a.name(), name);
            assert!(a.is_overlay());
        }
    }

    #[test]
    fn a_body_round_trips_and_another_schema_is_refused() {
        let mut row = StewardRow::default();
        row.open(IntentAction::Accept { goal: 4 }, 9);
        row.decline(7);
        row.stuck_accept = Some(StuckAccept { goal: 3, day: 2 });
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
