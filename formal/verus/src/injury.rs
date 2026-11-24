//! The injury kernel: penetration after DR, times the wounding multiplier.
//!
//! Mirrors `crates/mud-rules/src/damage.rs`. The text between the mirror
//! markers must match the Rust source; only the spec clauses differ.
//!
//! Bound: `raw` and `dr` in `-10_000..=10_000` (`INJURY_LIMIT`), so no
//! subtraction or multiplication overflows. `resolve_attack` clamps to it.

use vstd::prelude::*;

verus! {

/// The type of a hit. It sets the wounding multiplier.
// mirror-begin damage_type
pub enum DamageType {
    /// Blunt force: ×1.
    Crushing,
    /// Edges: ×1.5.
    Cutting,
    /// Points: ×2.
    Impaling,
    /// Fire and acid: ×1.
    Burn,
}
// mirror-end damage_type

/// The injury, read plainly: nothing unless the hit beats the DR, else the
/// penetration times the multiplier, rounded down. There is no floor at 1:
/// [`injury`] proves its own floor never fires.
pub open spec fn spec_injury(raw: int, dr: int, kind: DamageType) -> int {
    if raw <= dr {
        0
    } else {
        match kind {
            DamageType::Crushing | DamageType::Burn => raw - dr,
            DamageType::Cutting => (raw - dr) * 3 / 2,
            DamageType::Impaling => 2 * (raw - dr),
        }
    }
}

// mirror-begin injury
/// The injury of a hit: penetration after DR, times the wounding
/// multiplier, rounded down. Never negative.
///
/// A hit that penetrates at all deals at least 1 injury.
#[must_use]
pub const fn injury(raw: i32, dr: i32, kind: DamageType) -> (r: i32)
    requires
        -10_000 <= raw <= 10_000,
        -10_000 <= dr <= 10_000,
    ensures
        r == spec_injury(raw as int, dr as int, kind),
        r >= 0,
        r == 0 <==> raw <= dr,
        // Impaling doubles the penetration.
        raw > dr && kind is Impaling ==> r == 2 * (raw - dr),
        // Cutting is 1.5 times it, floored; that is never below 1, so the
        // `scaled < 1` floor never decides a cutting hit.
        raw > dr && kind is Cutting ==> r == (raw - dr) * 3 / 2 && r >= 1,
        // Crushing and burn pass the penetration through.
        raw > dr && (kind is Crushing || kind is Burn) ==> r == raw - dr,
{
    let pen = if raw > dr { raw - dr } else { 0 };
    if pen == 0 {
        return 0;
    }
    let scaled = match kind {
        DamageType::Crushing | DamageType::Burn => pen,
        DamageType::Cutting => pen * 3 / 2,
        DamageType::Impaling => pen * 2,
    };
    if scaled < 1 { 1 } else { scaled }
}
// mirror-end injury

/// Injury never decreases as the raw damage grows.
pub proof fn lemma_injury_monotonic(raw1: int, raw2: int, dr: int, kind: DamageType)
    requires
        raw1 <= raw2,
    ensures
        spec_injury(raw1, dr, kind) <= spec_injury(raw2, dr, kind),
{
    if raw1 > dr {
        let (p1, p2) = (raw1 - dr, raw2 - dr);
        assert(p1 * 3 / 2 <= p2 * 3 / 2) by (nonlinear_arith)
            requires
                0 < p1 <= p2,
        ;
    }
}

/// Injury is never negative, for any input.
pub proof fn lemma_injury_nonnegative(raw: int, dr: int, kind: DamageType)
    ensures
        spec_injury(raw, dr, kind) >= 0,
        spec_injury(raw, dr, kind) == 0 <==> raw <= dr,
{
    if raw > dr {
        let p = raw - dr;
        assert(p * 3 / 2 >= 1) by (nonlinear_arith)
            requires
                p >= 1,
        ;
    }
}

} // verus!
