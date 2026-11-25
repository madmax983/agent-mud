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
//! No overflow: comparisons, and one subtraction under its guard.

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
/// vote and age, judged on the live accept count; or a refund (a human
/// decided the goal first).
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

} // verus!
