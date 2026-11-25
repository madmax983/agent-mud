//! What a new password must be.

use autumn_web::auth::{PasswordFailure, PasswordPolicy, validate_password};

use crate::hasher::Hasher;
use crate::secret::Secret;

/// Why a new password was refused. The `Display` text is what the player
/// reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PasswordRejection {
    /// Over 72 UTF-8 bytes.
    #[error("A password is at most 72 bytes long.")]
    TooLong,
    /// Leading or trailing white space.
    #[error("A password cannot start or end with a space.")]
    EdgeSpace,
    /// The connection could not decode it.
    #[error(
        "That password has characters this connection could not read; use plain letters, digits and symbols."
    )]
    Unreadable,
    /// Too short.
    #[error("A password needs at least {min} characters.")]
    TooShort {
        /// The minimum.
        min: usize,
    },
    /// In the weak-password list.
    #[error("That password is too common; choose something less guessable.")]
    TooCommon,
    /// The name, or built on it.
    #[error("A password cannot be your name or built on it.")]
    ContainsName,
}

/// Checks a new password for the character `name`. Passwords are never
/// trimmed or normalized; the first failing rule wins.
///
/// # Errors
/// The rule it breaks.
pub async fn check_new_password(
    pw: &Secret,
    name: &str,
    policy: &PasswordPolicy,
) -> Result<(), PasswordRejection> {
    let text = pw.expose();
    if pw.len_bytes() > Hasher::MAX_BYTES {
        return Err(PasswordRejection::TooLong);
    }
    if text.starts_with(char::is_whitespace) || text.ends_with(char::is_whitespace) {
        return Err(PasswordRejection::EdgeSpace);
    }
    if text.contains('\u{FFFD}') {
        return Err(PasswordRejection::Unreadable);
    }
    let v = validate_password(text, policy, &[name]).await;
    match v.failures.first() {
        None => Ok(()),
        Some(PasswordFailure::TooShort { min, .. }) => {
            Err(PasswordRejection::TooShort { min: *min })
        }
        Some(PasswordFailure::SimilarToContext) => Err(PasswordRejection::ContainsName),
        Some(
            PasswordFailure::TooCommon
            | PasswordFailure::Breached
            | PasswordFailure::BreachCheckUnavailable,
        ) => Err(PasswordRejection::TooCommon),
    }
}

#[cfg(test)]
mod tests {
    use autumn_web::auth::BreachCheck;

    use super::*;

    fn policy() -> PasswordPolicy {
        PasswordPolicy::new(8, true, BreachCheck::Off)
    }

    async fn check(pw: &str) -> Result<(), PasswordRejection> {
        check_new_password(&Secret::from(pw), "ada", &policy()).await
    }

    #[tokio::test]
    async fn every_rule_has_its_exact_text() {
        let cases = [
            (
                "x".repeat(73),
                PasswordRejection::TooLong,
                "A password is at most 72 bytes long.",
            ),
            (
                " leading-space-pw".to_owned(),
                PasswordRejection::EdgeSpace,
                "A password cannot start or end with a space.",
            ),
            (
                "trailing-tab-pw\t".to_owned(),
                PasswordRejection::EdgeSpace,
                "A password cannot start or end with a space.",
            ),
            (
                "caf\u{FFFD}-lantern".to_owned(),
                PasswordRejection::Unreadable,
                "That password has characters this connection could not read; use plain letters, digits and symbols.",
            ),
            (
                "short".to_owned(),
                PasswordRejection::TooShort { min: 8 },
                "A password needs at least 8 characters.",
            ),
            (
                "password".to_owned(),
                PasswordRejection::TooCommon,
                "That password is too common; choose something less guessable.",
            ),
            (
                "ada12345".to_owned(),
                PasswordRejection::ContainsName,
                "A password cannot be your name or built on it.",
            ),
        ];
        for (pw, want, text) in cases {
            let got = check(&pw).await.unwrap_err();
            assert_eq!(got, want, "{pw:?}");
            assert_eq!(got.to_string(), text);
        }
        assert_eq!(check("correct horse battery").await, Ok(()));
        assert_eq!(
            check(&"y".repeat(72)).await,
            Ok(()),
            "exactly 72 bytes is fine"
        );
    }

    #[tokio::test]
    async fn the_byte_limit_matches_the_hasher() {
        let h = Hasher::for_tests(4);
        for pw in [
            "correct horse battery",
            &"\u{20AC}".repeat(24),
            &"z9".repeat(36),
        ] {
            let s = Secret::from(pw);
            assert_eq!(check(pw).await, Ok(()), "{pw}");
            assert!(h.hash(&s).await.is_ok(), "{pw}");
        }
        let long = Secret::from("\u{20AC}".repeat(25).as_str());
        assert_eq!(check(long.expose()).await, Err(PasswordRejection::TooLong));
        assert!(h.hash(&long).await.is_err());
    }
}
