//! Errors.

use std::path::PathBuf;

use crate::policy::PasswordRejection;

/// A failure of the account service itself.
#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    /// SQLite failed.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// bcrypt failed.
    #[error("hashing a password: {0}")]
    Hash(#[from] bcrypt::BcryptError),
    /// A blocking job died.
    #[error("a password job stopped before it finished: {0}")]
    Join(#[from] tokio::task::JoinError),
    /// The OS had no randomness.
    #[error("no randomness: {0}")]
    Random(String),
    /// `accounts.db` is from a newer server.
    #[error("accounts.db is schema {found}; this server knows 1")]
    Schema {
        /// Its `user_version`.
        found: i64,
    },
    /// The file could not be made.
    #[error("creating {path}: {source}")]
    Create {
        /// The file.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// A password over 72 bytes reached the hasher.
    #[error("a password over 72 bytes reached the hasher")]
    TooLong,
    /// The name is not a human character.
    #[error("{0} is not a human character; only human characters have accounts")]
    NotHuman(String),
    /// No account has that name.
    #[error("there is no account named {0}")]
    NoAccount(String),
}

/// Why a login or a password change did not go through. The `Display`
/// text is what the player reads.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The password is wrong, or the name may not be logged into.
    #[error("Wrong password.")]
    WrongPassword,
    /// The name is held.
    #[error("Too many failed logins for that name. Try again in {}.", wait_text(*wait_ms))]
    NameHeld {
        /// How long.
        wait_ms: u64,
    },
    /// The address is held.
    #[error("Too many failed logins from your address. Try again in {}.", wait_text(*wait_ms))]
    AddressHeld {
        /// How long.
        wait_ms: u64,
    },
    /// The address has spent its password checks.
    #[error("Too many password checks from your address. Try again in {}.", wait_text(*wait_ms))]
    WorkHeld {
        /// How long.
        wait_ms: u64,
    },
    /// A table is full, or this name and address are already being checked.
    #[error("The server is busy. Try again in a moment.")]
    Busy,
    /// The account is locked.
    #[error("That character is locked. Ask the keeper of this world.")]
    Locked,
    /// New characters are off.
    #[error("This world is not taking new characters right now.")]
    ClosedToNew,
    /// The address made its new characters today.
    #[error("Too many new characters from your address today. Try again tomorrow.")]
    NewPerAddress,
    /// The world made its new characters today.
    #[error("Too many new characters today. Try again tomorrow.")]
    NewPerDay,
    /// The new password breaks a rule.
    #[error("{0}")]
    Weak(PasswordRejection),
    /// The two new passwords differ.
    #[error("The passwords did not match.")]
    Mismatch,
    /// Someone made the name between the question and the answer.
    #[error("Someone just took that name.")]
    NameJustTaken,
    /// A resume token did not open a session.
    #[error("Your saved login has expired.")]
    ResumeRejected,
    /// The service failed.
    #[error("The account service failed; try again later.")]
    Unavailable(#[source] AccountError),
}

impl From<AccountError> for AuthError {
    fn from(e: AccountError) -> Self {
        Self::Unavailable(e)
    }
}

/// `1 second`, `{n} seconds` (rounded up, up to 119), or `{n} minutes`
/// (rounded up).
#[must_use]
pub fn wait_text(ms: u64) -> String {
    let secs = ms.div_ceil(1_000).max(1);
    if secs == 1 {
        "1 second".to_owned()
    } else if secs < 120 {
        format!("{secs} seconds")
    } else {
        format!("{} minutes", secs.div_ceil(60))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_read_as_seconds_then_minutes() {
        assert_eq!(wait_text(0), "1 second");
        assert_eq!(wait_text(1_000), "1 second");
        assert_eq!(wait_text(1_001), "2 seconds");
        assert_eq!(wait_text(119_000), "119 seconds");
        assert_eq!(wait_text(119_001), "2 minutes");
        assert_eq!(wait_text(900_000), "15 minutes");
        assert_eq!(
            AuthError::NameHeld { wait_ms: 4_000 }.to_string(),
            "Too many failed logins for that name. Try again in 4 seconds."
        );
        assert_eq!(
            AuthError::from(AccountError::TooLong).to_string(),
            "The account service failed; try again later."
        );
    }
}
