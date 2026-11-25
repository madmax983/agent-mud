//! The one deterministic hash of the MUD: every derived choice (a wake id's
//! epoch, a session window, a typing delay) mixes a seed with some bytes
//! here, never a global RNG or the wall clock.

/// A 64-bit mix of a seed and some bytes (FNV-1a, then `SplitMix64`).
#[must_use]
pub fn mix(seed: u64, parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ seed;
    for p in parts {
        for b in *p {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut z = h.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The epoch of a `world.db` (or of an in-memory world's process): minted
/// once, from the seed and the game clock at the moment it was made.
#[must_use]
pub fn epoch(seed: u64, now_ms: u64) -> u64 {
    mix(seed, &[b"epoch", &now_ms.to_le_bytes()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_is_stable_and_separates_parts() {
        assert_eq!(mix(1, &[b"ab"]), mix(1, &[b"ab"]));
        assert_ne!(mix(1, &[b"ab"]), mix(2, &[b"ab"]));
        assert_ne!(mix(1, &[b"a", b"b"]), mix(1, &[b"ab"]));
        assert_ne!(epoch(1, 5), epoch(1, 6));
    }
}
