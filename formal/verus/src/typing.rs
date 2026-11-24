//! The typing-delay kernel of a hidden agent PC.
//!
//! Mirrors `crates/mud-supervisor/src/turing.rs`. The text between the
//! mirror markers must match the Rust source; only the spec clauses differ.
//!
//! Bound: `chars <= 100_000` (`MAX_TYPED_CHARS`), `reaction_ms <= 10_000`,
//! `jitter_permille <= 2_000`; `wpm` is any `u32`. Then the largest
//! intermediate, `typing * jitter`, is at most 2.4e12. `speech_delay_ms`
//! clamps `chars`, and passes reaction 800..=2200 and jitter 850..=1150.

use vstd::arithmetic::div_mod::lemma_div_is_ordered;
use vstd::arithmetic::mul::lemma_mul_inequality;
use vstd::prelude::*;

/// The arithmetic facts the typing kernel needs, broadcast so the mirrored
/// body needs no proof block. (A module cannot broadcast its own lemmas.)
mod bounds {
    use vstd::prelude::*;

    verus! {

    /// A quotient by at least 1 is between 0 and the dividend. Broadcast, so
    /// the mirrored body needs no proof block.
    pub broadcast proof fn lemma_quotient_bounds(x: int, w: int)
        requires
            0 <= x,
            1 <= w,
        ensures
            0 <= #[trigger] (x / w) <= x,
    {
        assert(0 <= x / w <= x) by (nonlinear_arith)
            requires
                0 <= x,
                1 <= w,
        ;
    }

    /// The scaled typing time stays far inside `u64`: the typing time is at
    /// most `100_000 * 12_000` and the jitter at most 2_000.
    pub broadcast proof fn lemma_scaled_bounds(t: int, j: int)
        requires
            0 <= t <= 1_200_000_000,
            0 <= j <= 2_000,
        ensures
            0 <= #[trigger] (t * j) <= 2_400_000_000_000,
    {
        assert(0 <= t * j <= 2_400_000_000_000) by (nonlinear_arith)
            requires
                0 <= t <= 1_200_000_000,
                0 <= j <= 2_000,
        ;
    }

    } // verus!
}

verus! {

/// The words per minute the kernel uses: 0 counts as 1.
pub open spec fn eff_wpm(wpm: int) -> int {
    if wpm == 0 {
        1
    } else {
        wpm
    }
}

/// The delay over unbounded integers.
pub open spec fn spec_delay(chars: int, wpm: int, reaction: int, jitter: int) -> int {
    reaction + (chars * 12_000 / eff_wpm(wpm)) * jitter / 1_000
}

broadcast use {bounds::lemma_quotient_bounds, bounds::lemma_scaled_bounds};

// mirror-begin typing_delay_ms
/// The hold before a typed command commits, in ms:
/// `reaction + chars / 5 words at wpm words per minute, scaled by jitter`.
///
/// `jitter_permille` is the scale in thousandths (850 to 1150).
#[must_use]
pub const fn typing_delay_ms(chars: u32, wpm: u32, reaction_ms: u32, jitter_permille: u32) -> (r: u64)
    requires
        chars <= 100_000,
        reaction_ms <= 10_000,
        jitter_permille <= 2_000,
    ensures
        r == spec_delay(chars as int, wpm as int, reaction_ms as int, jitter_permille as int),
        r >= reaction_ms,
{
    let wpm = if wpm == 0 { 1 } else { wpm };
    let typing = (chars as u64) * 12_000 / (wpm as u64);
    (reaction_ms as u64) + typing * (jitter_permille as u64) / 1_000
}
// mirror-end typing_delay_ms

/// A longer line never types faster.
pub proof fn lemma_delay_monotonic(c1: int, c2: int, wpm: int, reaction: int, jitter: int)
    requires
        0 <= c1 <= c2,
        0 <= wpm,
        0 <= jitter,
    ensures
        spec_delay(c1, wpm, reaction, jitter) <= spec_delay(c2, wpm, reaction, jitter),
{
    let w = eff_wpm(wpm);
    assert(c1 * 12_000 <= c2 * 12_000);
    lemma_div_is_ordered(c1 * 12_000, c2 * 12_000, w);
    let (t1, t2) = (c1 * 12_000 / w, c2 * 12_000 / w);
    lemma_mul_inequality(t1, t2, jitter);
    lemma_div_is_ordered(t1 * jitter, t2 * jitter, 1_000);
}

} // verus!
