//! [`TokenRegistry`]: which entity each bearer token speaks for.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};

use mud_world::{EntityId, Seq};
use thiserror::Error;

/// How many random bytes a token carries. Hex doubles it: 64 characters.
const TOKEN_BYTES: usize = 32;

/// The hex digits.
const HEX: &[u8; 16] = b"0123456789abcdef";

/// A token could not be issued.
#[derive(Debug, Error)]
pub enum TokenError {
    /// The operating system gave no randomness.
    #[error("no randomness for a token: {0}")]
    Random(#[from] getrandom::Error),
}

/// One issued token.
#[derive(Debug, Clone)]
struct Grant {
    /// The entity the token acts as. Fixed for the life of the token.
    entity: EntityId,
    /// Where `mud_events` resumes when the caller names no `since_seq`.
    cursor: Seq,
}

/// The bearer tokens of the MCP server, each bound to exactly one entity.
///
/// A token is 64 random hex characters. It is bound to its entity when it
/// is issued and can never be rebound: to change the entity, revoke the
/// token and issue a new one. The registry also remembers, per token, the
/// `mud_events` cursor of that token.
///
/// The registry is thread-safe; share it as an `Arc<TokenRegistry>`.
#[derive(Default)]
pub struct TokenRegistry {
    grants: Mutex<HashMap<String, Grant>>,
}

impl fmt::Debug for TokenRegistry {
    // The tokens themselves are secrets: never print them.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenRegistry")
            .field("tokens", &self.lock().len())
            .finish()
    }
}

impl TokenRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Grant>> {
        self.grants.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Issues a fresh token that acts as `entity`. Its `mud_events` cursor
    /// starts at the beginning of the event log.
    ///
    /// # Errors
    /// The operating system gives no randomness.
    pub fn issue(&self, entity: EntityId) -> Result<String, TokenError> {
        self.issue_at(entity, Seq(0))
    }

    /// Issues a fresh token that acts as `entity`, whose `mud_events`
    /// cursor starts after `cursor`. After a restart the server passes the
    /// agent's replay floor (ADR 0011): the last event a committed wake of
    /// it consumed, so the agent is handed nothing it consumed before the
    /// restart and misses nothing after it.
    ///
    /// # Errors
    /// The operating system gives no randomness.
    pub fn issue_at(&self, entity: EntityId, cursor: Seq) -> Result<String, TokenError> {
        let mut bytes = [0_u8; TOKEN_BYTES];
        getrandom::fill(&mut bytes)?;
        let token: String = bytes
            .iter()
            .flat_map(|b| [HEX[usize::from(b >> 4)], HEX[usize::from(b & 0xf)]])
            .map(char::from)
            .collect();
        self.lock().insert(token.clone(), Grant { entity, cursor });
        Ok(token)
    }

    /// Revokes `token`. Whether it was live.
    pub fn revoke(&self, token: &str) -> bool {
        self.lock().remove(token).is_some()
    }

    /// Revokes every token of `entity`. How many there were.
    pub fn revoke_entity(&self, entity: &EntityId) -> usize {
        let mut grants = self.lock();
        let before = grants.len();
        grants.retain(|_, g| &g.entity != entity);
        before - grants.len()
    }

    /// The entity `token` acts as, while it is live.
    #[must_use]
    pub fn resolve(&self, token: &str) -> Option<EntityId> {
        self.lock().get(token).map(|g| g.entity.clone())
    }

    /// Where `mud_events` resumes, for each live token of `entity`, when it
    /// names no `since_seq`; in no particular order. The tokens themselves
    /// stay hidden.
    #[must_use]
    pub fn cursors_of(&self, entity: &EntityId) -> Vec<Seq> {
        self.lock()
            .values()
            .filter(|g| &g.entity == entity)
            .map(|g| g.cursor)
            .collect()
    }

    /// How many tokens are live.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no token is live.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Reads the remembered cursor of `token`, which must still act as
    /// `entity`, and stores the cursor `f` returns, all under one lock, so
    /// two concurrent reads of one token never hand out the same events.
    /// `None` when the token is gone or bound to another entity.
    pub(crate) fn advance_cursor<R>(
        &self,
        token: &str,
        entity: &EntityId,
        f: impl FnOnce(Seq) -> (Seq, R),
    ) -> Option<R> {
        let mut grants = self.lock();
        let grant = grants.get_mut(token).filter(|g| &g.entity == entity)?;
        let (next, out) = f(grant.cursor);
        grant.cursor = grant.cursor.max(next);
        drop(grants);
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issued_tokens_resolve_to_their_entity() {
        let reg = TokenRegistry::new();
        let a = reg.issue(EntityId::from("ada")).unwrap();
        let k = reg.issue(EntityId::from("pc_kestrel")).unwrap();
        assert_eq!(reg.resolve(&a), Some(EntityId::from("ada")));
        assert_eq!(reg.resolve(&k), Some(EntityId::from("pc_kestrel")));
        assert_eq!(reg.resolve("nope"), None);
        assert_eq!(reg.resolve(""), None);
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn tokens_are_long_random_hex() {
        let reg = TokenRegistry::new();
        let a = reg.issue(EntityId::from("ada")).unwrap();
        let b = reg.issue(EntityId::from("ada")).unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two tokens of one entity differ");
        assert_eq!(reg.resolve(&b), Some(EntityId::from("ada")));
    }

    #[test]
    fn revoked_tokens_stop_resolving() {
        let reg = TokenRegistry::new();
        let a = reg.issue(EntityId::from("ada")).unwrap();
        let a2 = reg.issue(EntityId::from("ada")).unwrap();
        let k = reg.issue(EntityId::from("pc_kestrel")).unwrap();
        assert!(reg.revoke(&a));
        assert!(!reg.revoke(&a), "already gone");
        assert_eq!(reg.resolve(&a), None);
        assert_eq!(reg.revoke_entity(&EntityId::from("ada")), 1);
        assert_eq!(reg.resolve(&a2), None);
        assert_eq!(reg.resolve(&k), Some(EntityId::from("pc_kestrel")));
        assert!(!reg.is_empty());
        assert!(reg.revoke(&k));
        assert!(reg.is_empty());
    }

    #[test]
    fn the_cursor_belongs_to_one_token_and_never_moves_back() {
        let reg = TokenRegistry::new();
        let ada = EntityId::from("ada");
        let a = reg.issue(ada.clone()).unwrap();
        let b = reg.issue(ada.clone()).unwrap();
        assert_eq!(reg.advance_cursor(&a, &ada, |c| (Seq(5), c)), Some(Seq(0)));
        assert_eq!(reg.advance_cursor(&a, &ada, |c| (Seq(3), c)), Some(Seq(5)));
        assert_eq!(reg.advance_cursor(&a, &ada, |c| (c, c)), Some(Seq(5)));
        assert_eq!(reg.advance_cursor(&b, &ada, |c| (c, c)), Some(Seq(0)));
        let other = EntityId::from("pc_kestrel");
        assert_eq!(reg.advance_cursor(&a, &other, |c| (c, c)), None);
        assert_eq!(reg.advance_cursor("nope", &ada, |c| (c, c)), None);
    }

    #[test]
    fn a_token_issued_at_a_cursor_reads_on_from_it() {
        let reg = TokenRegistry::new();
        let ada = EntityId::from("ada");
        let a = reg.issue_at(ada.clone(), Seq(42)).unwrap();
        assert_eq!(reg.cursors_of(&ada), [Seq(42)]);
        assert_eq!(reg.advance_cursor(&a, &ada, |c| (c, c)), Some(Seq(42)));
        let b = reg.issue(ada.clone()).unwrap();
        let mut both = reg.cursors_of(&ada);
        both.sort();
        assert_eq!(both, [Seq(0), Seq(42)]);
        assert!(reg.revoke(&b));
        assert_eq!(reg.cursors_of(&EntityId::from("nope")), Vec::<Seq>::new());
    }

    #[test]
    fn debug_hides_the_tokens() {
        let reg = TokenRegistry::new();
        let a = reg.issue(EntityId::from("ada")).unwrap();
        let shown = format!("{reg:?}");
        assert!(!shown.contains(&a));
        assert!(shown.contains('1'));
    }
}
