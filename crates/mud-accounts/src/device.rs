//! Web resume tokens: one static `series:token` per browser, sent as an
//! explicit WebSocket message, never in a cookie, and never rotated.
//!
//! Rotation assumes a cookie jar that atomically holds the newest value;
//! over WebSocket messages a lost `remember` message or a duplicated tab
//! would read as theft. A static token is revoked instead: by `logout`, a
//! password change, an operator reset, lock or sign-out, and by presenting
//! a wrong token for its series.

use autumn_web::auth::{
    format_remember_cookie_value, generate_remember_credential, hash_remember_token,
    parse_remember_cookie_value, verify_remember_token,
};

use crate::secret::Secret;

/// A device's series: the public half of its token, the key of its row.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Series(String);

impl Series {
    /// Wraps a series id.
    #[must_use]
    pub const fn new(s: String) -> Self {
        Self(s)
    }

    /// The id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A token handed to a browser.
#[derive(Debug)]
pub struct IssuedToken {
    /// `series:token`, for the client to keep.
    pub value: Secret,
    /// Its series.
    pub series: Series,
    /// Whether it is a "stay signed in" token (else a tab token).
    pub persist: bool,
}

/// A fresh credential: the token for the client and the hash to store.
pub(crate) struct Fresh {
    pub series: Series,
    pub value: Secret,
    pub token_hash: String,
}

/// Mints a fresh credential from the OS CSPRNG (autumn-web's 128-bit
/// series and token).
pub(crate) fn fresh() -> Fresh {
    let c = generate_remember_credential();
    let token_hash = hash_remember_token(&c.token);
    let value = Secret::new(format_remember_cookie_value(&c.series, &c.token));
    Fresh {
        series: Series(c.series),
        value,
        token_hash,
    }
}

/// Splits a presented credential into its series and token.
pub(crate) fn parse(credential: &Secret) -> Option<(Series, Secret)> {
    parse_remember_cookie_value(credential.expose())
        .map(|(series, token)| (Series(series), Secret::new(token)))
}

/// Whether `token` matches a stored hash (constant time).
pub(crate) fn matches(token: &Secret, stored_hash: &str) -> bool {
    verify_remember_token(token.expose(), stored_hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_token_parses_and_matches_only_its_hash() {
        let f = fresh();
        let (series, token) = parse(&f.value).unwrap();
        assert_eq!(series, f.series);
        assert!(matches(&token, &f.token_hash));
        assert!(!matches(&Secret::from("nope"), &f.token_hash));
        assert!(!f.token_hash.contains(token.expose()));
        assert!(parse(&Secret::from("no-separator")).is_none());
        assert!(parse(&Secret::from(":x")).is_none());
        assert_eq!(Series::new("s".into()).as_str(), "s");
    }
}
