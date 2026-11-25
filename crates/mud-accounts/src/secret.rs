//! A password or a token in memory.

use zeroize::Zeroizing;

/// A password or a resume token. Its memory is wiped when it drops; it has
/// no `Display`, `Serialize` or `Clone`, and its `Debug` shows nothing of
/// it, so it cannot slip into a log line or an event by accident.
pub struct Secret(Zeroizing<String>);

impl Secret {
    /// Wraps `s`.
    #[must_use]
    pub fn new(s: String) -> Self {
        Self(Zeroizing::new(s))
    }

    /// The text, for the one place that needs it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Its length in UTF-8 bytes.
    #[must_use]
    pub fn len_bytes(&self) -> usize {
        self.0.len()
    }

    /// Whether it is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<String> for Secret {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl From<&str> for Secret {
    fn from(s: &str) -> Self {
        Self::new(s.to_owned())
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_the_secret() {
        let s = Secret::from("hunter2");
        assert_eq!(format!("{s:?}"), "Secret(..)");
        assert_eq!(format!("{:?}", Some(&s)), "Some(Secret(..))");
        assert_eq!(s.expose(), "hunter2");
        assert_eq!(s.len_bytes(), 7);
        assert!(!s.is_empty());
        assert!(Secret::new(String::new()).is_empty());
    }
}
