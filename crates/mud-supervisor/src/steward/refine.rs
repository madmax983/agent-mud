//! Step 5 of a pass: refine (ADR 0015, phase 4).
//!
//! lg's proposer (`refine_propose`) reads an agent's recent play and
//! writes a revision of its overlay. The Steward never activates that
//! proposal. In `menu` mode it is only evidence: the council picks
//! operator menu items from it, and the Steward puts and activates
//! `{"guardrails": <the picked items>}` itself.
//!
//! lg's `overlay_activate` takes no expected version, so the Steward keeps
//! its own view of lg's active version (`pointer`) and checks every move:
//!
//! - **The pointer check.** Every pass reads `overlays`. A version the
//!   Steward did not set and did not give up on is a human's move (lg's
//!   Roll back button, or its refine tab): refine pauses for that entity,
//!   the Steward's text is vetoed for good, and the Steward never moves the
//!   pointer away from the human's choice. Only the operator resumes
//!   (`resume_refine`).
//! - **Activate.** It re-reads the pointer right before, writes the intent
//!   durably, and checks the `previous` lg reports. A human move in between
//!   is restored at once, then refine pauses. A lost answer is never sent
//!   again: lg's pointer settles it (not applied: the version goes on the
//!   late list; applied: a move to `{}` is adopted, a text is undone and
//!   refine pauses, `unconfirmed`).
//! - **One move above the baseline.** First contact puts and activates an
//!   empty `{}` baseline (or pauses as `preexisting` when lg already has an
//!   active overlay). Every text goes on from the baseline, so one Roll
//!   back always lands on `{}`.

use std::collections::BTreeMap;
use std::path::Path;

use mud_world::EntityId;

use super::council::Ballot;
use super::state::{Authored, IntentAction, OverlaySource, Pause, Purpose};
use super::vet::{self, PointerVerdict};
use super::{OwnedDaemons, Run, Steward, StewardEvent, StewardNote, StewardPoint};
use crate::budget::BudgetLevel;
use crate::littleguy::PersonaText;
use crate::littleguy::owner::{self, OverlayEntry, OwnerError, OwnerRequest, OwnerResponse};
use crate::mind::Usage;
use crate::runtime::Supervisor;

/// The author lg records for every put and activate of the MUD.
pub const MUD_AUTHOR: &str = "operator:mud";

/// What `overlays` answered.
#[derive(Debug, Clone)]
pub(super) struct Overlays {
    /// lg's active version.
    pub active: Option<u64>,
    /// Every version, oldest first.
    pub entries: Vec<OverlayEntry>,
}

/// How an activate ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Activated {
    /// lg made exactly the move the Steward meant.
    Done,
    /// It did not happen as meant; the row says what holds now.
    Not,
    /// No answer, and lg's pointer could not be read: the intent stays
    /// open for the next pass.
    Open,
}

/// What an `overlay_activate` got back.
enum Sent {
    /// `overlay_active`.
    Answer { version: u64, previous: Option<u64> },
    /// lg refused it.
    Refused(String),
    /// No answer; the intent stays open.
    Lost,
    /// The intent could not be written: nothing was sent.
    NotWritten,
}

/// What the pointer check asks for next.
enum Next {
    /// Nothing.
    Nothing,
    /// First contact found nothing active: put and activate the baseline.
    Baseline,
    /// An activate given up on applied late: undo it, back to `to`.
    Undo { to: u64, late: u64 },
}

fn as_id(version: u64) -> Option<i64> {
    i64::try_from(version).ok()
}

impl Steward {
    /// Step 5: settles an open overlay intent, checks lg's pointer, then
    /// activates a pending text or runs a propose when one is due.
    pub(super) async fn refine(
        &self,
        d: &dyn OwnedDaemons,
        e: &EntityId,
        socket: &Path,
        run: &mut Run,
        now: u64,
        level: BudgetLevel,
    ) {
        if let Some(intent) = run.row.intent.clone()
            && intent.action.is_overlay()
            && !self
                .reconcile_overlay(
                    e,
                    socket,
                    run,
                    intent.action,
                    intent.sent_ms,
                    intent.tries,
                    now,
                    level,
                )
                .await
        {
            return;
        }
        let can_act = run.row.intent.is_none()
            && run.row.stuck_until_ms.is_none()
            && level != BudgetLevel::Hard;
        let Some(ov) = self.read_overlays(socket).await else {
            return;
        };
        // Versions up to here are known: a lost put is looked for above.
        if let Some(max) = ov.entries.iter().map(|x| x.version).max()
            && run.row.overlay.known_max.is_none_or(|k| k < max)
        {
            run.row.overlay.see(max);
            run.dirty = true;
        }
        match self.judge_pointer(e, run, ov.active, now) {
            Next::Nothing => {}
            Next::Baseline if can_act => {
                self.start_baseline(e, socket, run, now).await;
                return;
            }
            Next::Undo { to, late } if can_act => {
                run.row.overlay.late.retain(|v| *v != late);
                run.dirty = true;
                self.publish(
                    e,
                    "overlay",
                    as_id(late),
                    "late_undone",
                    "unconfirmed",
                    None,
                    None,
                );
                self.restore(e, socket, run, to, Some(late), now).await;
                return;
            }
            Next::Baseline | Next::Undo { .. } => return,
        }
        let ov = &run.row.overlay;
        if !can_act || ov.paused.is_some() || ov.baseline.is_none() || ov.pointer.is_none() {
            return;
        }
        if let Some(pending) = run.row.overlay.pending.take() {
            run.dirty = true;
            self.chain_text(e, socket, run, pending, now).await;
            return;
        }
        if self.propose_due(e, run, now, level) {
            self.propose(d, e, socket, run, now).await;
        }
    }

    /// Whether a propose is due now (9.1): the kernel, a judge, a ballot
    /// left today, and no player in the entity's room.
    fn propose_due(&self, e: &EntityId, run: &Run, now: u64, level: BudgetLevel) -> bool {
        let ov = &run.row.overlay;
        self.judge.is_some()
            && run.row.ballots.today(now) < self.cfg.max_council_votes_per_day
            && vet::refine_due(
                now,
                ov.last_refine_ms,
                self.cfg.refine_every_hours.saturating_mul(3_600_000),
                run.row.wakes_since_refine,
                self.cfg.refine_min_wakes,
                ov.paused.is_some(),
                level == BudgetLevel::Normal,
            )
            && !self.player_near(e)
    }

    /// Whether a player character other than `e` is in `e`'s room.
    fn player_near(&self, e: &EntityId) -> bool {
        self.sup.game().read(|w| {
            w.entity(e)
                .is_some_and(|me| w.player_present_except(&me.room, e))
        })
    }

    /// lg's overlay listing; `None` when it does not answer (or refine is
    /// off in that daemon).
    pub(super) async fn read_overlays(&self, socket: &Path) -> Option<Overlays> {
        match owner::request(socket, &OwnerRequest::Overlays, self.read_timeout).await {
            Ok(OwnerResponse::Overlays { active, entries }) => Some(Overlays { active, entries }),
            other => {
                tracing::debug!(answer = ?other, "no overlay listing");
                None
            }
        }
    }

    /// The pointer check (9.2) on lg's active version `lg`: settles what
    /// it can in the row, and says what move is needed next.
    fn judge_pointer(&self, e: &EntityId, run: &mut Run, lg: Option<u64>, now: u64) -> Next {
        let ov = &run.row.overlay;
        let expected = ov.pointer;
        let fresh = expected.is_none()
            && ov.baseline.is_none()
            && ov.authored.is_empty()
            && ov.paused.is_none();
        let late = lg.is_some_and(|v| ov.late.contains(&v));
        match vet::pointer_verdict(lg, expected, late) {
            PointerVerdict::Same => {
                if ov.baseline.is_none() && ov.paused.is_none() {
                    Next::Baseline
                } else {
                    Next::Nothing
                }
            }
            PointerVerdict::OwnLate => {
                let Some(v) = lg else {
                    return Next::Nothing;
                };
                if ov.is_empty_version(v) {
                    // A move to `{}` is always safe: adopt it.
                    let ov = &mut run.row.overlay;
                    ov.late.retain(|x| *x != v);
                    ov.pointer = Some(v);
                    ov.baseline = ov.baseline.or(Some(v));
                    ov.active = None;
                    run.dirty = true;
                    self.publish(e, "overlay", as_id(v), "activated", "baseline", None, None);
                    Next::Nothing
                } else if let Some(to) = expected {
                    Next::Undo { to, late: v }
                } else {
                    self.human_moved(e, run, lg, now, "unconfirmed");
                    Next::Nothing
                }
            }
            PointerVerdict::Human => {
                if fresh {
                    // Someone set an overlay before the Steward ever ran: it
                    // is never replaced.
                    run.row.overlay.pointer = lg;
                    self.pause(e, run, now, "preexisting", false);
                } else {
                    self.human_moved(e, run, lg, now, "rolled_back");
                }
                Next::Nothing
            }
        }
    }

    /// A human moved lg's pointer to `lg`: the Steward's text there is
    /// vetoed and refine pauses. While paused, a later move is adopted and
    /// says nothing new. The Steward never moves the pointer back.
    pub(super) fn human_moved(
        &self,
        e: &EntityId,
        run: &mut Run,
        lg: Option<u64>,
        now: u64,
        code: &str,
    ) {
        let ov = &mut run.row.overlay;
        if ov.paused.is_some() {
            if ov.pointer != lg {
                ov.pointer = lg;
                ov.active = ov.text_at(lg);
                run.dirty = true;
            }
            return;
        }
        if let Some(a) = ov.active.take() {
            run.row.veto(a.digest);
        }
        let ov = &mut run.row.overlay;
        ov.pointer = lg;
        // A human may roll back onto an older text of the Steward's: that
        // is their choice.
        ov.active = ov.text_at(lg);
        self.pause(e, run, now, code, true);
    }

    /// Pauses refine for `e`, durably, and says so.
    fn pause(&self, e: &EntityId, run: &mut Run, now: u64, code: &str, tell: bool) {
        let ov = &mut run.row.overlay;
        ov.pending = None;
        if ov.paused.is_none() {
            ov.paused = Some(Pause {
                at_ms: now,
                boot: self.boot,
                code: code.to_owned(),
            });
            self.publish(
                e,
                "overlay",
                run.row.overlay.pointer.and_then(as_id),
                "paused",
                code,
                None,
                None,
            );
            if tell {
                self.note_plain(e, "overlay_off");
            }
        }
        run.dirty = true;
        self.write(e, run, true);
    }

    /// Puts the empty baseline (or reuses one put before) and activates it
    /// from whatever lg holds now.
    async fn start_baseline(&self, e: &EntityId, socket: &Path, run: &mut Run, now: u64) {
        let ov = &run.row.overlay;
        let from = ov.pointer;
        let reuse = ov
            .authored
            .iter()
            .rev()
            .find(|a| {
                a.source == OverlaySource::Baseline
                    && !ov.late.contains(&a.version)
                    && Some(a.version) != from
            })
            .map(|a| a.version);
        let b = match reuse {
            Some(b) => b,
            None => match self.put(e, socket, run, OverlaySource::Baseline, now).await {
                Some(b) => b,
                None => return,
            },
        };
        self.activate(e, socket, run, b, from, Purpose::Baseline, now)
            .await;
    }

    /// The guardrails text of `source`: `""` for the baseline.
    fn source_text(&self, source: &OverlaySource) -> String {
        match source {
            OverlaySource::Baseline => String::new(),
            OverlaySource::Menu { ids } => vet::render_guardrails(&self.menu, ids),
        }
    }

    /// Puts the overlay of `source` (intent first). Returns its version
    /// once lg stored it; `None` when lg refused it or the answer was lost
    /// (the intent then stays open, and a later pass finds the version by
    /// its content).
    async fn put(
        &self,
        e: &EntityId,
        socket: &Path,
        run: &mut Run,
        source: OverlaySource,
        now: u64,
    ) -> Option<u64> {
        let text = self.source_text(&source);
        let digest = vet::overlay_digest(&text);
        run.row.open(
            IntentAction::Put {
                digest,
                source: source.clone(),
            },
            now,
        );
        if !self.write(e, run, true) {
            run.row.intent = None;
            return None;
        }
        self.point(StewardPoint::IntentWritten, e);
        self.send_put(e, socket, run, &text, digest, source).await
    }

    /// Sends the put of an open `Put` intent and settles it from the
    /// answer.
    async fn send_put(
        &self,
        e: &EntityId,
        socket: &Path,
        run: &mut Run,
        text: &str,
        digest: u64,
        source: OverlaySource,
    ) -> Option<u64> {
        let req = OwnerRequest::OverlayPut {
            overlay: vet::overlay_json(text),
        };
        match owner::request(socket, &req, self.owner_timeout).await {
            Ok(OwnerResponse::OverlayStored { version }) => {
                run.row.overlay.record(Authored {
                    version,
                    digest,
                    source,
                });
                run.row.intent = None;
                run.dirty = true;
                self.point(StewardPoint::OverlayStored, e);
                Some(version)
            }
            Err(err) => {
                tracing::warn!(entity = %e, error = %err, "an overlay put got no answer; a later pass finds it by its content");
                None
            }
            Ok(other) => {
                tracing::warn!(entity = %e, answer = ?other, "lg refused an overlay put");
                run.row.intent = None;
                run.dirty = true;
                self.publish(e, "overlay", None, "stuck", "put_refused", None, None);
                None
            }
        }
    }

    /// Activates `v` (a baseline, a clear back to it, or a text), expecting
    /// lg to hold `from` (9.2).
    #[expect(
        clippy::too_many_arguments,
        reason = "one move's context, passed down rather than stored"
    )]
    async fn activate(
        &self,
        e: &EntityId,
        socket: &Path,
        run: &mut Run,
        v: u64,
        from: Option<u64>,
        purpose: Purpose,
        now: u64,
    ) -> Activated {
        // 1. Re-read right before: a human move since the pass began is
        // the pointer check's, and nothing is sent.
        let Some(ov) = self.read_overlays(socket).await else {
            return Activated::Not;
        };
        if ov.active != from {
            let _ = self.judge_pointer(e, run, ov.active, now);
            return Activated::Not;
        }
        // 2 and 3. The intent, then the activate.
        match self
            .send_activate(e, socket, run, v, from, purpose.clone(), now)
            .await
        {
            Sent::Answer { version, previous } => {
                if vet::pointer_verdict(previous, from, false) == PointerVerdict::Same {
                    self.applied(e, run, v, &purpose);
                    return Activated::Done;
                }
                // A human moved the pointer to `previous` between the
                // re-read and lg running ours, and ours overwrote it.
                // Restore it, then pause.
                // The human moved away from the text at `from` (if one of
                // the Steward's) and never chose this one: both are vetoed.
                if let Purpose::Text { digest, .. } = &purpose {
                    run.row.veto(*digest);
                }
                if let Some(a) = run.row.overlay.active.take() {
                    run.row.veto(a.digest);
                }
                run.row.overlay.pointer = Some(version);
                run.dirty = true;
                match previous {
                    Some(p) => {
                        if self.restore(e, socket, run, p, Some(version), now).await {
                            self.pause(e, run, now, "rolled_back", true);
                        }
                    }
                    None => self.pause(e, run, now, "rolled_back", true),
                }
                Activated::Not
            }
            Sent::Refused(message) => {
                tracing::warn!(entity = %e, version = v, %message, "lg refused an overlay activate");
                self.publish(
                    e,
                    "overlay",
                    as_id(v),
                    "stuck",
                    "activate_refused",
                    None,
                    None,
                );
                Activated::Not
            }
            Sent::Lost => {
                self.settle_lost(e, socket, run, v, from, purpose, now)
                    .await
            }
            Sent::NotWritten => Activated::Not,
        }
    }

    /// Writes the `Activate` intent and sends it, waiting on the connection
    /// up to `activate_wait_secs`. It is never sent again.
    #[expect(
        clippy::too_many_arguments,
        reason = "one move's context, passed down rather than stored"
    )]
    async fn send_activate(
        &self,
        e: &EntityId,
        socket: &Path,
        run: &mut Run,
        v: u64,
        from: Option<u64>,
        purpose: Purpose,
        now: u64,
    ) -> Sent {
        run.row.open(
            IntentAction::Activate {
                version: v,
                expected_previous: from,
                purpose,
            },
            now,
        );
        if !self.write(e, run, true) {
            run.row.intent = None;
            return Sent::NotWritten;
        }
        self.point(StewardPoint::IntentWritten, e);
        let req = OwnerRequest::OverlayActivate { version: v };
        match owner::request(socket, &req, self.activate_wait).await {
            Ok(OwnerResponse::OverlayActive { version, previous }) => {
                run.row.intent = None;
                run.dirty = true;
                Sent::Answer { version, previous }
            }
            Ok(other) => {
                run.row.intent = None;
                run.dirty = true;
                Sent::Refused(format!("{other:?}"))
            }
            Err(err) => {
                tracing::warn!(entity = %e, version = v, error = %err, "an overlay activate got no answer; it is never sent again");
                Sent::Lost
            }
        }
    }

    /// The move of `purpose` to `v` happened exactly as meant.
    fn applied(&self, e: &EntityId, run: &mut Run, v: u64, purpose: &Purpose) {
        let ov = &mut run.row.overlay;
        ov.pointer = Some(v);
        ov.late.retain(|x| *x != v);
        run.dirty = true;
        match purpose {
            Purpose::Baseline => {
                ov.baseline = Some(v);
                ov.active = None;
                self.publish(e, "overlay", as_id(v), "activated", "baseline", None, None);
            }
            Purpose::Clear => {
                ov.active = None;
                self.publish(e, "overlay", as_id(v), "activated", "clear", None, None);
            }
            Purpose::Text { digest, source } => {
                ov.active = Some(Authored {
                    version: v,
                    digest: *digest,
                    source: source.clone(),
                });
                let text = self.source_text(source);
                self.publish_full(StewardEvent {
                    entity: e.clone(),
                    item: "overlay".into(),
                    id: as_id(v),
                    action: "activated".into(),
                    code: "text".into(),
                    votes: None,
                    text: Some(format!("{}: {text}", source.label())),
                    reasons: Vec::new(),
                });
                self.note_plain(e, "overlay_on");
            }
            Purpose::Restore => {
                ov.active = ov.text_at(Some(v));
            }
        }
    }

    /// Activates `to`, a version someone else chose, undoing the Steward's
    /// own move to `from`. Returns whether lg is back on `to` exactly. It
    /// never chains: anything else adopts lg's pointer and pauses
    /// (`unconfirmed`).
    async fn restore(
        &self,
        e: &EntityId,
        socket: &Path,
        run: &mut Run,
        to: u64,
        from: Option<u64>,
        now: u64,
    ) -> bool {
        let Some(ov) = self.read_overlays(socket).await else {
            self.pause(e, run, now, "unconfirmed", true);
            return false;
        };
        if ov.active != from {
            self.adopt_unconfirmed(e, run, ov.active, now);
            return false;
        }
        match self
            .send_activate(e, socket, run, to, from, Purpose::Restore, now)
            .await
        {
            Sent::Answer { version, previous } => {
                if version == to
                    && vet::pointer_verdict(previous, from, false) == PointerVerdict::Same
                {
                    self.applied(e, run, to, &Purpose::Restore);
                    self.publish(e, "overlay", as_id(to), "restored", "restored", None, None);
                    true
                } else {
                    self.adopt_unconfirmed(e, run, Some(version), now);
                    false
                }
            }
            Sent::Refused(_) => {
                self.publish(
                    e,
                    "overlay",
                    as_id(to),
                    "stuck",
                    "activate_refused",
                    None,
                    None,
                );
                self.pause(e, run, now, "unconfirmed", true);
                false
            }
            Sent::Lost => {
                if let Some(ov) = self.read_overlays(socket).await {
                    run.row.intent = None;
                    self.adopt_unconfirmed(e, run, ov.active, now);
                }
                false
            }
            Sent::NotWritten => false,
        }
    }

    /// Takes lg's pointer as it is and pauses: the Steward cannot tell
    /// whose move it was.
    fn adopt_unconfirmed(&self, e: &EntityId, run: &mut Run, lg: Option<u64>, now: u64) {
        let ov = &mut run.row.overlay;
        ov.pointer = lg;
        ov.active = ov.text_at(lg);
        run.dirty = true;
        self.publish(
            e,
            "overlay",
            lg.and_then(as_id),
            "unconfirmed",
            "unconfirmed",
            None,
            None,
        );
        self.pause(e, run, now, "unconfirmed", true);
    }

    /// An activate of `v` from `from` got no answer (9.2 step 6), now or
    /// before a restart. It is never sent again; lg's pointer settles it.
    #[expect(
        clippy::too_many_arguments,
        reason = "one move's context, passed down rather than stored"
    )]
    async fn settle_lost(
        &self,
        e: &EntityId,
        socket: &Path,
        run: &mut Run,
        v: u64,
        from: Option<u64>,
        purpose: Purpose,
        now: u64,
    ) -> Activated {
        let Some(ov) = self.read_overlays(socket).await else {
            return Activated::Open;
        };
        run.row.intent = None;
        run.dirty = true;
        let lg = ov.active;
        if lg == from {
            // Not applied; it may still wait in lg's queue. If it applies
            // later, the pointer check undoes it.
            run.row.overlay.add_late(v);
            self.publish(
                e,
                "overlay",
                as_id(v),
                "unconfirmed",
                "unconfirmed",
                None,
                None,
            );
            return Activated::Not;
        }
        if lg == Some(v) {
            return match purpose {
                Purpose::Baseline | Purpose::Clear => {
                    self.applied(e, run, v, &purpose);
                    Activated::Done
                }
                Purpose::Text { .. } => {
                    // Applied, and its `previous` is unknown: a human move
                    // inside the window cannot be ruled out. Back to the
                    // baseline, and pause.
                    run.row.overlay.pointer = Some(v);
                    run.row.overlay.active = None;
                    match from {
                        Some(b) => {
                            if self.restore(e, socket, run, b, Some(v), now).await {
                                self.pause(e, run, now, "unconfirmed", true);
                            }
                        }
                        None => self.pause(e, run, now, "unconfirmed", true),
                    }
                    Activated::Not
                }
                Purpose::Restore => {
                    self.adopt_unconfirmed(e, run, lg, now);
                    Activated::Not
                }
            };
        }
        if purpose == Purpose::Restore {
            self.adopt_unconfirmed(e, run, lg, now);
            return Activated::Not;
        }
        run.row.overlay.add_late(v);
        self.human_moved(e, run, lg, now, "rolled_back");
        Activated::Not
    }

    /// Settles an open overlay intent at the start of a pass (step 0).
    /// Returns whether it is settled.
    #[expect(
        clippy::too_many_arguments,
        reason = "one intent's context, passed down rather than stored"
    )]
    async fn reconcile_overlay(
        &self,
        e: &EntityId,
        socket: &Path,
        run: &mut Run,
        action: IntentAction,
        sent_ms: u64,
        tries: u32,
        now: u64,
        level: BudgetLevel,
    ) -> bool {
        match action {
            IntentAction::Propose => {
                // The proposer ran, or not, before a restart; its answer is
                // gone. Count it as this interval's propose.
                run.row.intent = None;
                run.row.overlay.last_refine_ms = Some(now);
                run.row.wakes_since_refine = 0;
                run.dirty = true;
                true
            }
            IntentAction::Activate {
                version,
                expected_previous,
                purpose,
            } => {
                if level == BudgetLevel::Hard {
                    return false;
                }
                if purpose == Purpose::Restore {
                    let Some(ov) = self.read_overlays(socket).await else {
                        return false;
                    };
                    run.row.intent = None;
                    self.adopt_unconfirmed(e, run, ov.active, now);
                    return true;
                }
                self.settle_lost(e, socket, run, version, expected_previous, purpose, now)
                    .await
                    != Activated::Open
            }
            IntentAction::Put { digest, source } => {
                self.reconcile_put(e, socket, run, digest, source, sent_ms, tries, now, level)
                    .await
            }
            IntentAction::Accept { .. }
            | IntentAction::Decline { .. }
            | IntentAction::DropGoal { .. } => true,
        }
    }

    /// Finds a put whose answer was lost by its content; sends it again
    /// after the retry window.
    #[expect(
        clippy::too_many_arguments,
        reason = "one intent's context, passed down rather than stored"
    )]
    async fn reconcile_put(
        &self,
        e: &EntityId,
        socket: &Path,
        run: &mut Run,
        digest: u64,
        source: OverlaySource,
        sent_ms: u64,
        tries: u32,
        now: u64,
        level: BudgetLevel,
    ) -> bool {
        let text = self.source_text(&source);
        if vet::overlay_digest(&text) != digest {
            // The menu changed since: this put is no longer wanted.
            run.row.intent = None;
            run.dirty = true;
            self.publish(e, "overlay", None, "stuck", "put_changed", None, None);
            return true;
        }
        let Some(ov) = self.read_overlays(socket).await else {
            return false;
        };
        let known = run.row.overlay.known_max;
        for entry in ov
            .entries
            .iter()
            .filter(|x| x.author == MUD_AUTHOR && known.is_none_or(|k| x.version > k))
        {
            let req = OwnerRequest::OverlayShow {
                version: entry.version,
            };
            let Ok(OwnerResponse::Overlay { overlay, .. }) =
                owner::request(socket, &req, self.read_timeout).await
            else {
                continue;
            };
            if vet::overlay_matches(&overlay, &text) {
                let found = Authored {
                    version: entry.version,
                    digest,
                    source,
                };
                if found.source.is_text() {
                    run.row.overlay.pending = Some(found.clone());
                }
                run.row.overlay.record(found);
                run.row.intent = None;
                run.dirty = true;
                return true;
            }
        }
        let retry_ms = self.cfg.intent_retry_secs.saturating_mul(1_000);
        if now.saturating_sub(sent_ms) < retry_ms || level == BudgetLevel::Hard {
            return false;
        }
        if tries >= self.cfg.max_intent_tries {
            tracing::warn!(entity = %e, tries, "an overlay put got no answer; the Steward backs off");
            run.row.intent = None;
            run.row.stuck_until_ms =
                Some(now.saturating_add(self.cfg.stuck_backoff_hours.saturating_mul(3_600_000)));
            run.dirty = true;
            self.publish(e, "overlay", None, "stuck", "overlay_put", None, None);
            return true;
        }
        if let Some(i) = run.row.intent.as_mut() {
            i.tries = i.tries.saturating_add(1);
            i.sent_ms = now;
        }
        if !self.write(e, run, true) {
            return false;
        }
        if let Some(v) = self
            .send_put(e, socket, run, &text, digest, source.clone())
            .await
            && source.is_text()
        {
            run.row.overlay.pending = run.row.overlay.authored(v).cloned();
        }
        run.row.intent.is_none()
    }

    /// Activates a text of its own one move above the baseline: back to
    /// the baseline first when lg is anywhere else, then the text.
    async fn chain_text(
        &self,
        e: &EntityId,
        socket: &Path,
        run: &mut Run,
        text: Authored,
        now: u64,
    ) {
        let Some(b) = run.row.overlay.baseline else {
            return;
        };
        let pointer = run.row.overlay.pointer;
        if pointer != Some(b)
            && self
                .activate(e, socket, run, b, pointer, Purpose::Clear, now)
                .await
                != Activated::Done
        {
            return;
        }
        self.activate(
            e,
            socket,
            run,
            text.version,
            Some(b),
            Purpose::Text {
                digest: text.digest,
                source: text.source,
            },
            now,
        )
        .await;
    }

    /// Runs lg's proposer under a claim (9.4), then the menu ballot (9.6).
    async fn propose(
        &self,
        d: &dyn OwnedDaemons,
        e: &EntityId,
        socket: &Path,
        run: &mut Run,
        now: u64,
    ) {
        let Some(claim) = Supervisor::claim(&self.sup, e) else {
            return;
        };
        // lg's proposer writes no ledger row: charge an estimate.
        self.sup.record_spend(
            e,
            Usage {
                calls: 1,
                input_tokens: self.cfg.refine_charge_tokens,
                output_tokens: 0,
                cost_usd_micros: 0,
            },
        );
        run.row.open(IntentAction::Propose, now);
        if !self.write(e, run, true) {
            run.row.intent = None;
            return;
        }
        self.point(StewardPoint::IntentWritten, e);
        let answer =
            match owner::request(socket, &OwnerRequest::RefinePropose, self.refine_timeout).await {
                Ok(a) => Some(a),
                Err(OwnerError::Timeout) => {
                    // The proposer still runs in lg's loop, and wakes would
                    // queue behind it there: hold the claim until it is done.
                    if !self.drain(socket).await {
                        self.publish(e, "overlay", None, "stuck", "refine_propose", None, None);
                    }
                    None
                }
                Err(err) => {
                    tracing::warn!(entity = %e, error = %err, "refine_propose failed");
                    None
                }
            };
        drop(claim);
        run.row.intent = None;
        run.row.overlay.last_refine_ms = Some(now);
        run.row.wakes_since_refine = 0;
        run.dirty = true;
        match answer {
            Some(OwnerResponse::Proposed { version, overlay }) => {
                // The proposer's own version: never activated.
                run.row.overlay.see(version);
                self.publish(e, "overlay", as_id(version), "proposed", "menu", None, None);
                self.menu_pick(d, e, socket, run, &overlay, now).await;
            }
            Some(OwnerResponse::Kept) => {
                self.publish(e, "overlay", None, "kept", "kept", None, None);
            }
            Some(other) => {
                tracing::info!(entity = %e, answer = ?other, "refine_propose made no proposal");
                self.publish(e, "overlay", None, "kept", "no_candidate", None, None);
            }
            None => {}
        }
    }

    /// After a propose timed out: probes lg's loop with `reload` (a loop
    /// job, idempotent on an unchanged config) until one answers, for at
    /// most `refine_drain_secs`. Returns whether one answered.
    async fn drain(&self, socket: &Path) -> bool {
        let deadline = tokio::time::Instant::now() + self.refine_drain;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            let wait = left.min(self.drain_every);
            match owner::request(socket, &OwnerRequest::Reload, wait).await {
                Ok(_) => return true,
                Err(OwnerError::Timeout) => {}
                Err(_) => {
                    // The daemon is gone: nothing runs in its loop.
                    return true;
                }
            }
        }
    }

    /// The menu ballot on a proposal (9.6): each member picks items; an
    /// item a strict majority picks is selected, in menu order, at most
    /// `max_guardrails`.
    async fn menu_pick(
        &self,
        d: &dyn OwnedDaemons,
        e: &EntityId,
        socket: &Path,
        run: &mut Run,
        evidence: &str,
        now: u64,
    ) {
        let current: Vec<String> = match &run.row.overlay.active {
            Some(Authored {
                source: OverlaySource::Menu { ids },
                ..
            }) => ids.clone(),
            _ => Vec::new(),
        };
        let Some((picks, reasons)) = self.menu_votes(d, e, &current, evidence).await else {
            return;
        };
        let members = self.cfg.council_members;
        run.row.ballots.cast(now);
        run.dirty = true;
        let max = usize::try_from(self.cfg.max_guardrails).unwrap_or(usize::MAX);
        let ids: Vec<String> = self
            .menu
            .iter()
            .filter(|s| vet::council_majority(picks.get(&s.id).copied().unwrap_or(0), members))
            .take(max)
            .map(|s| s.id.clone())
            .collect();
        let event = |action: &str, code: &str| StewardEvent {
            entity: e.clone(),
            item: "overlay".into(),
            id: None,
            action: action.into(),
            code: code.into(),
            votes: None,
            text: Some(ids.join(", ")),
            reasons: reasons.clone(),
        };
        if ids == current {
            self.publish_full(event("kept", "kept"));
            return;
        }
        if ids.is_empty() {
            // No item now: back to the baseline.
            let (Some(b), pointer) = (run.row.overlay.baseline, run.row.overlay.pointer) else {
                return;
            };
            self.publish_full(event("voted", "clear"));
            if self
                .activate(e, socket, run, b, pointer, Purpose::Clear, now)
                .await
                == Activated::Done
            {
                self.note_plain(e, "overlay_off");
            }
            return;
        }
        let text = vet::render_guardrails(&self.menu, &ids);
        let digest = vet::overlay_digest(&text);
        if run.row.is_vetoed(digest) {
            self.publish_full(event("declined", "vetoed"));
            return;
        }
        self.publish_full(event("voted", "menu"));
        let source = OverlaySource::Menu { ids };
        let ov = &run.row.overlay;
        let reuse = ov
            .authored
            .iter()
            .rev()
            .find(|a| a.digest == digest && a.source == source && !ov.late.contains(&a.version))
            .map(|a| a.version);
        let v = match reuse {
            Some(v) => v,
            None => match self.put(e, socket, run, source.clone(), now).await {
                Some(v) => v,
                None => return,
            },
        };
        self.chain_text(
            e,
            socket,
            run,
            Authored {
                version: v,
                digest,
                source,
            },
            now,
        )
        .await;
    }

    /// Each member's menu picks on `evidence`, counted per item, and the
    /// reasons; `None` without a judge. A failed vote picks nothing.
    async fn menu_votes(
        &self,
        d: &dyn OwnedDaemons,
        e: &EntityId,
        current: &[String],
        evidence: &str,
    ) -> Option<(BTreeMap<String, u32>, Vec<String>)> {
        let judge = self.judge.as_ref()?;
        let brief = d.brief(e).unwrap_or_else(|| PersonaText {
            name: String::new(),
            persona: String::new(),
            goal: String::new(),
            body: String::new(),
        });
        let mut picks: BTreeMap<String, u32> = BTreeMap::new();
        let mut reasons = Vec::new();
        for member in 0..self.cfg.council_members {
            let ballot = Ballot::menu(member, &brief, &self.menu, current, evidence);
            match judge.vote(&ballot).await {
                Ok((mut vote, usage)) => {
                    self.sup.record_spend(e, usage);
                    // One member counts once per item.
                    vote.ids.sort();
                    vote.ids.dedup();
                    for id in vote.ids {
                        *picks.entry(id).or_default() += 1;
                    }
                    reasons.extend(vote.reason);
                }
                Err(err) => {
                    tracing::warn!(entity = %e, error = %err, "a council vote failed; it picks nothing");
                }
            }
        }
        Some((picks, reasons))
    }

    /// Queues an overlay note for the agent's next wake; it carries no
    /// text.
    fn note_plain(&self, e: &EntityId, code: &str) {
        self.sup.steward_note(
            e,
            StewardNote {
                code: code.to_owned(),
                goal: None,
                title: None,
            },
        );
    }

    /// What the observer shows of `run`'s overlays: lg's pointer, the
    /// baseline, the pause, and the Steward's versions with their full
    /// text, newest first.
    pub(super) fn overlay_view(&self, run: &Run) -> super::OverlayView {
        let ov = &run.row.overlay;
        super::OverlayView {
            mode: match self.cfg.refine {
                crate::littleguy::RefineMode::Free => "free",
                _ => "menu",
            }
            .to_owned(),
            pointer: ov.pointer,
            baseline: ov.baseline,
            source: ov.active.as_ref().map(|a| a.source.label()),
            paused: ov.paused.as_ref().map(|p| p.code.clone()),
            chain: ov
                .authored
                .iter()
                .rev()
                .map(|a| super::ChainEntry {
                    version: a.version,
                    label: a.source.label(),
                    text: self.source_text(&a.source),
                    active: ov.pointer == Some(a.version),
                })
                .collect(),
        }
    }
}
