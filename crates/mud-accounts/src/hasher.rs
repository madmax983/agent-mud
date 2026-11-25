//! bcrypt, one verify per answer.
//!
//! bcrypt 0.19's `non_truncating_*` functions refuse inputs of 72 bytes or
//! more, so this uses the truncating `hash` and `verify` behind its own
//! 72-byte check: `hash` refuses longer input, and `verify` checks the
//! first 72 bytes of a longer input against the same hash a shorter
//! answer to that name would meet (so at the same cost) and answers
//! `false`.
//!
//! A name with no hash (no account, an agent PC, a sealed legacy human)
//! verifies against a dummy hash. The dummy's cost follows the mix of the
//! stored hashes' costs, chosen per name from a salted hash of the name, so
//! a name with no account looks like an account hashed at some cost in the
//! same proportions as the real ones.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::error::AccountError;
use crate::name::NameKey;
use crate::secret::Secret;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The cost of a bcrypt hash, if it parses.
#[must_use]
pub fn cost_of(hash: &str) -> Option<u32> {
    hash.parse::<bcrypt::HashParts>().ok().map(|p| p.get_cost())
}

/// Fills `buf` from the OS CSPRNG.
///
/// # Errors
/// The OS has no randomness.
pub fn random(buf: &mut [u8]) -> Result<(), AccountError> {
    getrandom::fill(buf).map_err(|e| AccountError::Random(e.to_string()))
}

/// Lowercase hex.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(DIGITS[usize::from(b >> 4)]));
        s.push(char::from(DIGITS[usize::from(b & 15)]));
    }
    s
}

/// A dummy hash at `cost`: the hash of 32 random bytes nobody knows.
fn make_dummy(cost: u32) -> Result<String, AccountError> {
    let mut raw = Zeroizing::new([0u8; 32]);
    random(raw.as_mut())?;
    Ok(bcrypt::hash(hex(raw.as_ref()), cost)?)
}

/// The hash one verify checks.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    /// The stored hash.
    Real(String),
    /// The dummy at this cost.
    Dummy(u32),
}

impl Target {
    #[cfg(test)]
    fn cost(&self) -> Option<u32> {
        match self {
            Self::Real(h) => cost_of(h),
            Self::Dummy(c) => Some(*c),
        }
    }
}

/// Hashes and checks passwords on blocking threads, a bounded number at a
/// time.
#[derive(Debug)]
pub struct Hasher {
    cost: u32,
    salt: [u8; 32],
    mix: Mutex<BTreeMap<u32, u64>>,
    dummies: Mutex<BTreeMap<u32, String>>,
    permits: tokio::sync::Semaphore,
    verifications: AtomicU64,
}

impl Hasher {
    /// The longest password bcrypt reads.
    pub const MAX_BYTES: usize = 72;

    /// A hasher at `cost`, keyed by `salt`, with the stored hashes' cost
    /// mix, running at most `concurrency` jobs at once.
    #[must_use]
    pub fn new(cost: u32, salt: [u8; 32], mix: BTreeMap<u32, u64>, concurrency: usize) -> Self {
        Self {
            cost,
            salt,
            mix: Mutex::new(mix),
            dummies: Mutex::new(BTreeMap::new()),
            permits: tokio::sync::Semaphore::new(concurrency.max(1)),
            verifications: AtomicU64::new(0),
        }
    }

    /// A hasher for tests: `cost`, a zero salt, no stored hashes.
    #[must_use]
    pub fn for_tests(cost: u32) -> Self {
        Self::new(cost, [0; 32], BTreeMap::new(), 2)
    }

    /// The configured cost.
    #[must_use]
    pub const fn cost(&self) -> u32 {
        self.cost
    }

    /// Makes the dummy hash of every cost in the mix and the configured
    /// one, so no answer pays for making one.
    ///
    /// # Errors
    /// bcrypt or the OS randomness failed.
    pub async fn prepare(&self) -> Result<(), AccountError> {
        let mut costs: Vec<u32> = lock(&self.mix).keys().copied().collect();
        costs.push(self.cost);
        costs.sort_unstable();
        costs.dedup();
        let made = tokio::task::spawn_blocking(move || {
            costs
                .into_iter()
                .map(|c| make_dummy(c).map(|d| (c, d)))
                .collect::<Result<Vec<_>, _>>()
        })
        .await??;
        lock(&self.dummies).extend(made);
        Ok(())
    }

    async fn permit(&self) -> Result<tokio::sync::SemaphorePermit<'_>, AccountError> {
        self.permits
            .acquire()
            .await
            .map_err(|_| AccountError::Random("the password checker is closed".to_owned()))
    }

    /// Hashes a new password at the configured cost.
    ///
    /// # Errors
    /// [`AccountError::TooLong`] over 72 bytes, or bcrypt failed.
    pub async fn hash(&self, pw: &Secret) -> Result<String, AccountError> {
        if pw.len_bytes() > Self::MAX_BYTES {
            return Err(AccountError::TooLong);
        }
        let input = Zeroizing::new(pw.expose().as_bytes().to_vec());
        let cost = self.cost;
        let _permit = self.permit().await?;
        Ok(tokio::task::spawn_blocking(move || bcrypt::hash(&*input, cost)).await??)
    }

    /// What one verify of an answer `len` bytes long to `key` checks
    /// against, and whether a match may count.
    fn target(&self, key: &NameKey, hash: Option<&str>, len: usize) -> (Target, bool) {
        let real = match hash {
            Some(h) if cost_of(h).is_some() => Some(h.to_owned()),
            Some(_) => {
                tracing::warn!(target: "mud::auth", name = %key, "a stored password hash does not parse; it can never match");
                None
            }
            None => None,
        };
        let counts = real.is_some() && len <= Self::MAX_BYTES;
        let t = real.map_or_else(|| Target::Dummy(self.dummy_cost(key)), Target::Real);
        (t, counts)
    }

    /// Checks `pw` against `hash`, with exactly one bcrypt verify whatever
    /// the input. A missing or malformed hash checks the name's dummy and
    /// answers `false`. A password over 72 bytes checks its first 72
    /// bytes against the stored hash when there is one (the dummy when
    /// not), so its cost is the cost of any other answer to that name,
    /// and answers `false`.
    ///
    /// # Errors
    /// bcrypt or the blocking pool failed.
    pub async fn verify(
        &self,
        key: &NameKey,
        pw: &Secret,
        hash: Option<&str>,
    ) -> Result<bool, AccountError> {
        let bytes = pw.expose().as_bytes();
        let (target, counts) = self.target(key, hash, bytes.len());
        let dummy = match &target {
            Target::Dummy(c) => lock(&self.dummies).get(c).cloned(),
            Target::Real(_) => None,
        };
        let input = Zeroizing::new(bytes[..bytes.len().min(Self::MAX_BYTES)].to_vec());
        let permit = self.permit().await?;
        let (ok, made) = tokio::task::spawn_blocking(move || {
            let (against, made) = match (target, dummy) {
                (Target::Real(h), _) | (Target::Dummy(_), Some(h)) => (h, None),
                (Target::Dummy(c), None) => {
                    let d = make_dummy(c)?;
                    (d.clone(), Some((c, d)))
                }
            };
            Ok::<_, AccountError>((bcrypt::verify(&*input, &against)?, made))
        })
        .await??;
        drop(permit);
        self.verifications.fetch_add(1, Ordering::Relaxed);
        if let Some((c, d)) = made {
            lock(&self.dummies).entry(c).or_insert(d);
        }
        Ok(counts && ok)
    }

    /// The cost of the dummy hash `key` checks against: a cost from the
    /// stored mix, weighted by count, chosen by a salted hash of the name.
    /// The same name always gets the same cost.
    #[must_use]
    pub fn dummy_cost(&self, key: &NameKey) -> u32 {
        let mix: Vec<(u32, u64)> = lock(&self.mix).iter().map(|(c, n)| (*c, *n)).collect();
        let total: u64 = mix.iter().map(|(_, n)| n).sum();
        if total == 0 {
            return self.cost;
        }
        let mut h = Sha256::new();
        h.update(self.salt);
        h.update(key.as_str().as_bytes());
        let d = h.finalize();
        let mut first = [0u8; 8];
        first.copy_from_slice(&d[..8]);
        let mut x = u64::from_be_bytes(first) % total;
        for (cost, n) in mix {
            if x < n {
                return cost;
            }
            x -= n;
        }
        self.cost
    }

    /// Whether a stored hash should be redone at the configured cost.
    #[must_use]
    pub fn needs_rehash(&self, hash: &str) -> bool {
        cost_of(hash).is_some_and(|c| c != self.cost)
    }

    /// Records a stored hash moving from cost `old` (none: a new hash) to
    /// `new`.
    pub fn note_cost(&self, old: Option<u32>, new: u32) {
        let mut mix = lock(&self.mix);
        if let Some(o) = old
            && let Some(n) = mix.get_mut(&o)
        {
            *n = n.saturating_sub(1);
            if *n == 0 {
                mix.remove(&o);
            }
        }
        *mix.entry(new).or_default() += 1;
    }

    /// bcrypt verifies run so far.
    #[must_use]
    pub fn verifications(&self) -> u64 {
        self.verifications.load(Ordering::Relaxed)
    }

    /// bcrypt slots free now.
    #[must_use]
    pub fn free_slots(&self) -> usize {
        self.permits.available_permits()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> NameKey {
        NameKey::parse(s).unwrap()
    }

    #[tokio::test]
    async fn hashes_carry_the_configured_cost() {
        let h = Hasher::for_tests(4);
        let hash = h.hash(&Secret::from("lantern-and-jerkin")).await.unwrap();
        assert!(hash.starts_with("$2b$04$"), "{hash}");
        assert_eq!(cost_of(&hash), Some(4));
        assert!(!h.needs_rehash(&hash));
        assert!(Hasher::for_tests(5).needs_rehash(&hash));
    }

    #[tokio::test]
    async fn a_72_byte_password_hashes_and_verifies() {
        let h = Hasher::for_tests(4);
        for pw in ["a".repeat(72), "\u{20AC}".repeat(24)] {
            assert_eq!(pw.len(), 72);
            let s = Secret::from(pw.as_str());
            let hash = h.hash(&s).await.unwrap();
            assert!(h.verify(&key("ada"), &s, Some(&hash)).await.unwrap());
        }
    }

    #[tokio::test]
    async fn over_72_bytes_never_verifies_and_still_costs_one_verify() {
        let h = Hasher::for_tests(4);
        let real = "b".repeat(72);
        let hash = h.hash(&Secret::from(real.as_str())).await.unwrap();
        let before = h.verifications();
        let longer = Secret::from(format!("{real}c").as_str());
        assert!(!h.verify(&key("ada"), &longer, Some(&hash)).await.unwrap());
        assert_eq!(h.verifications(), before + 1);
        assert!(matches!(h.hash(&longer).await, Err(AccountError::TooLong)));
    }

    #[tokio::test]
    async fn a_missing_or_malformed_hash_checks_a_dummy() {
        let h = Hasher::for_tests(4);
        h.prepare().await.unwrap();
        let pw = Secret::from("anything-at-all");
        for stored in [None, Some("not a hash"), Some("$2b$04$short")] {
            let before = h.verifications();
            assert!(!h.verify(&key("ada"), &pw, stored).await.unwrap());
            assert_eq!(h.verifications(), before + 1, "{stored:?}");
        }
        // A cost with no dummy yet makes one inside the same job.
        let cold = Hasher::for_tests(4);
        assert!(!cold.verify(&key("ada"), &pw, None).await.unwrap());
        assert_eq!(cold.verifications(), 1);
    }

    /// The cost of the hash an answer is checked against depends on the
    /// name only, never on the answer's length: a long answer to a human
    /// account must not run at a dummy's cost.
    #[tokio::test]
    async fn the_cost_checked_never_depends_on_the_answer_length() {
        // Nearly every stored hash is at cost 5; ada's is at cost 4.
        let h = Hasher::new(5, [3; 32], BTreeMap::from([(5, 1_000), (4, 1)]), 1);
        let ada = key("ada");
        assert_eq!(h.dummy_cost(&ada), 5);
        let stored = Hasher::for_tests(4)
            .hash(&Secret::from("lantern-and-jerkin"))
            .await
            .unwrap();
        for len in [0, 20, 72, 73, 100, 1_024] {
            let (t, counts) = h.target(&ada, Some(&stored), len);
            assert_eq!(t.cost(), Some(4), "{len} bytes");
            assert_eq!(counts, len <= Hasher::MAX_BYTES, "{len} bytes");
            let (none, _) = h.target(&key("kestrel"), None, len);
            assert_eq!(none.cost(), Some(h.dummy_cost(&key("kestrel"))));
        }
        let (bad, counts) = h.target(&ada, Some("not a hash"), 20);
        assert_eq!((bad, counts), (Target::Dummy(5), false));
        // And the long answer to the real hash never matches, even when
        // its first 72 bytes are the password.
        let real = "b".repeat(72);
        let at4 = Hasher::for_tests(4);
        let hash = at4.hash(&Secret::from(real.as_str())).await.unwrap();
        let longer = Secret::from(format!("{real}c").as_str());
        assert!(!at4.verify(&ada, &longer, Some(&hash)).await.unwrap());
    }

    #[test]
    fn dummy_costs_follow_the_stored_costs() {
        let none = Hasher::for_tests(6);
        assert_eq!(none.dummy_cost(&key("ada")), 6);
        let h = Hasher::new(5, [7; 32], BTreeMap::from([(4, 3), (5, 1)]), 1);
        let at4 = (0..2_000)
            .filter(|i| h.dummy_cost(&key(&format!("n{i}"))) == 4)
            .count();
        #[expect(clippy::cast_precision_loss, reason = "a share of 2 000")]
        let share = at4 as f64 / 2_000.0;
        assert!((share - 0.75).abs() <= 0.04, "{share}");
        assert_eq!(h.dummy_cost(&key("ada")), h.dummy_cost(&key("ada")));
        h.note_cost(Some(4), 5);
        h.note_cost(Some(4), 5);
        h.note_cost(Some(4), 5);
        assert_eq!(h.dummy_cost(&key("ada")), 5, "only cost 5 is left");
        assert_eq!(hex(&[0, 15, 255]), "000fff");
    }
}
