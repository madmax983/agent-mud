//! Time windows of the day, in UTC minutes. Shared by the pack check and
//! the supervisor's session schedules.

// mirror-begin day_minutes
/// Minutes in a day.
pub const DAY_MINUTES: u32 = 24 * 60;
// mirror-end day_minutes

/// A window of the day, `start..end` in minutes after midnight UTC. A
/// window whose end is before its start wraps past midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// The first minute.
    pub start: u32,
    /// The minute after the last.
    pub end: u32,
}

impl Window {
    /// Whether `minute` of the day falls inside.
    #[must_use]
    pub const fn contains(self, minute: u32) -> bool {
        let m = minute % DAY_MINUTES;
        if self.start <= self.end {
            m >= self.start && m < self.end
        } else {
            m >= self.start || m < self.end
        }
    }

    /// The window shifted by `offset` minutes, wrapping around the day.
    #[must_use]
    pub const fn shifted(self, offset: i32) -> Self {
        Self {
            start: shift_minute(self.start, offset),
            end: shift_minute(self.end, offset),
        }
    }
}

// mirror-begin shift_minute
/// A minute of the day moved by `offset`, wrapping around midnight.
///
/// The result is always in `0..DAY_MINUTES`, and shifting back by
/// `-offset` restores any minute of the day (proved in `formal/verus`).
#[must_use]
pub const fn shift_minute(minute: u32, offset: i32) -> u32 {
    let day = DAY_MINUTES as i64;
    let sum = minute as i64 + offset as i64;
    // `%` keeps the sign of `sum`, so a negative remainder is lifted by one
    // day. This is `sum.rem_euclid(day)`, written so Verus can check it.
    let rem = sum % day;
    let m = if rem < 0 { rem + day } else { rem };
    // The wrapped minute is in 0..1440, so the cast is exact.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "0..1440"
    )]
    let out = m as u32;
    out
}
// mirror-end shift_minute

/// Parses `HH:MM-HH:MM`.
#[must_use]
pub fn parse_window(text: &str) -> Option<Window> {
    let (a, b) = text.trim().split_once('-')?;
    let minute = |t: &str| -> Option<u32> {
        let (h, m) = t.trim().split_once(':')?;
        let h: u32 = h.parse().ok()?;
        let m: u32 = m.parse().ok()?;
        (h < 24 && m < 60).then_some(h * 60 + m)
    };
    Some(Window {
        start: minute(a)?,
        end: minute(b)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn shifting_is_rem_euclid(minute in any::<u32>(), offset in any::<i32>()) {
            let want = (i64::from(minute) + i64::from(offset)).rem_euclid(i64::from(DAY_MINUTES));
            prop_assert_eq!(i64::from(shift_minute(minute, offset)), want);
        }
    }

    #[test]
    fn shifting_at_the_extremes() {
        for (minute, offset) in [
            (0, i32::MIN),
            (u32::MAX, i32::MAX),
            (0, -1),
            (1439, 1),
            (5, -1445),
        ] {
            let want = (i64::from(minute) + i64::from(offset)).rem_euclid(i64::from(DAY_MINUTES));
            assert_eq!(i64::from(shift_minute(minute, offset)), want);
        }
    }

    #[test]
    fn windows_parse_and_wrap() {
        let w = parse_window("17:00-23:30").unwrap();
        assert!(w.contains(17 * 60));
        assert!(!w.contains(23 * 60 + 30));
        let night = parse_window("22:00-02:00").unwrap();
        assert!(night.contains(23 * 60));
        assert!(night.contains(60));
        assert!(!night.contains(12 * 60));
        assert!(parse_window("25:00-01:00").is_none());
        assert!(parse_window("nope").is_none());
        let s = w.shifted(-20);
        assert_eq!(s.start, 16 * 60 + 40);
        assert_eq!(
            parse_window("00:10-01:00").unwrap().shifted(-20).start,
            DAY_MINUTES - 10
        );
    }
}
