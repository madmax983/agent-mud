//! Boot derivations (ADR 0011, phase 4): which addressed events a restart
//! replays, and where an agent's MCP cursor starts.
//!
//! Mirrors `crates/mud-supervisor/src/replay.rs`. The text between the
//! mirror markers must match the Rust source; only the spec clauses differ.
//!
//! - `replay_floor`: the stored `seen_seq` (the last event a committed
//!   wake consumed), never above the world's last seq, or the last seq for
//!   an entity with no stored row. The MCP cursor starts there too, so it
//!   never skips an event after it (`lemma_floor_never_skips`).
//! - `replay_due`: exactly `seq > floor && at_ms + max_age_ms >= now_ms`.
//!   A consumed event is never replayed (`lemma_consumed_is_never_replayed`),
//!   and one too old at a boot stays too old at every later boot
//!   (`lemma_too_old_stays_old`).
//! - The tell model: one tell, its durable floor, the wake the router
//!   holds for it, and the answers it got, over every history of answers,
//!   silent closes and crashes with a boot at any time. A tell is answered
//!   at most once (`lemma_a_tell_is_answered_at_most_once`), and a tell no
//!   committed wake consumed is queued again by every boot within the
//!   window (`lemma_a_fresh_tell_is_not_lost`).
//!
//! No bound: comparisons, and one addition guarded against overflow.

use vstd::prelude::*;

verus! {

pub open spec fn spec_floor(has_row: bool, seen_seq: u64, last_seq: u64) -> u64 {
    if has_row && seen_seq < last_seq {
        seen_seq
    } else {
        last_seq
    }
}

// mirror-begin replay_floor
/// The seq after which the boot replays an entity's addressed events, and
/// where its MCP cursor starts.
///
/// It is the stored `seen_seq` (the last event a committed wake of the
/// entity consumed), never above the world's last seq. An entity with no
/// stored row (a file from before versions, an entity new to the pack)
/// starts at the last seq: nothing says whether the events before it were
/// answered.
#[must_use]
pub const fn replay_floor(has_row: bool, seen_seq: u64, last_seq: u64) -> (r: u64)
    ensures
        r == spec_floor(has_row, seen_seq, last_seq),
        r <= last_seq,
        has_row && seen_seq <= last_seq ==> r == seen_seq,
        !has_row ==> r == last_seq,
{
    if has_row && seen_seq < last_seq {
        seen_seq
    } else {
        last_seq
    }
}
// mirror-end replay_floor

pub open spec fn spec_due(seq: u64, at_ms: u64, floor: u64, now_ms: u64, max_age_ms: u64) -> bool {
    seq > floor && at_ms as int + max_age_ms as int >= now_ms as int
}

// mirror-begin replay_due
/// Whether the boot replays an addressed event: it came after the
/// entity's floor, and it is at most `max_age_ms` old at `now_ms`
/// (`at_ms + max_age_ms >= now_ms`, saturating).
#[must_use]
pub const fn replay_due(seq: u64, at_ms: u64, floor: u64, now_ms: u64, max_age_ms: u64) -> (r: bool)
    ensures
        r == spec_due(seq, at_ms, floor, now_ms, max_age_ms),
{
    let edge = if max_age_ms <= u64::MAX - at_ms {
        at_ms + max_age_ms
    } else {
        u64::MAX
    };
    seq > floor && edge >= now_ms
}
// mirror-end replay_due

/// The cursor an agent's MCP token starts at never passes the last event
/// of the world, so `mud_events` skips nothing that comes after the boot;
/// with a stored row it is exactly the stored `seen_seq`, so nothing a
/// committed wake consumed is handed out again.
pub proof fn lemma_floor_never_skips(has_row: bool, seen_seq: u64, last_seq: u64, later: u64)
    requires
        later > last_seq,
    ensures
        spec_floor(has_row, seen_seq, last_seq) < later,
        has_row && seen_seq <= last_seq ==> spec_floor(has_row, seen_seq, last_seq) == seen_seq,
{
}

/// An event at or below the floor (a committed wake consumed it) is never
/// replayed, at any time.
pub proof fn lemma_consumed_is_never_replayed(seq: u64, at_ms: u64, floor: u64, now_ms: u64, max_age_ms: u64)
    requires
        seq <= floor,
    ensures
        !spec_due(seq, at_ms, floor, now_ms, max_age_ms),
{
}

/// An event too old to replay at one boot is too old at every later boot
/// (a clock that goes forward), whatever the floor.
pub proof fn lemma_too_old_stays_old(
    seq: u64,
    at_ms: u64,
    floor: u64,
    later_floor: u64,
    now_ms: u64,
    later: u64,
    max_age_ms: u64,
)
    requires
        (at_ms as int + max_age_ms as int) < now_ms as int,
        now_ms <= later,
    ensures
        !spec_due(seq, at_ms, floor, now_ms, max_age_ms),
        !spec_due(seq, at_ms, later_floor, later, max_age_ms),
{
}

/// One step in the life of a tell, after it was stored.
pub enum TellStep {
    /// The wake that carries the tell answers it: its first effectful
    /// command commits with its consume, which raises the floor past the
    /// tell.
    Answer,
    /// The wake closes without an effectful command: its close commits the
    /// consume, and the tell goes unanswered by the mind's choice.
    Silent,
    /// The process is killed (the router's waiting wakes are lost), and a
    /// new one boots at `now`, replaying what is due.
    Crash(u64),
}

/// The durable floor, whether the router holds a wake that carries the
/// tell, and how many answers it got.
pub struct TellState {
    pub floor: u64,
    pub pending: bool,
    pub answered: nat,
}

pub open spec fn tell_apply(s: TellState, st: TellStep, seq: u64, at_ms: u64, max_age_ms: u64) -> TellState {
    match st {
        TellStep::Answer => TellState {
            floor: if s.floor >= seq { s.floor } else { seq },
            pending: false,
            answered: s.answered + 1,
        },
        TellStep::Silent => TellState {
            floor: if s.floor >= seq { s.floor } else { seq },
            pending: false,
            answered: s.answered,
        },
        TellStep::Crash(now) => TellState {
            floor: s.floor,
            pending: spec_due(seq, at_ms, s.floor, now, max_age_ms),
            answered: s.answered,
        },
    }
}

/// Only a wake the router holds can answer or close: a step that needs
/// one needs `pending`.
pub open spec fn tell_ok(s: TellState, st: TellStep) -> bool {
    match st {
        TellStep::Answer | TellStep::Silent => s.pending,
        TellStep::Crash(_) => true,
    }
}

pub open spec fn tell_state(start: TellState, h: Seq<TellStep>, seq: u64, at_ms: u64, max_age_ms: u64) -> TellState
    decreases h.len(),
{
    if h.len() == 0 {
        start
    } else {
        tell_apply(tell_state(start, h.drop_last(), seq, at_ms, max_age_ms), h.last(), seq, at_ms, max_age_ms)
    }
}

pub open spec fn tell_valid(start: TellState, h: Seq<TellStep>, seq: u64, at_ms: u64, max_age_ms: u64) -> bool
    decreases h.len(),
{
    h.len() == 0 || (tell_valid(start, h.drop_last(), seq, at_ms, max_age_ms) && tell_ok(
        tell_state(start, h.drop_last(), seq, at_ms, max_age_ms),
        h.last(),
    ))
}

/// How many steps of `h` committed a consume (an answer or a silent close).
pub open spec fn consumes(h: Seq<TellStep>) -> nat
    decreases h.len(),
{
    if h.len() == 0 {
        0
    } else {
        consumes(h.drop_last()) + match h.last() {
            TellStep::Answer | TellStep::Silent => 1nat,
            TellStep::Crash(_) => 0nat,
        }
    }
}

/// A tell as it is stored: no wake consumed it yet (the floor is below
/// it); the live router may or may not have queued a wake for it before
/// the first crash.
pub open spec fn tell_start(s: TellState, seq: u64) -> bool {
    s.floor < seq && s.answered == 0
}

pub open spec fn tell_inv(s: TellState, seq: u64) -> bool {
    s.answered <= 1 && (s.answered >= 1 ==> s.floor >= seq) && (s.pending ==> s.answered == 0
        && s.floor < seq)
}

proof fn lemma_tell_inv(start: TellState, h: Seq<TellStep>, seq: u64, at_ms: u64, max_age_ms: u64)
    requires
        tell_start(start, seq),
        tell_valid(start, h, seq, at_ms, max_age_ms),
    ensures
        tell_inv(tell_state(start, h, seq, at_ms, max_age_ms), seq),
        consumes(h) == 0 ==> tell_state(start, h, seq, at_ms, max_age_ms).floor == start.floor,
    decreases h.len(),
{
    if h.len() > 0 {
        lemma_tell_inv(start, h.drop_last(), seq, at_ms, max_age_ms);
    }
}

/// A tell is answered at most once, over every history of answers, silent
/// closes and crashes with a boot at any time: an answer commits the floor
/// past the tell in the answer's own transaction, and a boot replays only
/// what is above the floor.
pub proof fn lemma_a_tell_is_answered_at_most_once(
    start: TellState,
    h: Seq<TellStep>,
    seq: u64,
    at_ms: u64,
    max_age_ms: u64,
)
    requires
        tell_start(start, seq),
        tell_valid(start, h, seq, at_ms, max_age_ms),
    ensures
        tell_state(start, h, seq, at_ms, max_age_ms).answered <= 1,
{
    lemma_tell_inv(start, h, seq, at_ms, max_age_ms);
}

/// A tell no committed wake consumed (no answer and no silent close in
/// `h`) is queued again by a boot at any `now` within the window: a crash
/// never loses it while it is fresh.
pub proof fn lemma_a_fresh_tell_is_not_lost(
    start: TellState,
    h: Seq<TellStep>,
    seq: u64,
    at_ms: u64,
    max_age_ms: u64,
    now: u64,
)
    requires
        tell_start(start, seq),
        tell_valid(start, h, seq, at_ms, max_age_ms),
        consumes(h) == 0,
        now as int <= at_ms as int + max_age_ms as int,
    ensures
        tell_valid(start, h.push(TellStep::Crash(now)), seq, at_ms, max_age_ms),
        tell_state(start, h.push(TellStep::Crash(now)), seq, at_ms, max_age_ms).pending,
{
    lemma_tell_inv(start, h, seq, at_ms, max_age_ms);
    assert(h.push(TellStep::Crash(now)).drop_last() =~= h);
}

} // verus!
