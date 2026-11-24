//! Friction for hidden agent PCs: session windows, selective attention,
//! distracted spells and typing delay.
//!
//! A convincing player is not one with better prose. It logs in late,
//! misses things, steps away, and takes time to type. Every random choice
//! here derives from a hash of the entity and the moment, so a replay of
//! the same day makes the same choices.

use mud_world::pack::TuringSpec;
use mud_world::schedule::{DAY_MINUTES, Window, parse_window};

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

/// A number in `lo..=hi` from a hash.
#[must_use]
pub const fn pick(hash: u64, lo: u64, hi: u64) -> u64 {
    if hi <= lo {
        lo
    } else {
        lo + hash % (hi - lo + 1)
    }
}

/// The offset of today's session windows, in minutes, in `-bound..=bound`.
#[must_use]
pub fn daily_offset(seed: u64, entity: &str, day: u64, bound: u32) -> i32 {
    let span = u64::from(bound) * 2;
    let v = pick(
        mix(seed, &[entity.as_bytes(), &day.to_le_bytes(), b"offset"]),
        0,
        span,
    );
    i32::try_from(v).unwrap_or(0) - i32::try_from(bound).unwrap_or(0)
}

/// Whether the PC should be logged in at `now_ms`.
#[must_use]
pub fn in_session(spec: &TuringSpec, seed: u64, entity: &str, now_ms: u64) -> bool {
    let day = now_ms / 86_400_000;
    let minute = u32::try_from((now_ms / 60_000) % u64::from(DAY_MINUTES)).unwrap_or(0);
    let offset = daily_offset(seed, entity, day, spec.daily_offset_minutes);
    spec.active_windows_utc
        .iter()
        .filter_map(|w| parse_window(w))
        .map(|w: Window| w.shifted(offset))
        .any(|w| w.contains(minute))
}

/// What a room batch looks like to the attention filter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Salience {
    /// The PC was named.
    pub named: bool,
    /// Combat or loot happened.
    pub combat_or_loot: bool,
    /// A goal keyword appeared.
    pub goal_keyword: bool,
}

/// Whether a hidden PC notices an event: always when named, in a fight,
/// near loot, or on a goal keyword; otherwise with `wake_chance`.
#[must_use]
pub fn notices(spec: &TuringSpec, s: Salience, hash: u64) -> bool {
    if s.named || s.combat_or_loot || s.goal_keyword {
        return true;
    }
    let chance = spec.wake_chance.clamp(0.0, 1.0);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "0..=10000"
    )]
    let threshold = (chance * 10_000.0).round() as u64;
    pick(hash, 0, 9_999) < threshold
}

/// Whether `text` holds one of the goal keywords.
#[must_use]
pub fn has_keyword(spec: &TuringSpec, text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    spec.goal_keywords
        .iter()
        .any(|k| t.contains(&k.to_ascii_lowercase()))
}

// mirror-begin typing_delay_ms
/// The hold before a typed command commits, in ms:
/// `reaction + chars / 5 words at wpm words per minute, scaled by jitter`.
///
/// `jitter_permille` is the scale in thousandths (850 to 1150).
#[must_use]
pub const fn typing_delay_ms(chars: u32, wpm: u32, reaction_ms: u32, jitter_permille: u32) -> u64 {
    let wpm = if wpm == 0 { 1 } else { wpm };
    let typing = (chars as u64) * 12_000 / (wpm as u64);
    (reaction_ms as u64) + typing * (jitter_permille as u64) / 1_000
}
// mirror-end typing_delay_ms

/// The longest line [`typing_delay_ms`] is proved for (`formal/verus`):
/// longer counts are clamped. The parser already rejects lines this long.
pub const MAX_TYPED_CHARS: usize = 100_000;

/// The hold before a typed (speech) command, from a hash.
#[must_use]
pub fn speech_delay_ms(spec: &TuringSpec, chars: usize, hash: u64) -> u64 {
    let reaction = u32::try_from(pick(hash, 800, 2_200)).unwrap_or(800);
    let jitter = u32::try_from(pick(hash >> 20, 850, 1_150)).unwrap_or(1_000);
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "wpm is small and positive"
    )]
    let wpm = spec.wpm.max(1.0).round() as u32;
    typing_delay_ms(
        u32::try_from(chars.min(MAX_TYPED_CHARS)).unwrap_or(0),
        wpm,
        reaction,
        jitter,
    )
}

/// The hold before a movement or combat command: 600 to 1400 ms.
#[must_use]
pub const fn action_delay_ms(hash: u64) -> u64 {
    pick(hash, 600, 1_400)
}

/// The distracted-spell schedule of one PC.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Afk {
    /// When the next spell starts.
    pub next_at_ms: u64,
    /// When the current spell ends, if one is on.
    pub until_ms: Option<u64>,
}

impl Afk {
    /// Advances the schedule; returns whether the PC is away at `now`.
    pub fn update(
        &mut self,
        spec: &TuringSpec,
        seed: u64,
        entity: &str,
        now_ms: u64,
        idle_in_town: bool,
    ) -> bool {
        if let Some(until) = self.until_ms {
            if now_ms < until {
                return true;
            }
            self.until_ms = None;
            self.schedule_next(spec, seed, entity, now_ms);
        }
        if self.next_at_ms == 0 {
            self.schedule_next(spec, seed, entity, now_ms);
        }
        if idle_in_town && now_ms >= self.next_at_ms {
            let h = mix(
                seed,
                &[entity.as_bytes(), &now_ms.to_le_bytes(), b"afk-len"],
            );
            let mins = pick(
                h,
                u64::from(spec.afk_minutes[0]),
                u64::from(spec.afk_minutes[1]),
            );
            self.until_ms = Some(now_ms + mins * 60_000);
            return true;
        }
        false
    }

    fn schedule_next(&mut self, spec: &TuringSpec, seed: u64, entity: &str, now_ms: u64) {
        let h = mix(
            seed,
            &[entity.as_bytes(), &now_ms.to_le_bytes(), b"afk-gap"],
        );
        let mins = pick(
            h,
            u64::from(spec.afk_every_minutes[0]),
            u64::from(spec.afk_every_minutes[1]),
        );
        self.next_at_ms = now_ms + mins * 60_000;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn spec() -> TuringSpec {
        TuringSpec {
            active_windows_utc: vec!["17:00-23:30".into()],
            daily_offset_minutes: 20,
            wake_chance: 0.35,
            goal_keywords: vec!["cellar".into(), "ore".into()],
            wpm: 55.0,
            afk_every_minutes: [20, 60],
            afk_minutes: [2, 6],
        }
    }

    #[test]
    fn the_formula_matches_the_design() {
        // 55 chars at 60 wpm: 11 words, 11 s; plus 1 s reaction, ×1.0.
        assert_eq!(typing_delay_ms(55, 60, 1_000, 1_000), 12_000);
        assert_eq!(typing_delay_ms(0, 60, 800, 1_150), 800);
        assert_eq!(
            typing_delay_ms(10, 0, 0, 1_000),
            120_000,
            "zero wpm is treated as one"
        );
        let d = speech_delay_ms(&spec(), 30, 12345);
        assert!((800 + 5_563..=2_200 + 7_528).contains(&d), "{d}");
        for h in 0..200 {
            assert!((600..=1_400).contains(&action_delay_ms(h)));
        }
    }

    #[test]
    fn offsets_are_bounded_and_stable_per_day() {
        for day in 0..100 {
            let o = daily_offset(1, "kestrel", day, 20);
            assert!((-20..=20).contains(&o));
            assert_eq!(o, daily_offset(1, "kestrel", day, 20));
        }
        let all_same = (0..30)
            .map(|d| daily_offset(1, "kestrel", d, 20))
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == 1;
        assert!(!all_same, "the offset varies from day to day");
    }

    #[test]
    fn sessions_follow_the_windows() {
        let s = spec();
        let noon = 12 * 3_600_000;
        assert!(!in_session(&s, 1, "kestrel", noon));
        let eight_pm = 20 * 3_600_000;
        assert!(in_session(&s, 1, "kestrel", eight_pm));
        let always = TuringSpec {
            active_windows_utc: vec!["00:00-23:59".into()],
            daily_offset_minutes: 0,
            ..s
        };
        assert!(in_session(&always, 1, "kestrel", noon));
    }

    #[test]
    fn attention_filters_unaddressed_chatter() {
        let s = spec();
        assert!(notices(
            &s,
            Salience {
                named: true,
                ..Salience::default()
            },
            9_999
        ));
        assert!(notices(
            &s,
            Salience {
                goal_keyword: true,
                ..Salience::default()
            },
            9_999
        ));
        let woke = (0..10_000u64)
            .filter(|h| notices(&s, Salience::default(), mix(3, &[&h.to_le_bytes()])))
            .count();
        assert!((3_000..4_000).contains(&woke), "about 35%: {woke}");
        assert!(has_keyword(&s, "Anyone going to the CELLAR?"));
        assert!(!has_keyword(&s, "nice weather"));
    }

    #[test]
    fn afk_spells_come_and_go() {
        let s = spec();
        let mut a = Afk::default();
        assert!(!a.update(&s, 1, "kestrel", 1_000, true));
        let start = a.next_at_ms;
        assert!((1_000 + 20 * 60_000..=1_000 + 60 * 60_000).contains(&start));
        assert!(a.update(&s, 1, "kestrel", start, true));
        let until = a.until_ms.unwrap();
        assert!((start + 2 * 60_000..=start + 6 * 60_000).contains(&until));
        assert!(
            a.update(&s, 1, "kestrel", until - 1, false),
            "a spell runs its course"
        );
        assert!(!a.update(&s, 1, "kestrel", until, false));
        assert!(
            !a.update(&s, 1, "kestrel", a.next_at_ms, false),
            "only idle in town"
        );
    }

    proptest! {
        #[test]
        fn typing_delay_grows_with_length(chars in 0u32..2_000, wpm in 1u32..200, r in 0u32..3_000, j in 850u32..=1_150) {
            prop_assert!(typing_delay_ms(chars + 1, wpm, r, j) >= typing_delay_ms(chars, wpm, r, j));
            prop_assert!(typing_delay_ms(chars, wpm, r, j) >= u64::from(r));
        }

        #[test]
        fn a_huge_line_types_as_the_longest_proved_one(hash in any::<u64>()) {
            let s = spec();
            let longest = speech_delay_ms(&s, MAX_TYPED_CHARS, hash);
            prop_assert_eq!(speech_delay_ms(&s, usize::MAX, hash), longest);
            prop_assert!(longest > speech_delay_ms(&s, MAX_TYPED_CHARS - 1_000, hash));
        }
    }
}
