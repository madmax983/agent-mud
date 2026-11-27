//! Step 5 of a pass: refine (ADR 0015, phase 4; free text, ADR 0016).
//!
//! lg's proposer (`refine_propose`) reads an agent's recent play and
//! writes a revision of its overlay. The Steward never activates that
//! proposal. In `menu` mode it is only evidence: the council picks
//! operator menu items from it, and the Steward puts and activates
//! `{"guardrails": <the picked items>}` itself.
//!
//! In `free` mode (ADR 0016), and never for a Turing-tier entity, the
//! Steward takes only the proposal's `guardrails` string, strips the
//! preamble it wrote before, and runs the deterministic guard
//! ([`vet::FreeGuard::check`]). A candidate that passes goes to every
//! council member as the exact lines; only a unanimous council of at least
//! [`vet::FREE_MIN_MEMBERS`] activates it ([`vet::overlay_verdict`]). The
//! Steward puts `{"guardrails": preamble + lines}` and nothing else. An
//! active free text that comes to name an entity (a player registers a
//! matching name) is revoked on the next pass, even at a hard budget: a
//! Clear costs no tokens. When free text is no longer the mode for an
//! entity (the operator set `menu`, or it is Turing-tier), the next pass
//! withdraws its free text the same way, with no ballot and no veto.
//!
//! The Steward keeps its own view of lg's active version (`pointer`) and
//! checks every move. Each `overlay_activate` names the pointer it read
//! (`expected`: lg's `active` and `head`, littleguy #122), and lg moves
//! the pointer only when nothing moved since, checked in its swap
//! transaction:
//!
//! - **The pointer check.** Every pass reads `overlays`. A version the
//!   Steward did not set and did not give up on is a human's move (lg's
//!   Roll back button, or its refine tab): refine pauses for that entity,
//!   the Steward's text is vetoed for good, and the Steward never moves the
//!   pointer away from the human's choice. Only the operator resumes
//!   (`resume_refine`).
//! - **The head of its own move** (littleguy #124). The row keeps, next to
//!   the pointer, the head of lg's pointer that the Steward holds as its
//!   own (`pointer_head`): the head of the Steward's last guarded move.
//!   lg's `overlay_active` names it. When it names none (lg #122, or a
//!   move by another process right after the swap, said once per daemon
//!   start as `no_head old_lg`), the guard implies it: lg checked the read
//!   pointer in its swap, so the move was the first after the read, one
//!   past its head ([`vet::one_move`]). Each pass, and the re-read before
//!   each activate, compares it with the head `overlays` names
//!   ([`vet::head_verdict`]). The same version at another head is a move
//!   away and back since the Steward's own move: a human's veto, late list
//!   or not, since lg logs no move to the active version. Refine pauses
//!   (`moved away_and_back`, then `paused rolled_back`), the text the
//!   human returned to is theirs and stays, a late text sent from the
//!   Steward's pointer, which may have run and been rolled back, is
//!   vetoed, and the pointer is never moved. A late activate of another
//!   version stays under the late-list rules below. A head below the
//!   Steward's own, or none, is a reset store ([`vet::head_went_back`]).
//! - **Adoption.** The row knows no head of its own in a row from before
//!   #124, after an unguarded or lost answer, and after a store reset.
//!   Then a pass whose read agrees with the pointer takes its head as the
//!   Steward's own. That read races a person: a move away and back after
//!   the Steward's move and before that read is taken as the Steward's
//!   own. That is exactly the version check every pass made before #124,
//!   so adoption misses nothing that check saw. It misses only a move away
//!   and back before the first read that names a head; every move after
//!   that read shows. A guarded move has no such race: its head is lg's
//!   answer or the one its guard implies, never a later move's.
//! - **lg names no head** (an lg before #122, or after a downgrade): the
//!   check is by version alone, as before #124, and nothing is adopted.
//! - **Restore.** An undo (of a late activate, or of a lost text that lg
//!   shows applied) re-reads the pointer right before, like an activate,
//!   and needs it exactly as the pointer check judged it, head included.
//!   The same version at another head is a person's later choice of it:
//!   it stands, nothing moves, and refine pauses (`unconfirmed`).
//! - **Activate.** It re-reads the pointer right before, writes the intent
//!   (with the head it read) durably, and sends the pointer it read as
//!   `expected`. `overlay_active` with `guarded` is exactly the move meant.
//!   `overlay_moved` is a move after the read, a human's veto: nothing
//!   changed on lg, so nothing is restored, nothing is sent again, and
//!   refine pauses as for any human move. The text about to go live is
//!   vetoed with the one the human left; a text the human lands on or
//!   returns to is theirs and stays. A lost answer is never sent again:
//!   lg's pointer settles it ([`vet::lost_verdict`]). The head unmoved:
//!   not applied, and the version goes on the late list with the head it
//!   sent. The head moved and lg back on the read version: a move away and
//!   back, a human's veto. lg numbers each move one past the last, and a
//!   guarded activate applies only as the first move after its read
//!   ([`vet::one_move`]). So lg on the activate's version two or more moves
//!   on is a human's later choice of it, which stands. One move on, it is
//!   applied or a human's one move to it, which the protocol cannot tell
//!   apart, so the fail-safe below decides (a move to `{}` is adopted, a
//!   text is undone and refine pauses, `unconfirmed`).
//! - **A read that named no head** came from an lg before #122: the intent
//!   records no guard, and a lost answer settles by version. A guard counts
//!   at settlement only when lg names a head again, so a daemon that
//!   changed its build in between is never taken for a human's move.
//! - **An older lg.** An lg before #122 drops `expected` and moves the
//!   pointer unchecked; its answer has no `guarded` (said once per daemon
//!   start). For it the Steward checks the `previous` lg reports: a human
//!   move in between is restored at once, then refine pauses. Its listing
//!   has no head, so a lost answer is settled by the version alone.
//! - **One move above the baseline.** First contact puts and activates an
//!   empty `{}` baseline (or pauses as `preexisting` when lg already has an
//!   active overlay). Every text goes on from the baseline, so one Roll
//!   back after a text lands on `{}`.
//! - **lg's store, not the row's numbers.** lg numbers versions per store.
//!   A listing that lacks a version the row holds, lists one of the
//!   Steward's under another author, or ends below `known_max` means the
//!   store was reset: the row forgets it and starts from first contact.
//!   A version put earlier is shown (`overlay_show`) and matched before it
//!   is activated again.
//! - **Late is bounded.** An activate given up on stays late until the
//!   daemon restarts or its window ends, and a guarded one only while lg
//!   can still run it: the head it sent is lg's head. lg on a late version
//!   counts as the Steward's own only one move past that head. A late move
//!   to `{}` while a text of the Steward's is active looks like a human's
//!   Roll back, so it is taken as one.

use std::collections::BTreeMap;
use std::path::Path;

use mud_world::EntityId;
use mud_world::pack::Tier;
use serde_json::Value;

use super::council::Ballot;
use super::state::{Authored, Guard, IntentAction, OverlayRow, OverlaySource, Pause, Purpose};
use super::vet::{self, EntityName, LostVerdict, OverlayVerdict, PointerVerdict};
use super::{OwnedDaemons, Run, Steward, StewardEvent, StewardNote, StewardPoint};
use crate::budget::BudgetLevel;
use crate::littleguy::owner::{
    self, ExpectedPointer, OverlayEntry, OwnerError, OwnerRequest, OwnerResponse,
};
use crate::littleguy::{PersonaText, RefineMode};
use crate::mind::Usage;
use crate::runtime::Supervisor;

/// The author lg records for every put and activate of the MUD.
pub const MUD_AUTHOR: &str = "operator:mud";

/// A late activate lives this many `activate_wait_secs`: past that, lg's
/// loop is taken to have dropped it, and a move to it is a human's.
pub const LATE_WAITS: u64 = 6;

/// What `overlays` answered.
#[derive(Debug, Clone)]
pub(super) struct Overlays {
    /// lg's active version.
    pub active: Option<u64>,
    /// The `seq` of lg's last pointer move, read with `active`; `None`
    /// before the first move and from an lg before #122.
    pub head: Option<u64>,
    /// Whether lg named `head` (lg #122 always does, `null` included). Only
    /// a named head guards an activate.
    pub named: bool,
    /// Every version, oldest first.
    pub entries: Vec<OverlayEntry>,
}

impl Overlays {
    /// lg's pointer as read.
    pub const fn seen(&self) -> Seen {
        Seen {
            active: self.active,
            head: self.head,
            named: self.named,
        }
    }
}

/// lg's pointer as one answer named it: from `overlays`, or from an
/// `overlay_moved` refusal (only lg #122 sends one, so its head is named).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Seen {
    /// lg's active version.
    pub active: Option<u64>,
    /// The `seq` of its last move.
    pub head: Option<u64>,
    /// Whether lg named the head.
    pub named: bool,
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
    /// `overlay_active`; `guarded` when lg checked the expected pointer,
    /// and `head` when lg named the head of its move (lg #124).
    Answer {
        version: u64,
        previous: Option<u64>,
        guarded: bool,
        head: Option<u64>,
    },
    /// `overlay_moved`: the pointer moved after the read, to `active` at
    /// `head`, and lg moved nothing.
    Moved {
        active: Option<u64>,
        head: Option<u64>,
    },
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
    /// An activate given up on applied late: undo it, back to `to`. `seen`
    /// is lg's pointer as the pointer check judged it, on the late version.
    Undo { to: u64, late: u64, seen: Seen },
}

fn as_id(version: u64) -> Option<i64> {
    i64::try_from(version).ok()
}

/// The guard an activate from the pointer `read` sends: its head, when lg
/// named one and has not answered as an lg before #122 since it started.
/// An older lg drops `expected`, so its intent records no guard, and a
/// lost answer is settled by the version alone.
fn guard_of(run: &Run, read: Seen) -> Option<Guard> {
    (read.named && !run.build.old_lg).then_some(Guard { head: read.head })
}

/// The head of a guarded move to `v` from the pointer `read`, when lg's
/// answer names none (lg #122, or a move by another process right after
/// the swap, lg #124). lg checked `read` in its swap, so the move was the
/// first one after it, and lg numbers each move one past the last
/// ([`vet::one_move`]). A move to the active version logs none: the head
/// stays. A read that named no head implies none.
const fn guarded_head(read: Seen, v: u64) -> Option<u64> {
    if !read.named {
        return None;
    }
    match (read.active, read.head) {
        (Some(a), head) if a == v => head,
        (_, Some(h)) => Some(h.saturating_add(1)),
        (_, None) => Some(1),
    }
}

/// The pointer check's verdict on lg's pointer `seen` against the row
/// ([`vet::head_verdict`]): the version, and the head of the Steward's own
/// last move when lg names a head and the row knows its own. A late
/// version of the Steward's counts as its own only as lg could have run
/// it: a guarded one only one move past its read ([`OverlayRow::own_late`]).
fn verdict(ov: &OverlayRow, seen: Seen) -> PointerVerdict {
    let late = seen
        .active
        .is_some_and(|v| ov.own_late(v, seen.head, seen.named));
    vet::head_verdict(
        seen.active,
        seen.head,
        seen.named,
        ov.pointer,
        ov.pointer_head,
        late,
    )
}

/// Vetoes the text of `purpose`, if it is one: it is never put again.
fn veto_text(run: &mut Run, purpose: &Purpose) {
    if let Purpose::Text { digest, .. } = purpose {
        run.row.veto(*digest);
    }
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
        if run
            .row
            .intent
            .as_ref()
            .is_some_and(|i| i.action.is_overlay())
        {
            // A reset store first: an open intent names versions of the
            // old one.
            let Some(ov) = self.read_overlays(socket).await else {
                return;
            };
            self.check_store(e, run, &ov, now);
        }
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
        self.check_store(e, run, &ov, now);
        if run.row.overlay.expire_late(now, self.late_window_ms()) {
            run.dirty = true;
        }
        // Versions up to here are known: a lost put is looked for above.
        if let Some(max) = ov.entries.iter().map(|x| x.version).max()
            && run.row.overlay.known_max.is_none_or(|k| k < max)
        {
            run.row.overlay.see(max);
            run.dirty = true;
        }
        let next = self.judge_pointer(e, run, ov.seen(), now);
        // A guarded late activate applies only as the next move after its
        // read: once the head moved past that, lg refuses it. Pruned after
        // the pointer check, which vetoes a late text that may have run
        // and been rolled back.
        if run.row.overlay.prune_late(ov.active, ov.head, ov.named) {
            run.dirty = true;
        }
        match next {
            Next::Nothing => {}
            Next::Baseline if can_act => {
                self.start_baseline(e, socket, run, now).await;
                return;
            }
            Next::Undo { to, late, seen } if can_act => {
                run.row.overlay.drop_late(late);
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
                self.restore(e, socket, run, to, seen, now).await;
                return;
            }
            Next::Baseline | Next::Undo { .. } => return,
        }
        let ov = &run.row.overlay;
        // A Clear costs no tokens: the Steward withdraws its own text even
        // at a hard budget. It never moves a paused pointer.
        let can_clear = run.row.intent.is_none() && run.row.stuck_until_ms.is_none();
        if !can_clear || ov.paused.is_some() || ov.baseline.is_none() || ov.pointer.is_none() {
            return;
        }
        if self.revoke_if_named(e, socket, run, now).await
            || self.withdraw_if_not_free(e, socket, run, now).await
            || !can_act
        {
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

    /// How long a late activate stays late, in game-clock ms.
    const fn late_window_ms(&self) -> u64 {
        self.cfg
            .activate_wait_secs
            .saturating_mul(1_000)
            .saturating_mul(LATE_WAITS)
    }

    /// Checks lg's listing against the versions the row holds. A missing
    /// one, one of the Steward's under another author, a highest version
    /// below `known_max`, or a head below the Steward's own (or none)
    /// means lg's store was reset (its data deleted): the row forgets it.
    /// Returns whether it did.
    fn check_store(&self, e: &EntityId, run: &mut Run, ov: &Overlays, now: u64) -> bool {
        let listed: BTreeMap<u64, &str> = ov
            .entries
            .iter()
            .map(|x| (x.version, x.author.as_str()))
            .collect();
        let row = &run.row.overlay;
        let max = listed.keys().next_back().copied();
        let shrank = row.known_max.is_some_and(|k| max.is_none_or(|m| m < k));
        let gone = row.recorded().iter().any(|(v, ours)| {
            listed
                .get(v)
                .is_none_or(|author| *ours && *author != MUD_AUTHOR)
        });
        // The head only rises in one store: below the Steward's own, or
        // gone, it is another store, even with the same version numbers.
        let back = vet::head_went_back(row.pointer_head, ov.head, ov.named);
        if shrank || gone || back {
            self.store_reset(e, run, now);
        }
        shrank || gone || back
    }

    /// lg's overlay store is not the one the row knows: forget it (the
    /// pause and the veto list stay) and start again from first contact.
    fn store_reset(&self, e: &EntityId, run: &mut Run, now: u64) {
        tracing::warn!(entity = %e, "lg's overlay store was reset; refine starts again from first contact");
        run.row.overlay.forget_store();
        if let Some(i) = run.row.intent.take_if(|i| i.action.is_overlay())
            && i.action == IntentAction::Propose
        {
            run.row.overlay.last_refine_ms = Some(now);
            run.row.wakes_since_refine = 0;
        }
        run.dirty = true;
        self.publish(e, "overlay", None, "reset", "store_reset", None, None);
        self.write(e, run, true);
    }

    /// Whether lg's `version` holds exactly the guardrails `text` (`""`
    /// for the baseline). Checked before a version put earlier is
    /// activated again. Another text there means the store was reset: the
    /// row forgets it. No answer confirms nothing.
    async fn confirm(
        &self,
        e: &EntityId,
        socket: &Path,
        run: &mut Run,
        version: u64,
        text: &str,
        now: u64,
    ) -> bool {
        let req = OwnerRequest::OverlayShow { version };
        match owner::request(socket, &req, self.read_timeout).await {
            Ok(OwnerResponse::Overlay { overlay, .. }) => {
                let same = vet::overlay_matches(&overlay, text);
                if !same {
                    self.store_reset(e, run, now);
                }
                same
            }
            other => {
                tracing::warn!(entity = %e, version, answer = ?other, "an overlay version could not be shown; it is not activated");
                false
            }
        }
    }

    /// With refine off, nothing settles an open overlay intent, and it
    /// would hold the goal step forever: give it up. lg's pointer is not
    /// read, so an activate goes on the late list.
    pub(super) fn refine_off(&self, e: &EntityId, run: &mut Run, now: u64) {
        let Some(intent) = run.row.intent.take_if(|i| i.action.is_overlay()) else {
            return;
        };
        let id = match intent.action {
            IntentAction::Activate { version, guard, .. } => {
                run.row.overlay.add_late(version, now, guard);
                as_id(version)
            }
            IntentAction::Propose => {
                run.row.overlay.last_refine_ms = Some(now);
                run.row.wakes_since_refine = 0;
                None
            }
            _ => None,
        };
        run.dirty = true;
        self.publish(e, "overlay", id, "stuck", "refine_off", None, None);
    }

    /// lg's overlay listing; `None` when it does not answer (or refine is
    /// off in that daemon).
    pub(super) async fn read_overlays(&self, socket: &Path) -> Option<Overlays> {
        match owner::request(socket, &OwnerRequest::Overlays, self.read_timeout).await {
            Ok(OwnerResponse::Overlays {
                active,
                head,
                entries,
            }) => Some(Overlays {
                active,
                head: head.flatten(),
                named: head.is_some(),
                entries,
            }),
            other => {
                tracing::debug!(answer = ?other, "no overlay listing");
                None
            }
        }
    }

    /// The pointer check (9.2) on lg's pointer `seen` ([`verdict`]):
    /// settles what it can in the row, and says what move is needed next.
    /// The same version at another head than the Steward's own is a move
    /// away and back since its own move ([`Self::moved_since`]).
    fn judge_pointer(&self, e: &EntityId, run: &mut Run, seen: Seen, now: u64) -> Next {
        let lg = seen.active;
        let ov = &run.row.overlay;
        let expected = ov.pointer;
        let fresh = expected.is_none()
            && ov.baseline.is_none()
            && ov.authored.is_empty()
            && ov.paused.is_none();
        match verdict(ov, seen) {
            PointerVerdict::Same => {
                if ov.pointer_head.is_none() && seen.head.is_some() {
                    // Adoption: the Steward knows no head of its own (a row
                    // from before lg #124, or an answer that named none),
                    // so it takes the head of a read that agrees.
                    run.row.overlay.pointer_head = seen.head;
                    run.dirty = true;
                }
                let ov = &run.row.overlay;
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
                // `OwnLate` is never the version the Steward holds
                // ([`vet::head_verdict`]): lg logs no move to the active
                // version.
                if ov.is_empty_version(v) && ov.active.is_some() {
                    // The late Clear of a text, or a human's Roll back off
                    // it: they look the same, so it is taken as the human's.
                    run.row.overlay.drop_late(v);
                    self.human_moved(e, run, seen, now, "unconfirmed");
                    Next::Nothing
                } else if ov.is_empty_version(v) {
                    // A move to `{}` from `{}` is always safe: adopt it.
                    let ov = &mut run.row.overlay;
                    ov.drop_late(v);
                    ov.pointer = Some(v);
                    ov.pointer_head = seen.head;
                    ov.baseline = ov.baseline.or(Some(v));
                    ov.active = None;
                    ov.rollback_to = None;
                    run.dirty = true;
                    self.publish(e, "overlay", as_id(v), "activated", "baseline", None, None);
                    Next::Nothing
                } else if let Some(to) = expected {
                    Next::Undo { to, late: v, seen }
                } else {
                    self.human_moved(e, run, seen, now, "unconfirmed");
                    Next::Nothing
                }
            }
            PointerVerdict::Human if fresh => {
                // Someone set an overlay before the Steward ever ran: it
                // is never replaced.
                run.row.overlay.pointer = lg;
                run.row.overlay.pointer_head = seen.head;
                self.pause(e, run, now, "preexisting", false);
                Next::Nothing
            }
            PointerVerdict::Human if lg == expected => {
                self.moved_since(e, run, seen, now);
                Next::Nothing
            }
            PointerVerdict::Human => {
                self.human_moved(e, run, seen, now, "rolled_back");
                Next::Nothing
            }
        }
    }

    /// lg is on the Steward's own version at another head than its own
    /// move's (lg #124): someone moved the pointer since, and back. Only a
    /// human moves it but the Steward, and none of the Steward's moves
    /// explains it, so it is a human's veto, as [`Self::away_and_back`]
    /// after a read: refine pauses, and the text the human returned to is
    /// theirs and stays. A late text the Steward sent from its own pointer
    /// may have run and been rolled back in between, so it is vetoed.
    fn moved_since(&self, e: &EntityId, run: &mut Run, seen: Seen, now: u64) {
        let own = run.row.overlay.pointer_head;
        if run.row.overlay.paused.is_none() {
            tracing::info!(entity = %e, version = ?seen.active, own_head = ?own, head = ?seen.head, "lg's overlay pointer moved away and back since the Steward's own move");
            self.publish(
                e,
                "overlay",
                seen.active.and_then(as_id),
                "moved",
                "away_and_back",
                None,
                None,
            );
        }
        self.away_and_back(e, run, own, seen.head, &Purpose::Restore, now);
    }

    /// A human moved lg's pointer to `seen`: the Steward's text they moved
    /// away from is vetoed and refine pauses. A text they land on is their
    /// choice and stays. While paused, a later move is adopted and says
    /// nothing new. The Steward never moves the pointer back. The row takes
    /// the head `seen` named as its own: the next move after it shows.
    pub(super) fn human_moved(
        &self,
        e: &EntityId,
        run: &mut Run,
        seen: Seen,
        now: u64,
        code: &str,
    ) {
        let lg = seen.active;
        let ov = &mut run.row.overlay;
        if ov.paused.is_some() {
            if ov.pointer != lg || ov.pointer_head != seen.head {
                ov.pointer = lg;
                ov.pointer_head = seen.head;
                ov.active = ov.text_at(lg);
                ov.rollback_to = None;
                run.dirty = true;
            }
            return;
        }
        if let Some(a) = ov.active.take()
            && Some(a.version) != lg
        {
            run.row.veto(a.digest);
        }
        let ov = &mut run.row.overlay;
        ov.pointer = lg;
        ov.pointer_head = seen.head;
        ov.rollback_to = None;
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
                    && !ov.is_late(a.version)
                    && Some(a.version) != from
            })
            .map(|a| a.version);
        let b = match reuse {
            Some(b) => {
                if !self.confirm(e, socket, run, b, "", now).await {
                    return;
                }
                b
            }
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
            OverlaySource::Free { lines } => vet::render_free(lines),
        }
    }

    /// Every entity of the world, players included, by name and aliases:
    /// what a free text must never name.
    fn entity_names(&self) -> Vec<EntityName> {
        self.sup.game().read(|w| {
            w.state()
                .entities
                .values()
                .map(|x| EntityName {
                    name: x.name.clone(),
                    aliases: x.aliases.clone(),
                })
                .collect()
        })
    }

    /// Whether `e` is a Turing-tier entity. Read here and never shown: a
    /// hidden PC's habits come from the operator menu only.
    fn is_turing(&self, e: &EntityId) -> bool {
        self.sup
            .game()
            .read(|w| w.entity(e).is_some_and(|x| x.tier == Tier::Turing))
    }

    /// Revokes a free text of the Steward's that names an entity now (9.5
    /// step 7): a pending one is dropped, an active one is cleared back to
    /// the baseline; either is vetoed for good. This is the Steward
    /// withdrawing its own text, not a pause. Returns whether it moved.
    async fn revoke_if_named(&self, e: &EntityId, socket: &Path, run: &mut Run, now: u64) -> bool {
        let hits = |a: &Authored, cast: &[EntityName]| {
            a.source
                .free_lines()
                .is_some_and(|lines| vet::names_entity(lines, cast))
        };
        let (active, pending) = (&run.row.overlay.active, &run.row.overlay.pending);
        if !active
            .iter()
            .chain(pending)
            .any(|a| a.source.free_lines().is_some())
        {
            return false;
        }
        let names = self.entity_names();
        if let Some(p) = run.row.overlay.pending.take_if(|p| hits(p, &names)) {
            run.row.veto(p.digest);
            run.dirty = true;
            self.write(e, run, true);
            self.revoked_event(e, &p);
        }
        let Some(a) = run.row.overlay.active.clone().filter(|a| hits(a, &names)) else {
            return false;
        };
        let Some(b) = run.row.overlay.baseline else {
            return false;
        };
        run.row.veto(a.digest);
        run.dirty = true;
        self.write(e, run, true);
        self.revoked_event(e, &a);
        let pointer = run.row.overlay.pointer;
        if self.clear(e, socket, run, b, pointer, now).await {
            self.note_plain(e, "overlay_off");
        }
        true
    }

    /// Whether free text is the mode for `e`: `refine = "free"`, and `e`
    /// is not Turing-tier.
    fn free_mode(&self, e: &EntityId) -> bool {
        self.cfg.refine == RefineMode::Free && !self.is_turing(e)
    }

    /// Withdraws a free text of the Steward's when free text is no longer
    /// the mode for `e` (the operator set `refine = "menu"`, or `e` is
    /// Turing-tier): a pending one is dropped, an active one is cleared
    /// back to the baseline at once, with no ballot. Neither is vetoed: the
    /// text was not refused, the mode changed. Returns whether it moved.
    async fn withdraw_if_not_free(
        &self,
        e: &EntityId,
        socket: &Path,
        run: &mut Run,
        now: u64,
    ) -> bool {
        let is_free = |a: &Authored| a.source.free_lines().is_some();
        let ov = &run.row.overlay;
        if !(ov.active.as_ref().is_some_and(is_free) || ov.pending.as_ref().is_some_and(is_free))
            || self.free_mode(e)
        {
            return false;
        }
        if let Some(p) = run.row.overlay.pending.take_if(|p| is_free(p)) {
            run.dirty = true;
            self.withdrawn_event(e, &p);
        }
        let Some(a) = run.row.overlay.active.clone().filter(is_free) else {
            return false;
        };
        let Some(b) = run.row.overlay.baseline else {
            return false;
        };
        self.withdrawn_event(e, &a);
        let pointer = run.row.overlay.pointer;
        if self.clear(e, socket, run, b, pointer, now).await {
            self.note_plain(e, "overlay_off");
        }
        true
    }

    fn withdrawn_event(&self, e: &EntityId, a: &Authored) {
        tracing::info!(entity = %e, version = a.version, "free text is not the refine mode now; the Steward withdraws its text");
        self.publish_full(StewardEvent {
            entity: e.clone(),
            item: "overlay".into(),
            id: as_id(a.version),
            action: "withdrawn".into(),
            code: "mode_menu".into(),
            votes: None,
            text: Some(a.text.clone()),
            reasons: Vec::new(),
        });
    }

    fn revoked_event(&self, e: &EntityId, a: &Authored) {
        tracing::info!(entity = %e, version = a.version, "a free text names an entity now; it is revoked");
        self.publish_full(StewardEvent {
            entity: e.clone(),
            item: "overlay".into(),
            id: as_id(a.version),
            action: "revoked".into(),
            code: "names_entity".into(),
            votes: None,
            text: Some(a.text.clone()),
            reasons: Vec::new(),
        });
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
                    text: text.to_owned(),
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
        // 1. Re-read right before: a human move since the pass began, a
        // move away and back included, is the pointer check's, and nothing
        // is sent.
        let Some(ov) = self.read_overlays(socket).await else {
            return Activated::Not;
        };
        if ov.active != from || verdict(&run.row.overlay, ov.seen()) != PointerVerdict::Same {
            let _ = self.judge_pointer(e, run, ov.seen(), now);
            return Activated::Not;
        }
        // 2 and 3. The intent, then the activate, guarded by the pointer
        // just read.
        let read = ov.seen();
        let guard = guard_of(run, read);
        match self
            .send_activate(e, socket, run, v, read, purpose.clone(), now)
            .await
        {
            Sent::Answer {
                guarded: true,
                head,
                ..
            } => {
                // lg checked the pointer in its swap: the move was exactly
                // from `from`, and nothing came in between. Its head is the
                // Steward's own: the one lg names (lg #124), or else the
                // one the guard implies.
                self.no_head(e, run, head);
                let head = head.or_else(|| guarded_head(read, v));
                self.applied(e, run, v, from, head, &purpose);
                Activated::Done
            }
            Sent::Answer {
                version,
                previous,
                guarded: false,
                head,
            } => {
                // An lg before #122 moved it unchecked: check `previous`.
                self.unguarded(e, run);
                if vet::pointer_verdict(previous, from, false) == PointerVerdict::Same {
                    self.applied(e, run, v, from, head, &purpose);
                    return Activated::Done;
                }
                // A human moved the pointer to `previous` between the
                // re-read and lg running ours, and ours overwrote it.
                // Restore it, then pause.
                // The human moved away from the text at `from` (if one of
                // the Steward's) and never chose this one: both are vetoed.
                veto_text(run, &purpose);
                if let Some(a) = run.row.overlay.active.take() {
                    run.row.veto(a.digest);
                }
                run.row.overlay.pointer = Some(version);
                run.row.overlay.pointer_head = head;
                run.row.overlay.rollback_to = None;
                run.dirty = true;
                let ours = Seen {
                    active: Some(version),
                    head,
                    named: head.is_some(),
                };
                match previous {
                    Some(p) => {
                        if self.restore(e, socket, run, p, ours, now).await {
                            self.pause(e, run, now, "rolled_back", true);
                        }
                    }
                    None => self.pause(e, run, now, "rolled_back", true),
                }
                Activated::Not
            }
            Sent::Moved { active, head } => {
                let seen = Seen {
                    active,
                    head,
                    named: true,
                };
                self.moved(e, run, v, from, guard, &purpose, seen, now);
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
                self.settle_lost(e, socket, run, v, from, guard, purpose, now)
                    .await
            }
            Sent::NotWritten => Activated::Not,
        }
    }

    /// lg refused an activate of `v` from `from` with `overlay_moved`: its
    /// pointer moved after the Steward read it, and is at `seen` now.
    /// Nothing changed on lg, so nothing is restored and nothing is sent
    /// again.
    ///
    /// - The read named no head (`guard` is `None`): it came from an lg
    ///   before #122, and only lg #122 answers `overlay_moved`. The daemon
    ///   changed its build between the read and the send. Back on `from`,
    ///   nothing else moved for the Steward to tell: the next pass reads
    ///   again.
    /// - Back on `from`: a move away and back ([`Self::away_and_back`]).
    /// - Any other version goes to the pointer check: a human's veto, and
    ///   the text that was about to go live is vetoed too, unless it is a
    ///   late activate of the Steward's own that ran in between. The undo
    ///   of that late version waits for the next pass's pointer check on
    ///   purpose: a pass makes one move, and the late entry stays until
    ///   then.
    #[expect(
        clippy::too_many_arguments,
        reason = "one move's context, passed down rather than stored"
    )]
    fn moved(
        &self,
        e: &EntityId,
        run: &mut Run,
        v: u64,
        from: Option<u64>,
        guard: Option<Guard>,
        purpose: &Purpose,
        seen: Seen,
        now: u64,
    ) {
        let lg = seen.active;
        tracing::info!(entity = %e, version = v, active = ?lg, "lg's overlay pointer moved after the Steward read it; nothing was activated");
        self.publish(e, "overlay", as_id(v), "moved", "overlay_moved", None, None);
        match guard {
            None if lg == from => {
                tracing::info!(entity = %e, "this lg names a head its listing before did not: the daemon changed its build");
                return;
            }
            Some(g) if lg == from => {
                self.away_and_back(e, run, g.head, seen.head, purpose, now);
                return;
            }
            _ => {}
        }
        let ov = &run.row.overlay;
        let late = lg.is_some_and(|x| ov.own_late(x, seen.head, seen.named));
        if lg != Some(v) && vet::pointer_verdict(lg, ov.pointer, late) == PointerVerdict::Human {
            veto_text(run, purpose);
        }
        let _ = self.judge_pointer(e, run, seen, now);
    }

    /// lg is back on the version an activate read, and the head moved since
    /// `read_head` to `head`: a move away and back after the read. A human
    /// did it, or a late text of the Steward's from that same read ran in
    /// between and a human rolled it back; lg cannot say which. Refine
    /// pauses. The text at `from` is the human's choice now and is never
    /// vetoed. The text of `purpose` and every late text from that read,
    /// which may have run and been rolled back, are vetoed. A late entry
    /// from that read can no longer apply: it goes. The row takes `head`
    /// as its own, so the next move shows.
    fn away_and_back(
        &self,
        e: &EntityId,
        run: &mut Run,
        read_head: Option<u64>,
        head: Option<u64>,
        purpose: &Purpose,
        now: u64,
    ) {
        if run.row.overlay.pointer_head != head {
            run.row.overlay.pointer_head = head;
            run.dirty = true;
        }
        if run.row.overlay.paused.is_some() {
            return;
        }
        let from_read: Vec<u64> = run
            .row
            .overlay
            .late
            .iter()
            .filter(|l| l.guard.is_some_and(|g| g.head == read_head))
            .map(|l| l.version)
            .collect();
        for x in from_read {
            if let Some(a) = run.row.overlay.text_at(Some(x)) {
                run.row.veto(a.digest);
            }
            run.row.overlay.drop_late(x);
        }
        veto_text(run, purpose);
        run.row.overlay.rollback_to = None;
        run.dirty = true;
        self.pause(e, run, now, "rolled_back", true);
    }

    /// An lg before littleguy #122 answered an activate without `guarded`:
    /// it dropped `expected` and moved the pointer unchecked. The Steward
    /// says so once per daemon start, and checks `previous` instead.
    fn unguarded(&self, e: &EntityId, run: &mut Run) {
        if std::mem::replace(&mut run.build.old_lg, true) {
            return;
        }
        tracing::warn!(entity = %e, "this lg moves its overlay pointer unchecked (it predates littleguy #122); the Steward checks the previous version it reports instead");
        self.publish(e, "overlay", None, "unguarded", "old_lg", None, None);
    }

    /// A guarded answer named no `head` of its move: an lg before
    /// littleguy #124, or a move lg could not tell apart from another
    /// process's. The Steward takes the head its guard implies
    /// ([`guarded_head`]), so nothing goes unseen. It says so once per
    /// daemon start.
    fn no_head(&self, e: &EntityId, run: &mut Run, head: Option<u64>) {
        if head.is_some() || std::mem::replace(&mut run.build.no_head, true) {
            return;
        }
        tracing::warn!(entity = %e, "this lg named no head of its overlay move (it predates littleguy #124, or another process moved right after); the Steward takes the head its guard implies");
        self.publish(e, "overlay", None, "no_head", "old_lg", None, None);
    }

    /// Writes the `Activate` intent and sends it with `expected`, waiting
    /// on the connection up to `activate_wait_secs`. It is never sent
    /// again.
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
        read: Seen,
        purpose: Purpose,
        now: u64,
    ) -> Sent {
        run.row.open(
            IntentAction::Activate {
                version: v,
                expected_previous: read.active,
                guard: guard_of(run, read),
                purpose,
            },
            now,
        );
        let expected = ExpectedPointer {
            active: read.active,
            head: read.head,
        };
        if !self.write(e, run, true) {
            run.row.intent = None;
            return Sent::NotWritten;
        }
        self.point(StewardPoint::IntentWritten, e);
        let req = OwnerRequest::OverlayActivate {
            version: v,
            expected: Some(expected),
        };
        match owner::request(socket, &req, self.activate_wait).await {
            Ok(OwnerResponse::OverlayActive {
                version,
                previous,
                guarded,
                head,
            }) => {
                run.row.intent = None;
                run.dirty = true;
                Sent::Answer {
                    version,
                    previous,
                    guarded,
                    head,
                }
            }
            Ok(OwnerResponse::OverlayMoved { active, head, .. }) => {
                run.row.intent = None;
                run.dirty = true;
                Sent::Moved { active, head }
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

    /// The move of `purpose` to `v` happened as meant; `from` is where lg
    /// was before it, when lg said so (what its Roll back returns to), and
    /// `head` the head of the move, when lg named it (lg #124).
    fn applied(
        &self,
        e: &EntityId,
        run: &mut Run,
        v: u64,
        from: Option<u64>,
        head: Option<u64>,
        purpose: &Purpose,
    ) {
        let ov = &mut run.row.overlay;
        ov.pointer = Some(v);
        ov.pointer_head = head;
        ov.rollback_to = from;
        ov.drop_late(v);
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
            Purpose::Text {
                digest,
                source,
                text,
            } => {
                ov.active = Some(Authored {
                    version: v,
                    digest: *digest,
                    source: source.clone(),
                    text: text.clone(),
                });
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
    /// own move to `judged`, lg's pointer as the caller judged it. Returns
    /// whether lg is back on `to` exactly. It never chains: anything else
    /// adopts lg's pointer and pauses (`unconfirmed`), and a move after its
    /// read is a human's veto.
    ///
    /// The re-read must show `judged` exactly: the same version at another
    /// head is a move away and back since, a human's later choice of that
    /// version, which stands. Heads count only when both reads name one.
    async fn restore(
        &self,
        e: &EntityId,
        socket: &Path,
        run: &mut Run,
        to: u64,
        judged: Seen,
        now: u64,
    ) -> bool {
        let from = judged.active;
        let Some(ov) = self.read_overlays(socket).await else {
            self.pause(e, run, now, "unconfirmed", true);
            return false;
        };
        let moved = judged.named && ov.named && judged.head != ov.head;
        if ov.active != from || moved {
            self.adopt_unconfirmed(e, run, ov.seen(), now);
            return false;
        }
        let read = ov.seen();
        match self
            .send_activate(e, socket, run, to, read, Purpose::Restore, now)
            .await
        {
            Sent::Answer {
                version,
                previous,
                guarded,
                head,
            } => {
                if guarded {
                    self.no_head(e, run, head);
                } else {
                    self.unguarded(e, run);
                }
                // A guarded move is exactly from `from`; an older lg's is
                // checked by its `previous`.
                let exact = version == to
                    && (guarded
                        || vet::pointer_verdict(previous, from, false) == PointerVerdict::Same);
                if exact {
                    let head = if guarded {
                        head.or_else(|| guarded_head(read, to))
                    } else {
                        head
                    };
                    self.applied(e, run, to, from, head, &Purpose::Restore);
                    self.publish(e, "overlay", as_id(to), "restored", "restored", None, None);
                } else {
                    let seen = Seen {
                        active: Some(version),
                        head,
                        named: head.is_some(),
                    };
                    self.adopt_unconfirmed(e, run, seen, now);
                }
                exact
            }
            Sent::Moved { active, head } => {
                // Someone moved it again after the read: their choice
                // stands, and nothing more moves.
                tracing::info!(entity = %e, version = to, ?active, "lg's overlay pointer moved before a restore; nothing was activated");
                self.publish(
                    e,
                    "overlay",
                    as_id(to),
                    "moved",
                    "overlay_moved",
                    None,
                    None,
                );
                let seen = Seen {
                    active,
                    head,
                    named: true,
                };
                self.human_moved(e, run, seen, now, "rolled_back");
                false
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
                    self.adopt_unconfirmed(e, run, ov.seen(), now);
                }
                false
            }
            Sent::NotWritten => {
                // The Steward's own move stays, over someone else's
                // choice: nothing more may move.
                self.pause(e, run, now, "unconfirmed", true);
                false
            }
        }
    }

    /// Takes lg's pointer `seen` as it is, head and all, and pauses: the
    /// Steward cannot tell whose move it was.
    fn adopt_unconfirmed(&self, e: &EntityId, run: &mut Run, seen: Seen, now: u64) {
        let lg = seen.active;
        let ov = &mut run.row.overlay;
        ov.pointer = lg;
        ov.pointer_head = seen.head;
        ov.active = ov.text_at(lg);
        ov.rollback_to = None;
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
    /// before a restart. It is never sent again; lg's pointer settles it
    /// ([`vet::lost_verdict`]). `guard` holds the head the activate sent as
    /// lg's guard (`None` for an intent written before the guard, or from
    /// a read that named no head). The guard counts only when lg names a
    /// head now too: a daemon that changed its build reads by version.
    ///
    /// lg numbers each move one past the last, and a guarded activate
    /// applies only as the first move after its read. So the head tells
    /// "not applied" apart from a move away and back, and lg on `v` two or
    /// more moves on is a later move to `v`, a human's choice that stands.
    /// It cannot tell the Steward's own move to `v` from a human's one
    /// move to `v`: the protocol names no mover. There the phase 4
    /// fail-safe decides, as for an older lg.
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
        guard: Option<Guard>,
        purpose: Purpose,
        now: u64,
    ) -> Activated {
        let Some(ov) = self.read_overlays(socket).await else {
            return Activated::Open;
        };
        run.row.intent = None;
        run.dirty = true;
        let lg = ov.active;
        let guard = guard.filter(|_| ov.named);
        let sent_head = guard.and_then(|g| g.head);
        match vet::lost_verdict(lg, ov.head, from, sent_head, guard.is_some(), v) {
            LostVerdict::NotApplied => {
                // Not applied; it may still wait in lg's queue. If it
                // applies later, the pointer check undoes it.
                run.row.overlay.add_late(v, now, guard);
                self.publish(
                    e,
                    "overlay",
                    as_id(v),
                    "unconfirmed",
                    "unconfirmed",
                    None,
                    None,
                );
                Activated::Not
            }
            LostVerdict::AtTarget => match purpose {
                Purpose::Baseline | Purpose::Clear if run.row.overlay.active.is_some() => {
                    // A move to `{}` off a text of the Steward's, applied:
                    // a human's Roll back off that text, run just before
                    // it, leaves lg on the same version. They look the
                    // same, so it is taken as the human's veto.
                    self.human_moved(e, run, ov.seen(), now, "unconfirmed");
                    Activated::Not
                }
                Purpose::Baseline | Purpose::Clear => {
                    // Its `previous` is unknown: so is what Roll back does.
                    // With the guard, lg's head is that of the one move to
                    // `v`, so it is the Steward's own; without it, the next
                    // agreeing read gives one.
                    let head = guard.and(ov.head);
                    self.applied(e, run, v, None, head, &purpose);
                    Activated::Done
                }
                Purpose::Text { .. } => {
                    // Applied, or a human's move to this very version: one
                    // cannot be ruled out. Back to the baseline, and pause.
                    run.row.overlay.pointer = Some(v);
                    run.row.overlay.pointer_head = ov.head;
                    run.row.overlay.active = None;
                    run.row.overlay.rollback_to = None;
                    match from {
                        Some(b) => {
                            if self.restore(e, socket, run, b, ov.seen(), now).await {
                                self.pause(e, run, now, "unconfirmed", true);
                            }
                        }
                        None => self.pause(e, run, now, "unconfirmed", true),
                    }
                    Activated::Not
                }
                Purpose::Restore => {
                    self.adopt_unconfirmed(e, run, ov.seen(), now);
                    Activated::Not
                }
            },
            LostVerdict::Human => {
                if purpose == Purpose::Restore {
                    self.adopt_unconfirmed(e, run, ov.seen(), now);
                    return Activated::Not;
                }
                if let Some(g) = guard
                    && lg == from
                {
                    // The head moved and lg is back where the activate read
                    // it: it ran and a human rolled it back, or a human
                    // moved away and back.
                    self.away_and_back(e, run, g.head, ov.head, &purpose, now);
                    return Activated::Not;
                }
                // A guarded activate still in lg's queue is refused once
                // the head moved: the head only rises. Unguarded, it may
                // still apply, so it goes on the late list.
                if guard.is_none_or(|g| g.head == ov.head) {
                    run.row.overlay.add_late(v, now, guard);
                }
                // The text the human did not land on is vetoed; the one they
                // chose (lg on `v`, moves after the activate) stands.
                if lg != Some(v) {
                    veto_text(run, &purpose);
                }
                self.human_moved(e, run, ov.seen(), now, "rolled_back");
                Activated::Not
            }
        }
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
                guard,
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
                    self.adopt_unconfirmed(e, run, ov.seen(), now);
                    return true;
                }
                self.settle_lost(
                    e,
                    socket,
                    run,
                    version,
                    expected_previous,
                    guard,
                    purpose,
                    now,
                )
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
                    text,
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
        if pointer != Some(b) && !self.clear(e, socket, run, b, pointer, now).await {
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
                text: text.text,
            },
            now,
        )
        .await;
    }

    /// Back to the baseline `b` from `from`, once lg shows `b` is still
    /// `{}`. Returns whether lg made exactly that move.
    async fn clear(
        &self,
        e: &EntityId,
        socket: &Path,
        run: &mut Run,
        b: u64,
        from: Option<u64>,
        now: u64,
    ) -> bool {
        self.confirm(e, socket, run, b, "", now).await
            && self
                .activate(e, socket, run, b, from, Purpose::Clear, now)
                .await
                == Activated::Done
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
                // A Turing-tier entity always uses the menu: a habit such
                // as "say you are not a real person" would leak its tier.
                let free = self.free_mode(e);
                let mode = if free { "free" } else { "menu" };
                self.publish(e, "overlay", as_id(version), "proposed", mode, None, None);
                if free {
                    self.free_pick(d, e, socket, run, &overlay, now).await;
                } else {
                    self.menu_pick(d, e, socket, run, &overlay, now).await;
                }
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
        let start = tokio::time::Instant::now();
        let Some(deadline) = start
            .checked_add(self.refine_drain)
            .or_else(|| start.checked_add(super::MAX_WAIT))
        else {
            return false;
        };
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
        let current_digest = run.row.overlay.active.as_ref().map(|a| a.digest);
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
        let text = vet::render_guardrails(&self.menu, &ids);
        let digest = vet::overlay_digest(&text);
        // By the text, not the ids: an edited menu text is put again.
        let kept = if ids.is_empty() {
            current_digest.is_none()
        } else {
            current_digest == Some(digest)
        };
        if kept {
            self.publish_full(event("kept", "kept"));
            return;
        }
        if ids.is_empty() {
            // No item now: back to the baseline.
            let (Some(b), pointer) = (run.row.overlay.baseline, run.row.overlay.pointer) else {
                return;
            };
            self.publish_full(event("voted", "clear"));
            if self.clear(e, socket, run, b, pointer, now).await {
                self.note_plain(e, "overlay_off");
            }
            return;
        }
        if run.row.is_vetoed(digest) {
            self.publish_full(event("declined", "vetoed"));
            return;
        }
        self.publish_full(event("voted", "menu"));
        self.put_and_chain(e, socket, run, OverlaySource::Menu { ids }, now)
            .await;
    }

    /// Puts the text of `source` (or reuses a version of it put before,
    /// once lg shows it unchanged), then activates it one move above the
    /// baseline.
    async fn put_and_chain(
        &self,
        e: &EntityId,
        socket: &Path,
        run: &mut Run,
        source: OverlaySource,
        now: u64,
    ) {
        let text = self.source_text(&source);
        let digest = vet::overlay_digest(&text);
        let ov = &run.row.overlay;
        let reuse = ov
            .authored
            .iter()
            .rev()
            .find(|a| a.digest == digest && a.source == source && !ov.is_late(a.version))
            .map(|a| a.version);
        let v = match reuse {
            Some(v) => {
                if !self.confirm(e, socket, run, v, &text, now).await {
                    return;
                }
                v
            }
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
                text,
            },
            now,
        )
        .await;
    }

    /// The free-text path on a proposal (9.5): extract the `guardrails`
    /// string only, normalize it (the preamble goes), guard it, put the
    /// exact lines to every member, and activate only on
    /// [`vet::overlay_verdict`]. Every candidate is published in full.
    async fn free_pick(
        &self,
        d: &dyn OwnedDaemons,
        e: &EntityId,
        socket: &Path,
        run: &mut Run,
        proposal: &str,
        now: u64,
    ) {
        // 1. Extract: the string `guardrails` only. Every other part of the
        // proposal is dropped here and never reaches lg again.
        let Some(raw) = extract_guardrails(proposal) else {
            self.publish(e, "overlay", None, "kept", "no_candidate", None, None);
            return;
        };
        // 2. Normalize, without the preamble.
        let lines = vet::normalize_free(&raw);
        let candidate = lines.join("\n");
        let current: Vec<String> = run
            .row
            .overlay
            .active
            .as_ref()
            .and_then(|a| a.source.free_lines())
            .map(<[String]>::to_vec)
            .unwrap_or_default();
        let event = |action: &str, code: &str, votes: Option<[u32; 2]>, reasons: Vec<String>| {
            StewardEvent {
                entity: e.clone(),
                item: "overlay".into(),
                id: None,
                action: action.into(),
                code: code.into(),
                votes,
                text: Some(candidate.clone()),
                reasons,
            }
        };
        if !current.is_empty() && lines == current {
            self.publish_full(event("kept", "kept", None, Vec::new()));
            return;
        }
        // 3. Guard: no model call.
        let names = self.entity_names();
        if let Err(why) = self.cfg.free_guard().check(
            &lines,
            &names,
            |digest| run.row.overlay_declined(digest),
            |digest| run.row.is_vetoed(digest),
        ) {
            tracing::info!(entity = %e, check = why.check(), "the free-text guard declined a candidate");
            self.publish_full(event("declined", why.code(), None, Vec::new()));
            return;
        }
        // 4. Council, on the exact lines.
        let Some((yes, no, reasons)) = self.free_votes(d, e, &current.join("\n"), &candidate).await
        else {
            return;
        };
        run.row.ballots.cast(now);
        run.dirty = true;
        let votes = Some([yes, no]);
        let level = self
            .sup
            .budget_of(e)
            .map_or(BudgetLevel::Normal, |(level, _)| level);
        match vet::overlay_verdict(
            true,
            yes,
            self.cfg.council_members,
            vet::FREE_MIN_MEMBERS,
            run.row.overlay.paused.is_some(),
            level == BudgetLevel::Normal,
        ) {
            OverlayVerdict::Hold => {
                let code = if run.row.overlay.paused.is_some() {
                    "paused"
                } else {
                    "budget"
                };
                self.publish_full(event("waiting", code, votes, reasons));
            }
            OverlayVerdict::Decline => {
                run.row.decline_overlay(vet::free_digest(&lines));
                self.publish_full(event("declined", "declined_council", votes, reasons));
            }
            OverlayVerdict::Activate => {
                self.publish_full(event("voted", "free", votes, reasons));
                // 5 and 6. Render and activate: the preamble and the lines
                // the council saw, nothing else.
                self.put_and_chain(e, socket, run, OverlaySource::Free { lines }, now)
                    .await;
            }
        }
    }

    /// Each member's vote on the free-text `candidate`: yes, no, and the
    /// reasons; `None` without a judge. A failed vote is a no.
    async fn free_votes(
        &self,
        d: &dyn OwnedDaemons,
        e: &EntityId,
        current: &str,
        candidate: &str,
    ) -> Option<(u32, u32, Vec<String>)> {
        let judge = self.judge.as_ref()?;
        let brief = d.brief(e).unwrap_or_else(|| PersonaText {
            name: String::new(),
            persona: String::new(),
            goal: String::new(),
            body: String::new(),
        });
        let (mut yes, mut no, mut reasons) = (0, 0, Vec::new());
        for member in 0..self.cfg.council_members {
            let ballot = Ballot::overlay(member, &brief, current, candidate);
            match judge.vote(&ballot).await {
                Ok((vote, usage)) => {
                    self.sup.record_spend(e, usage);
                    if vote.yes {
                        yes += 1;
                    } else {
                        no += 1;
                    }
                    reasons.extend(vote.reason);
                }
                Err(err) => {
                    tracing::warn!(entity = %e, error = %err, "a council vote failed; it counts as a no");
                    no += 1;
                }
            }
        }
        Some((yes, no, reasons))
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
                RefineMode::Free => "free",
                RefineMode::Menu | RefineMode::Off => "menu",
            }
            .to_owned(),
            pointer: ov.pointer,
            baseline: ov.baseline,
            source: ov.active.as_ref().map(|a| a.source.label()),
            lines: ov
                .active
                .as_ref()
                .and_then(|a| a.source.free_lines())
                .map(<[String]>::to_vec)
                .unwrap_or_default(),
            paused: ov.paused.as_ref().map(|p| p.code.clone()),
            rollback_to: ov.rollback_to,
            chain: ov
                .authored
                .iter()
                .rev()
                .map(|a| super::ChainEntry {
                    version: a.version,
                    label: a.source.label(),
                    text: a.text.clone(),
                    active: ov.pointer == Some(a.version),
                })
                .collect(),
        }
    }
}

/// The string `guardrails` of a proposal's overlay JSON, if it has one.
/// Nothing else of the proposal is read.
fn extract_guardrails(proposal: &str) -> Option<String> {
    match serde_json::from_str::<Value>(proposal) {
        Ok(Value::Object(mut obj)) => match obj.remove("guardrails") {
            Some(Value::String(g)) => Some(g),
            _ => None,
        },
        _ => None,
    }
}
