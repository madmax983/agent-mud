//! Time windows of the day, in UTC minutes. Shared by the pack check and
//! the supervisor's session schedules.

/// Minutes in a day.
pub const DAY_MINUTES: u32 = 24 * 60;

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

/// A minute of the day moved by `offset`, wrapping around midnight.
#[must_use]
pub const fn shift_minute(minute: u32, offset: i32) -> u32 {
    let day = DAY_MINUTES as i64;
    let m = (minute as i64 + offset as i64).rem_euclid(day);
    // rem_euclid by a day is in 0..1440, so the cast is exact.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "0..1440"
    )]
    let out = m as u32;
    out
}

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
