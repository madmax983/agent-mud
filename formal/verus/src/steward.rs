//! The Steward's goal kernels (ADR 0015).
//!
//! Mirrors `crates/mud-supervisor/src/steward/vet.rs`. The text between the
//! mirror markers must match the Rust source; only the spec clauses differ.
//!
//! - `goal_verdict`: an `Admit` has good text, a check interval of at least
//!   `min_check` (or none), fewer active goals than `max_active`, fewer
//!   accepts today than `max_per_day` and a vote to be had; every `Wait` is
//!   younger than the TTL, so every wait (no judge, a soft budget, the
//!   ballot cap, a full slate) ends in a decline at the TTL; bad text is
//!   always declined.
//! - The day model (`lemma_accepts_never_pass_the_cap`): over any day's
//!   steps, where an admitted goal that is sent reserves one accept and a
//!   refund gives one back, the live accepts never pass `max_per_day`.
//! - `council_majority` is strict (`yes > members - yes`), and two
//!   disjoint majorities of one council never both hold
//!   (`lemma_no_two_disjoint_majorities`).
//!
//! - `refine_due` (phase 4): due only when not paused, at a normal budget,
//!   after `min_wakes` wakes and `every_ms` since the last propose; a pause
//!   blocks every refine (`lemma_a_pause_blocks_every_refine`) and a clock
//!   set back is never due (`lemma_a_clock_set_back_is_not_due`).
//! - `pointer_verdict` (phase 4): `Same` exactly when lg's version is the
//!   expected one; `OwnLate` only for a version the Steward gave up on;
//!   every other move is a human's (`lemma_every_foreign_move_is_human`).
//! - `head_verdict` (lg #124): the pointer check with heads. The Steward's
//!   own version at its own head is `Same` (`lemma_an_unmoved_head_is_same`);
//!   at another head, with both heads known, it is a human's move away and
//!   back (`lemma_a_move_away_and_back_is_human`), unless it is the one
//!   move of a late activate of that version; with a head unknown on
//!   either side, or another version, it is exactly `pointer_verdict`
//!   (`lemma_an_unknown_head_is_the_version_check`,
//!   `lemma_another_version_is_the_version_check`). `head_went_back`
//!   says a reset store: lg names a head below the Steward's own, or none;
//!   a head at or above it never is (`lemma_a_risen_head_is_no_reset`).
//! - `lost_verdict` (lg #122): an activate whose answer never came is
//!   "not applied" only while lg shows the pointer it read, and with the
//!   guard only while the head has not moved
//!   (`lemma_a_moved_head_is_never_not_applied`); a move away and back is
//!   a human's (`lemma_away_and_back_is_human`); a version that is neither
//!   the read one nor the target is a human's
//!   (`lemma_a_third_version_is_human`). A guarded activate can only be
//!   the first move after its read (`one_move`), so with the guard lg on
//!   the target two or more moves on is a human's
//!   (`lemma_a_later_move_to_the_target_is_human`), and exactly one move
//!   on is `AtTarget` (`lemma_one_move_to_the_target_is_at_target`).
//! - `overlay_verdict` (phase 5, ADR 0016): a free text activates only
//!   with the guard passed, a unanimous council of at least `min_members`
//!   (at least one), no pause and a normal budget; a pause blocks every
//!   activation, one no vote never activates, and with `FREE_MIN_MEMBERS`
//!   (3, a mirrored constant) fewer than three members never activate.
//!
//! No overflow: comparisons, and subtractions under their guards.

use vstd::prelude::*;

verus! {

// mirror-begin goal_verdict_enum
/// What the Steward does with a proposed goal.
pub enum GoalVerdict {
    /// Put it to the council.
    Admit,
    /// Leave it proposed for now.
    Wait,
    /// Decline it.
    Decline,
}
// mirror-end goal_verdict_enum

#[derive(Clone, Copy)]
// mirror-begin goal_ask
/// One proposed goal, as the kernel sees it.
pub struct GoalAsk {
    /// The text guard passed.
    pub text_ok: bool,
    /// How often the goal is checked, in minutes (0: not given).
    pub check_minutes: u32,
    /// Active goals now.
    pub active: u32,
    /// Accepts reserved and not refunded this game-clock day.
    pub accepted_today: u32,
    /// A judge exists, the budget is normal and today's ballots are under
    /// the cap.
    pub can_vote: bool,
    /// How long the goal has been proposed, by the game clock.
    pub age_ms: u64,
}
// mirror-end goal_ask

#[derive(Clone, Copy)]
// mirror-begin goal_caps
/// The limits on goals.
pub struct GoalCaps {
    /// The shortest check interval, in minutes.
    pub min_check: u32,
    /// Active goals at most.
    pub max_active: u32,
    /// Accepts per game-clock day at most.
    pub max_per_day: u32,
    /// A goal still waiting at this age is declined.
    pub ttl_ms: u64,
}
// mirror-end goal_caps

pub open spec fn spec_goal_verdict(ask: GoalAsk, caps: GoalCaps) -> GoalVerdict {
    if !ask.text_ok || (ask.check_minutes != 0 && ask.check_minutes < caps.min_check) {
        GoalVerdict::Decline
    } else if ask.active >= caps.max_active || ask.accepted_today >= caps.max_per_day
        || !ask.can_vote {
        if ask.age_ms >= caps.ttl_ms {
            GoalVerdict::Decline
        } else {
            GoalVerdict::Wait
        }
    } else {
        GoalVerdict::Admit
    }
}

// mirror-begin goal_verdict
/// The verdict on one proposed goal.
///
/// Bad text or a check too frequent is declined; a goal past a cap, or with no vote to be had, waits until its
/// TTL and is then declined; anything else goes to the council.
#[must_use]
pub const fn goal_verdict(ask: GoalAsk, caps: GoalCaps) -> (r: GoalVerdict)
    ensures
        r == spec_goal_verdict(ask, caps),
        (r is Admit) ==> ask.text_ok && ask.active < caps.max_active && ask.accepted_today
            < caps.max_per_day && ask.can_vote && (ask.check_minutes == 0 || ask.check_minutes
            >= caps.min_check),
        (r is Wait) ==> ask.age_ms < caps.ttl_ms,
        !ask.text_ok ==> (r is Decline),
{
    if !ask.text_ok || (ask.check_minutes != 0 && ask.check_minutes < caps.min_check) {
        GoalVerdict::Decline
    } else if ask.active >= caps.max_active
        || ask.accepted_today >= caps.max_per_day
        || !ask.can_vote
    {
        if ask.age_ms >= caps.ttl_ms {
            GoalVerdict::Decline
        } else {
            GoalVerdict::Wait
        }
    } else {
        GoalVerdict::Admit
    }
}
// mirror-end goal_verdict

pub open spec fn spec_majority(yes: u32, members: u32) -> bool {
    yes <= members && yes > members - yes
}

// mirror-begin council_majority
/// Whether `yes` votes of a council of `members` are a strict majority.
#[must_use]
pub const fn council_majority(yes: u32, members: u32) -> (r: bool)
    ensures
        r == spec_majority(yes, members),
        r ==> yes > members - yes,
{
    yes <= members && yes > members - yes
}
// mirror-end council_majority

/// Two disjoint sets of votes of one council (`yes + other <= members`)
/// are never both majorities.
pub proof fn lemma_no_two_disjoint_majorities(yes: u32, other: u32, members: u32)
    requires
        yes + other <= members,
    ensures
        !(spec_majority(yes, members) && spec_majority(other, members)),
{
}

/// One step of a day: a goal asked with its own text, check, active count,
/// vote and age, judged on the live accept count; or a refund. The
/// Steward never refunds a place outright; it reuses one: a stuck accept's
/// goal, asked again the same day, is judged without its own place and
/// takes it back (a refund, then its ask).
pub enum DayStep {
    Ask(bool, u32, u32, bool, u64),
    Refund,
}

/// The live accepts after `steps`, from `start`: an admitted ask reserves
/// one, a refund gives one back.
pub open spec fn live(start: u32, caps: GoalCaps, steps: Seq<DayStep>) -> u32
    decreases steps.len(),
{
    if steps.len() == 0 {
        start
    } else {
        let c = live(start, caps, steps.drop_last());
        match steps.last() {
            DayStep::Ask(text_ok, check, active, can_vote, age) => {
                let ask = GoalAsk {
                    text_ok,
                    check_minutes: check,
                    active,
                    accepted_today: c,
                    can_vote,
                    age_ms: age,
                };
                if spec_goal_verdict(ask, caps) is Admit {
                    (c + 1) as u32
                } else {
                    c
                }
            },
            DayStep::Refund => if c > 0 {
                (c - 1) as u32
            } else {
                0
            },
        }
    }
}

/// Whatever is asked and refunded in a day, the live accepts never pass
/// the cap.
pub proof fn lemma_accepts_never_pass_the_cap(start: u32, caps: GoalCaps, steps: Seq<DayStep>)
    requires
        start <= caps.max_per_day,
    ensures
        live(start, caps, steps) <= caps.max_per_day,
    decreases steps.len(),
{
    if steps.len() > 0 {
        lemma_accepts_never_pass_the_cap(start, caps, steps.drop_last());
    }
}

pub open spec fn spec_refine_due(
    now_ms: u64,
    last_ms: Option<u64>,
    every_ms: u64,
    wakes_since: u32,
    min_wakes: u32,
    paused: bool,
    level_normal: bool,
) -> bool {
    let time_ok = match last_ms {
        None => true,
        Some(l) => now_ms >= l && now_ms - l >= every_ms,
    };
    !paused && level_normal && time_ok && wakes_since >= min_wakes
}

// mirror-begin refine_due
/// Whether a refine propose is due.
///
/// Never while paused or below a normal budget, never before `min_wakes`
/// wakes, and never before `every_ms` has passed since the last one by the
/// game clock. A clock set back (`now_ms < last`) is never due.
#[must_use]
pub const fn refine_due(
    now_ms: u64,
    last_ms: Option<u64>,
    every_ms: u64,
    wakes_since: u32,
    min_wakes: u32,
    paused: bool,
    level_normal: bool,
) -> (r: bool)
    ensures
        r == spec_refine_due(now_ms, last_ms, every_ms, wakes_since, min_wakes, paused, level_normal),
        r ==> !paused && level_normal && wakes_since >= min_wakes,
        r ==> match last_ms {
            None => true,
            Some(l) => now_ms >= l && now_ms - l >= every_ms,
        },
{
    let time_ok = match last_ms {
        None => true,
        Some(l) => now_ms >= l && now_ms - l >= every_ms,
    };
    !paused && level_normal && time_ok && wakes_since >= min_wakes
}
// mirror-end refine_due

/// A pause blocks every refine, whatever the clock, the wakes or the
/// budget say.
pub proof fn lemma_a_pause_blocks_every_refine(
    now_ms: u64,
    last_ms: Option<u64>,
    every_ms: u64,
    wakes_since: u32,
    min_wakes: u32,
    level_normal: bool,
)
    ensures
        !spec_refine_due(now_ms, last_ms, every_ms, wakes_since, min_wakes, true, level_normal),
{
}

/// A clock set back before the last propose is never due.
pub proof fn lemma_a_clock_set_back_is_not_due(
    now_ms: u64,
    l: u64,
    every_ms: u64,
    wakes_since: u32,
    min_wakes: u32,
    paused: bool,
    level_normal: bool,
)
    requires
        now_ms < l,
    ensures
        !spec_refine_due(now_ms, Some(l), every_ms, wakes_since, min_wakes, paused, level_normal),
{
}

// mirror-begin pointer_verdict_enum
/// What lg's active overlay version says, against the one the Steward
/// expects.
pub enum PointerVerdict {
    /// It is the expected version.
    Same,
    /// It is a version the Steward activated and gave up on: its activate
    /// applied late.
    OwnLate,
    /// Someone else moved it: a human's veto.
    Human,
}
// mirror-end pointer_verdict_enum

pub open spec fn spec_pointer_verdict(
    lg: Option<u64>,
    expected: Option<u64>,
    lg_is_late: bool,
) -> PointerVerdict {
    if lg == expected {
        PointerVerdict::Same
    } else if lg_is_late && lg is Some {
        PointerVerdict::OwnLate
    } else {
        PointerVerdict::Human
    }
}

// mirror-begin pointer_verdict
/// The verdict on lg's active version `lg` against `expected`.
///
/// `lg_is_late` says the Steward gave up on an activate of `lg`. Any
/// other move away from `expected` is a human's.
#[must_use]
pub const fn pointer_verdict(
    lg: Option<u64>,
    expected: Option<u64>,
    lg_is_late: bool,
) -> (r: PointerVerdict)
    ensures
        r == spec_pointer_verdict(lg, expected, lg_is_late),
        (r is Same) <==> lg == expected,
        (r is OwnLate) ==> lg_is_late && lg is Some && lg != expected,
        (r is Human) ==> lg != expected && !(lg_is_late && lg is Some),
{
    match lg {
        None => match expected {
            None => PointerVerdict::Same,
            Some(_) => PointerVerdict::Human,
        },
        Some(a) => {
            let same = match expected {
                Some(b) => a == b,
                None => false,
            };
            if same {
                PointerVerdict::Same
            } else if lg_is_late {
                PointerVerdict::OwnLate
            } else {
                PointerVerdict::Human
            }
        }
    }
}
// mirror-end pointer_verdict

/// A version the Steward neither expects nor gave up on is a human's
/// move: never `Same`, never `OwnLate`.
pub proof fn lemma_every_foreign_move_is_human(lg: Option<u64>, expected: Option<u64>)
    requires
        lg != expected,
    ensures
        spec_pointer_verdict(lg, expected, false) is Human,
{
}

/// lg's head `after` is one move past `before`: 1 after no move, else
/// one more.
pub open spec fn spec_one_move(before: Option<u64>, after: Option<u64>) -> bool {
    match before {
        None => after == Some(1u64),
        Some(b) => match after {
            Some(a) => a as int == b as int + 1,
            None => false,
        },
    }
}

// mirror-begin one_move
/// Whether lg's head `after` is exactly one move past `before` (lg #122).
///
/// lg numbers each move of the pointer one past the last, from 1, so the
/// first move after a read of `before` is the only one that lands there.
#[must_use]
pub const fn one_move(before: Option<u64>, after: Option<u64>) -> (r: bool)
    ensures
        r == spec_one_move(before, after),
{
    match (before, after) {
        (None, Some(a)) => a == 1,
        (Some(b), Some(a)) => b < a && a - b == 1,
        _ => false,
    }
}
// mirror-end one_move

/// lg names a head, the Steward knows its own, and they differ: someone
/// moved the pointer since the Steward's own move.
pub open spec fn spec_head_moved(
    lg_head: Option<u64>,
    lg_named: bool,
    own_head: Option<u64>,
) -> bool {
    lg_named && own_head is Some && lg_head != own_head
}

pub open spec fn spec_head_verdict(
    lg: Option<u64>,
    lg_head: Option<u64>,
    lg_named: bool,
    expected: Option<u64>,
    own_head: Option<u64>,
    lg_is_late: bool,
) -> PointerVerdict {
    if lg == expected && spec_head_moved(lg_head, lg_named, own_head) {
        if lg is Some && lg_is_late && spec_one_move(own_head, lg_head) {
            PointerVerdict::OwnLate
        } else {
            PointerVerdict::Human
        }
    } else {
        spec_pointer_verdict(lg, expected, lg_is_late)
    }
}

// mirror-begin head_verdict
/// The pointer check on lg's pointer `(lg, lg_head)` against the
/// Steward's `expected` version and `own_head`, the head of its own last
/// move to it (littleguy #124).
///
/// `lg_named` says lg named a head now, and `own_head` is `None` while the
/// Steward knows no head of its own. With both known, the same version at
/// another head is a move since the Steward's own and back to it: a
/// human's, unless it is the one move of a late activate of that version
/// ([`one_move`] from `own_head`). Every other case is the version check
/// of [`pointer_verdict`], as before #124.
#[must_use]
pub const fn head_verdict(
    lg: Option<u64>,
    lg_head: Option<u64>,
    lg_named: bool,
    expected: Option<u64>,
    own_head: Option<u64>,
    lg_is_late: bool,
) -> (r: PointerVerdict)
    ensures
        r == spec_head_verdict(lg, lg_head, lg_named, expected, own_head, lg_is_late),
        !lg_named || own_head is None ==> r == spec_pointer_verdict(lg, expected, lg_is_late),
        lg != expected ==> r == spec_pointer_verdict(lg, expected, lg_is_late),
        (r is Same) ==> lg == expected && !spec_head_moved(lg_head, lg_named, own_head),
        (r is OwnLate) ==> lg_is_late && lg is Some,
{
    let same_version = match (lg, expected) {
        (None, None) => true,
        (Some(a), Some(b)) => a == b,
        _ => false,
    };
    let moved = if lg_named {
        match (own_head, lg_head) {
            (Some(h), Some(a)) => a != h,
            (Some(_), None) => true,
            (None, _) => false,
        }
    } else {
        false
    };
    if same_version && moved {
        let own_late = match lg {
            Some(_) => lg_is_late && one_move(own_head, lg_head),
            None => false,
        };
        if own_late {
            PointerVerdict::OwnLate
        } else {
            PointerVerdict::Human
        }
    } else {
        pointer_verdict(lg, expected, lg_is_late)
    }
}
// mirror-end head_verdict

/// The Steward's own version at its own head: `Same`, late or not.
pub proof fn lemma_an_unmoved_head_is_same(v: Option<u64>, h: u64, lg_is_late: bool)
    ensures
        spec_head_verdict(v, Some(h), true, v, Some(h), lg_is_late) is Same,
{
}

/// The Steward's own version at another head, with both heads known, is a
/// human's move away and back, unless it is the one move of a late
/// activate of that version.
pub proof fn lemma_a_move_away_and_back_is_human(
    v: Option<u64>,
    lg_head: Option<u64>,
    own: u64,
    lg_is_late: bool,
)
    requires
        lg_head != Some(own),
        !(lg_is_late && spec_one_move(Some(own), lg_head)),
    ensures
        spec_head_verdict(v, lg_head, true, v, Some(own), lg_is_late) is Human,
{
}

/// With a head unknown on either side, the verdict is the version check
/// of `pointer_verdict`: nothing regresses from before #124.
pub proof fn lemma_an_unknown_head_is_the_version_check(
    lg: Option<u64>,
    lg_head: Option<u64>,
    lg_named: bool,
    expected: Option<u64>,
    own_head: Option<u64>,
    lg_is_late: bool,
)
    requires
        !lg_named || own_head is None,
    ensures
        spec_head_verdict(lg, lg_head, lg_named, expected, own_head, lg_is_late)
            == spec_pointer_verdict(lg, expected, lg_is_late),
{
}

/// Another version is judged by the version check alone, heads or not:
/// a late activate one move past its guard stays the Steward's own.
pub proof fn lemma_another_version_is_the_version_check(
    lg: Option<u64>,
    lg_head: Option<u64>,
    lg_named: bool,
    expected: Option<u64>,
    own_head: Option<u64>,
    lg_is_late: bool,
)
    requires
        lg != expected,
    ensures
        spec_head_verdict(lg, lg_head, lg_named, expected, own_head, lg_is_late)
            == spec_pointer_verdict(lg, expected, lg_is_late),
{
}

pub open spec fn spec_head_went_back(
    own_head: Option<u64>,
    lg_head: Option<u64>,
    lg_named: bool,
) -> bool {
    lg_named && match own_head {
        Some(h) => match lg_head {
            Some(a) => a < h,
            None => true,
        },
        None => false,
    }
}

// mirror-begin head_went_back
/// Whether lg's head `lg_head` is below `own_head`, or gone, while lg
/// names a head: lg's store was reset. Within one store the head only
/// rises.
#[must_use]
pub const fn head_went_back(own_head: Option<u64>, lg_head: Option<u64>, lg_named: bool) -> (r: bool)
    ensures
        r == spec_head_went_back(own_head, lg_head, lg_named),
        r ==> lg_named && own_head is Some,
{
    if lg_named {
        match (own_head, lg_head) {
            (Some(h), Some(a)) => a < h,
            (Some(_), None) => true,
            (None, _) => false,
        }
    } else {
        false
    }
}
// mirror-end head_went_back

/// A head at or above the Steward's own is never a reset: the move of a
/// human, or of the Steward's own late activate, raises it.
pub proof fn lemma_a_risen_head_is_no_reset(own: u64, lg: u64, lg_named: bool)
    requires
        own <= lg,
    ensures
        !spec_head_went_back(Some(own), Some(lg), lg_named),
{
}

// mirror-begin lost_verdict_enum
/// What lg's pointer, read again, says of an activate whose answer never
/// came.
pub enum LostVerdict {
    /// The pointer is where the activate read it: the activate did not
    /// apply. It may still wait in lg's queue.
    NotApplied,
    /// The pointer is on the activate's version, and (with the guard) one
    /// move past the read: it applied, or someone else made that one move
    /// to it. The two cannot be told apart.
    AtTarget,
    /// Someone else moved the pointer: a human's veto.
    Human,
}
// mirror-end lost_verdict_enum

pub open spec fn spec_lost_verdict(
    lg: Option<u64>,
    lg_head: Option<u64>,
    from: Option<u64>,
    from_head: Option<u64>,
    guarded: bool,
    target: u64,
) -> LostVerdict {
    if lg == from && (lg_head == from_head || !guarded) {
        LostVerdict::NotApplied
    } else if lg == Some(target) && (!guarded || spec_one_move(from_head, lg_head)) {
        LostVerdict::AtTarget
    } else {
        LostVerdict::Human
    }
}

// mirror-begin lost_verdict
/// The verdict on lg's pointer `(lg, lg_head)`, read after an activate of
/// `target` from `(from, from_head)` got no answer.
///
/// `guarded` says the activate carried `from_head` as lg's guard (lg
/// #122), and lg named a head both times. Each move raises the head by
/// one, so a head that moved is a move after the read, and a pointer back
/// on `from` is then a move away and back, never "not applied". A guarded
/// activate can only be the first move after its read ([`one_move`]): lg
/// on `target` with the head two or more moves on is a later move to it,
/// a human's. Without the guard only the version counts.
#[must_use]
pub const fn lost_verdict(
    lg: Option<u64>,
    lg_head: Option<u64>,
    from: Option<u64>,
    from_head: Option<u64>,
    guarded: bool,
    target: u64,
) -> (r: LostVerdict)
    ensures
        r == spec_lost_verdict(lg, lg_head, from, from_head, guarded, target),
        (r is NotApplied) ==> lg == from,
        (r is NotApplied) && guarded ==> lg_head == from_head,
        (r is AtTarget) ==> lg == Some(target),
        (r is AtTarget) && guarded ==> spec_one_move(from_head, lg_head),
        (r is Human) ==> lg != Some(target) || (guarded && !spec_one_move(from_head, lg_head)),
        (r is Human) ==> lg != from || (guarded && lg_head != from_head),
{
    let at_from = match (lg, from) {
        (None, None) => true,
        (Some(a), Some(b)) => a == b,
        _ => false,
    };
    let head_same = match (lg_head, from_head) {
        (None, None) => true,
        (Some(a), Some(b)) => a == b,
        _ => false,
    };
    let at_target = match lg {
        Some(a) => a == target,
        None => false,
    };
    if at_from && (head_same || !guarded) {
        LostVerdict::NotApplied
    } else if at_target && (!guarded || one_move(from_head, lg_head)) {
        LostVerdict::AtTarget
    } else {
        LostVerdict::Human
    }
}
// mirror-end lost_verdict

/// With the guard, a head that moved is never "not applied": a move after
/// the read is always seen.
pub proof fn lemma_a_moved_head_is_never_not_applied(
    lg: Option<u64>,
    lg_head: Option<u64>,
    from: Option<u64>,
    from_head: Option<u64>,
    target: u64,
)
    requires
        lg_head != from_head,
    ensures
        !(spec_lost_verdict(lg, lg_head, from, from_head, true, target) is NotApplied),
{
}

/// A move away and back while the answer was lost: lg shows the version
/// the activate read, the head moved, and the activate named another
/// version. It is a human's veto, never "not applied".
pub proof fn lemma_away_and_back_is_human(
    lg_head: Option<u64>,
    from: Option<u64>,
    from_head: Option<u64>,
    target: u64,
)
    requires
        lg_head != from_head,
        from != Some(target),
    ensures
        spec_lost_verdict(from, lg_head, from, from_head, true, target) is Human,
{
}

/// The pointer as the activate read it, head and all: not applied.
pub proof fn lemma_an_unmoved_pointer_is_not_applied(
    from: Option<u64>,
    from_head: Option<u64>,
    guarded: bool,
    target: u64,
)
    ensures
        spec_lost_verdict(from, from_head, from, from_head, guarded, target) is NotApplied,
{
}

/// With the guard, lg on the target two or more moves past the read is a
/// later move to it: the guarded activate could only be the first move.
/// It is a human's, never `AtTarget`.
pub proof fn lemma_a_later_move_to_the_target_is_human(
    lg_head: Option<u64>,
    from: Option<u64>,
    from_head: Option<u64>,
    target: u64,
)
    requires
        from != Some(target),
        !spec_one_move(from_head, lg_head),
    ensures
        spec_lost_verdict(Some(target), lg_head, from, from_head, true, target) is Human,
{
}

/// With the guard, lg on the target exactly one move past the read is
/// `AtTarget`: the Steward's move, or a human's one move to it.
pub proof fn lemma_one_move_to_the_target_is_at_target(
    from: Option<u64>,
    from_head: Option<u64>,
    target: u64,
)
    requires
        from != Some(target),
    ensures
        spec_lost_verdict(
            Some(target),
            Some(match from_head {
                None => 1u64,
                Some(h) => if h < u64::MAX { (h + 1) as u64 } else { h },
            }),
            from,
            from_head,
            true,
            target,
        ) is AtTarget || from_head == Some(u64::MAX),
{
}

/// A version that is neither the read one nor the target is a human's,
/// with or without the guard.
pub proof fn lemma_a_third_version_is_human(
    lg: Option<u64>,
    lg_head: Option<u64>,
    from: Option<u64>,
    from_head: Option<u64>,
    guarded: bool,
    target: u64,
)
    requires
        lg != from,
        lg != Some(target),
    ensures
        spec_lost_verdict(lg, lg_head, from, from_head, guarded, target) is Human,
{
}

// mirror-begin steward_consts
/// The fewest council members that may approve a free text (ADR 0016).
/// The Steward passes it as `min_members`, so "at least three" is in the
/// proof, not only in the start-up check.
pub const FREE_MIN_MEMBERS: u32 = 3;
// mirror-end steward_consts

// mirror-begin overlay_verdict_enum
/// What the Steward does with a free-text candidate after the council
/// voted.
pub enum OverlayVerdict {
    /// Put it and activate it.
    Activate,
    /// Decline it; it is never voted on again.
    Decline,
    /// Neither: refine is paused, or the budget is below normal.
    Hold,
}
// mirror-end overlay_verdict_enum

pub open spec fn spec_overlay_verdict(
    text_ok: bool,
    yes: u32,
    members: u32,
    min_members: u32,
    paused: bool,
    level_normal: bool,
) -> OverlayVerdict {
    if paused || !level_normal {
        OverlayVerdict::Hold
    } else if text_ok && min_members >= 1 && members >= min_members && yes == members {
        OverlayVerdict::Activate
    } else {
        OverlayVerdict::Decline
    }
}

// mirror-begin overlay_verdict
/// The verdict on a free-text candidate.
///
/// It activates only when the text guard passed, at least `min_members`
/// (and at least one) voted and every one approved, refine is not paused
/// and the budget is normal. A pause or a budget below normal holds it.
#[must_use]
pub const fn overlay_verdict(
    text_ok: bool,
    yes: u32,
    members: u32,
    min_members: u32,
    paused: bool,
    level_normal: bool,
) -> (r: OverlayVerdict)
    ensures
        r == spec_overlay_verdict(text_ok, yes, members, min_members, paused, level_normal),
        (r is Activate) ==> text_ok && min_members >= 1 && members >= min_members && yes
            == members && !paused && level_normal,
        paused ==> !(r is Activate),
        yes < members ==> !(r is Activate),
{
    if paused || !level_normal {
        OverlayVerdict::Hold
    } else if text_ok && min_members >= 1 && members >= min_members && yes == members {
        OverlayVerdict::Activate
    } else {
        OverlayVerdict::Decline
    }
}
// mirror-end overlay_verdict

/// A pause blocks every activation, whatever the guard and the votes say.
pub proof fn lemma_a_pause_blocks_every_activation(
    text_ok: bool,
    yes: u32,
    members: u32,
    min_members: u32,
    level_normal: bool,
)
    ensures
        !(spec_overlay_verdict(text_ok, yes, members, min_members, true, level_normal) is Activate),
{
}

/// One no vote never activates.
pub proof fn lemma_one_no_never_activates(
    text_ok: bool,
    yes: u32,
    members: u32,
    min_members: u32,
    paused: bool,
    level_normal: bool,
)
    requires
        yes < members,
    ensures
        !(spec_overlay_verdict(text_ok, yes, members, min_members, paused, level_normal) is Activate),
{
}

/// With `min_members = FREE_MIN_MEMBERS`, a council of fewer than three
/// never activates, and an activation always had three yes votes or more.
pub proof fn lemma_fewer_than_three_never_activate(
    text_ok: bool,
    yes: u32,
    members: u32,
    paused: bool,
    level_normal: bool,
)
    ensures
        FREE_MIN_MEMBERS == 3,
        members < 3 ==> !(spec_overlay_verdict(
            text_ok,
            yes,
            members,
            FREE_MIN_MEMBERS,
            paused,
            level_normal,
        ) is Activate),
        (spec_overlay_verdict(text_ok, yes, members, FREE_MIN_MEMBERS, paused, level_normal) is Activate)
            ==> yes >= 3,
{
}

} // verus!
