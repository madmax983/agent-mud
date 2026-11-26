//! The account service: the one handle the server holds.
//!
//! It never holds a `Game`: callers pass who holds a name ([`Holder`],
//! from `Game::name_owner`) and the time in. It never touches the world;
//! the server binds a session with `Game::login(name)` only after this
//! says the login is proved.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use autumn_web::auth::{BreachCheck, PasswordPolicy};
use mud_world::EntityId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::device::{self, IssuedToken, Series};
use crate::error::{AccountError, AuthError};
use crate::hasher::{Hasher, cost_of, hex, random};
use crate::name::NameKey;
use crate::peer::Peer;
use crate::policy::{PasswordRejection, check_new_password};
use crate::route::{Holder, Route, login_route, may_bind};
use crate::secret::Secret;
use crate::store::{self, AccountRow, AccountStore, NewAccount, NewDevice, Origin};
use crate::throttle::{Attempt, CreateRefusal, CreationSlot, Held, ThrottlePolicies, Throttles};

pub use crate::store::AccountSummary;

/// The four throttle policies.
pub type ThrottleConfig = ThrottlePolicies;

const DAY_MS: u64 = 86_400_000;

/// What happens to characters saved before accounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Legacy {
    /// The first confirmed password claims the character, within the
    /// window.
    #[default]
    Claim,
    /// Closed until an operator resets them.
    Sealed,
}

impl Legacy {
    /// The config word.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claim => "claim",
            Self::Sealed => "sealed",
        }
    }
}

/// How accounts behave (`[accounts]` in `mud.toml`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountsConfig {
    /// bcrypt cost, 4 to 16.
    pub bcrypt_cost: u32,
    /// The shortest new password, in characters.
    pub min_password_chars: usize,
    /// Refuse autumn-web's 10k weak passwords.
    pub reject_common_passwords: bool,
    /// Whether new characters may be made.
    pub allow_new: bool,
    /// New characters per creation bucket per UTC day; 0: no limit.
    pub new_characters_per_address_per_day: u32,
    /// New characters per UTC day in all; 0: no limit.
    pub new_characters_per_day: u32,
    /// Characters saved before accounts.
    pub legacy: Legacy,
    /// Days a claim window stays open; 0: never closes.
    pub legacy_claim_days: u32,
    /// "Stay signed in" days; 0 turns it off.
    pub remember_days: u32,
    /// A tab token's life after its last activity, in minutes; 0: off.
    pub reconnect_minutes: u32,
    /// Remembered browsers per character.
    pub max_remembered: u32,
    /// Recent login addresses per character that skip the shared limits.
    pub known_addresses: u32,
    /// Wrong passwords per connection before it closes.
    pub max_password_attempts: u32,
    /// bcrypt jobs at once.
    pub hash_concurrency: u32,
    /// How often live sessions re-check their account, in seconds.
    pub revalidate_secs: u32,
    /// The throttles.
    pub throttle: ThrottleConfig,
}

impl Default for AccountsConfig {
    fn default() -> Self {
        Self {
            bcrypt_cost: 12,
            min_password_chars: 8,
            reject_common_passwords: true,
            allow_new: true,
            new_characters_per_address_per_day: 5,
            new_characters_per_day: 0,
            legacy: Legacy::Claim,
            legacy_claim_days: 30,
            remember_days: 30,
            reconnect_minutes: 15,
            max_remembered: 10,
            known_addresses: 5,
            max_password_attempts: 3,
            hash_concurrency: 2,
            revalidate_secs: 30,
            throttle: ThrottleConfig::default(),
        }
    }
}

/// How a session was proved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthVia {
    /// The password.
    Password,
    /// A new character's first password.
    Created,
    /// A legacy character's first password.
    Claimed,
    /// A new password after an operator's reset.
    Reset,
    /// A web resume token.
    Resumed,
}

/// A proved login. Only this crate makes one, and only for a name a
/// session may bind to.
#[derive(Debug, Clone)]
pub struct Authenticated {
    key: NameKey,
    name: String,
    entity: EntityId,
    via: AuthVia,
    since_ms: u64,
    series: Option<Series>,
}

impl Authenticated {
    /// The account key.
    #[must_use]
    pub const fn key(&self) -> &NameKey {
        &self.key
    }

    /// The character's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The character's entity.
    #[must_use]
    pub const fn entity(&self) -> &EntityId {
        &self.entity
    }

    /// How it was proved.
    #[must_use]
    pub const fn via(&self) -> AuthVia {
        self.via
    }

    /// When the session began, or last changed its own password.
    #[must_use]
    pub const fn since_ms(&self) -> u64 {
        self.since_ms
    }

    /// The remembered browser it holds, if any.
    #[must_use]
    pub const fn series(&self) -> Option<&Series> {
        self.series.as_ref()
    }
}

/// A right password whose account must choose a new one first. Single
/// use, and void once the account is reset again, locked, or its password
/// changes.
pub struct ResetTicket {
    key: NameKey,
    name: String,
    holder: Holder,
    /// The stored hash the right password matched; a new password is set
    /// only over this one.
    hash: String,
}

impl std::fmt::Debug for ResetTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResetTicket")
            .field("key", &self.key)
            .field("name", &self.name)
            .field("holder", &self.holder)
            .finish_non_exhaustive()
    }
}

impl ResetTicket {
    /// The character's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// What a right password opens.
#[derive(Debug)]
pub enum SignIn {
    /// A session.
    Ok(Authenticated),
    /// The new-password prompt first.
    MustChange(ResetTicket),
}

/// Why a password is being made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewPurpose {
    /// A new character.
    Create,
    /// A legacy character's first password.
    Claim,
}

#[derive(Debug, Default)]
struct AccountStats {
    ok: AtomicU64,
    failed: AtomicU64,
    held: AtomicU64,
    created: AtomicU64,
    resumed: AtomicU64,
    resume_rejected: AtomicU64,
}

const fn create_refusal(r: CreateRefusal) -> AuthError {
    match r {
        CreateRefusal::PerAddress => AuthError::NewPerAddress,
        CreateRefusal::PerDay => AuthError::NewPerDay,
    }
}

fn bump(n: &AtomicU64) {
    n.fetch_add(1, Ordering::Relaxed);
}

/// Login counts since start, for the observer. No names, no addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct AccountStatsView {
    /// Good logins.
    pub ok: u64,
    /// Wrong passwords.
    pub failed: u64,
    /// Attempts a throttle held.
    pub held: u64,
    /// New characters.
    pub created: u64,
    /// Resumed browsers.
    pub resumed: u64,
    /// Refused resume tokens.
    pub resume_rejected: u64,
}

/// The account service.
#[derive(Debug)]
pub struct Accounts {
    store: AccountStore,
    hasher: Hasher,
    throttles: Throttles,
    cfg: AccountsConfig,
    policy: PasswordPolicy,
    legacy_opened_ms: u64,
    bucket_salt: [u8; 32],
    stats: AccountStats,
    created_new: bool,
}

fn parse_salt(hex_text: &str) -> Option<[u8; 32]> {
    let b = hex_text.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in b.chunks(2).enumerate() {
        let s = std::str::from_utf8(pair).ok()?;
        out[i] = u8::from_str_radix(s, 16).ok()?;
    }
    Some(out)
}

/// The temporary password alphabet: no `l`, `o`, `0` or `1`.
const TEMP_ALPHABET: &[u8; 32] = b"abcdefghijkmnpqrstuvwxyz23456789";

/// 16 symbols of 5 bits each from the OS CSPRNG: 80 bits.
fn temporary_password() -> Result<Secret, AccountError> {
    let mut raw = zeroize::Zeroizing::new([0u8; 16]);
    random(raw.as_mut())?;
    Ok(Secret::new(
        raw.iter()
            .map(|b| char::from(TEMP_ALPHABET[usize::from(b & 31)]))
            .collect(),
    ))
}

impl Accounts {
    /// Opens the service over `accounts.db` at `path` (in memory if
    /// `None`), first opened at `now_ms` for the legacy window.
    ///
    /// # Errors
    /// The store cannot be opened, or the OS has no randomness.
    pub async fn open(
        path: Option<&Path>,
        cfg: AccountsConfig,
        busy_timeout_ms: u32,
        now_ms: u64,
    ) -> Result<Self, AccountError> {
        let (store, created_new) = match path {
            Some(p) => {
                let p = p.to_path_buf();
                tokio::task::spawn_blocking(move || AccountStore::open(&p, busy_timeout_ms))
                    .await??
            }
            None => (AccountStore::open_in_memory()?, false),
        };
        let mut fresh_salt = [0u8; 32];
        random(&mut fresh_salt)?;
        let fresh_salt = hex(&fresh_salt);
        let (opened, salt, mix) = store
            .call(move |c| {
                let opened = store::meta_init(c, "legacy_opened_ms", &now_ms.to_string())?;
                let salt = store::meta_init(c, "bucket_salt", &fresh_salt)?;
                store::sweep(c, now_ms)?;
                Ok((opened, salt, store::cost_mix(c)?))
            })
            .await?;
        let bucket_salt = parse_salt(&salt).ok_or_else(|| {
            AccountError::Random("accounts.db holds a malformed bucket_salt".to_owned())
        })?;
        let hasher = Hasher::new(
            cfg.bcrypt_cost,
            bucket_salt,
            mix,
            usize::try_from(cfg.hash_concurrency).unwrap_or(1),
        );
        hasher.prepare().await?;
        let throttles = Throttles::new(
            cfg.throttle,
            cfg.new_characters_per_address_per_day,
            cfg.new_characters_per_day,
        );
        let policy = PasswordPolicy::new(
            cfg.min_password_chars,
            cfg.reject_common_passwords,
            BreachCheck::Off,
        );
        Ok(Self {
            store,
            hasher,
            throttles,
            policy,
            legacy_opened_ms: opened.parse().unwrap_or(now_ms),
            bucket_salt,
            stats: AccountStats::default(),
            created_new,
            cfg,
        })
    }

    /// The config.
    #[must_use]
    pub const fn config(&self) -> &AccountsConfig {
        &self.cfg
    }

    /// Whether `accounts.db` was made by this open.
    #[must_use]
    pub const fn created_new(&self) -> bool {
        self.created_new
    }

    /// When legacy characters became claimable.
    #[must_use]
    pub const fn legacy_opened_ms(&self) -> u64 {
        self.legacy_opened_ms
    }

    /// When the claim window closes, if it does.
    #[must_use]
    pub const fn legacy_closes_ms(&self) -> Option<u64> {
        if self.cfg.legacy_claim_days == 0 {
            None
        } else {
            Some(
                self.legacy_opened_ms
                    .saturating_add(self.cfg.legacy_claim_days as u64 * DAY_MS),
            )
        }
    }

    /// Whether a legacy character may be claimed at `now_ms`.
    #[must_use]
    pub const fn legacy_open(&self, now_ms: u64) -> bool {
        matches!(self.cfg.legacy, Legacy::Claim)
            && match self.legacy_closes_ms() {
                None => true,
                Some(t) => now_ms < t,
            }
    }

    fn bucket_hash(&self, peer: Peer) -> Option<String> {
        let Peer::Addr(ip) = peer else {
            return None;
        };
        let mut h = Sha256::new();
        h.update(self.bucket_salt);
        h.update(ip.to_string().as_bytes());
        Some(hex(&h.finalize()))
    }

    fn held(&self, h: Held) -> AuthError {
        bump(&self.stats.held);
        match h {
            Held::Name { wait_ms } => AuthError::NameHeld { wait_ms },
            Held::Address { wait_ms } => AuthError::AddressHeld { wait_ms },
            Held::Work { wait_ms } => AuthError::WorkHeld { wait_ms },
            Held::Busy => AuthError::Busy,
        }
    }

    async fn row_and_known(
        &self,
        key: &NameKey,
        peer: Peer,
    ) -> Result<(Option<AccountRow>, bool), AccountError> {
        let (k, bh) = (key.clone(), self.bucket_hash(peer));
        self.store
            .call(move |c| {
                let row = store::get(c, &k)?;
                let known = row.is_some() && store::is_known(c, &k, bh.as_deref())?;
                Ok((row, known))
            })
            .await
    }

    async fn row(&self, key: &NameKey) -> Result<Option<AccountRow>, AccountError> {
        let k = key.clone();
        self.store.call(move |c| store::get(c, &k)).await
    }

    /// The route of a login.
    ///
    /// # Errors
    /// The store failed.
    pub async fn route(
        &self,
        key: &NameKey,
        holder: Holder,
        now_ms: u64,
    ) -> Result<Route, AccountError> {
        let has = self.row(key).await?.is_some();
        Ok(login_route(holder, has, self.legacy_open(now_ms)))
    }

    /// Checks the throttles for a name before anything else happens.
    ///
    /// # Errors
    /// A hold, or the store failed.
    pub async fn check_name(
        &self,
        key: &NameKey,
        peer: Peer,
        now_ms: u64,
    ) -> Result<(), AuthError> {
        let (_, known) = self.row_and_known(key, peer).await?;
        self.throttles
            .check(key, peer, known, now_ms)
            .map_err(|h| self.held(h))
    }

    /// Takes a new character's place in today's counts.
    fn reserve_creation(&self, peer: Peer, now_ms: u64) -> Result<CreationSlot<'_>, AuthError> {
        if !self.cfg.allow_new {
            return Err(AuthError::ClosedToNew);
        }
        self.throttles
            .reserve_creation(peer, now_ms)
            .map_err(create_refusal)
    }

    /// Whether `peer` may make a new character now.
    ///
    /// # Errors
    /// New characters are off, or a cap applies.
    pub fn may_create(&self, peer: Peer, now_ms: u64) -> Result<(), AuthError> {
        if !self.cfg.allow_new {
            return Err(AuthError::ClosedToNew);
        }
        self.throttles
            .may_create(peer, now_ms)
            .map_err(create_refusal)
    }

    /// The checks before a hash: the throttles, the one check in flight
    /// per (name, address) and the address's work, gated and charged at
    /// once. Returns the row and the admitted attempt.
    async fn admit(
        &self,
        key: &NameKey,
        peer: Peer,
        now_ms: u64,
    ) -> Result<(Option<AccountRow>, Attempt<'_>), AuthError> {
        let (row, known) = self.row_and_known(key, peer).await?;
        let attempt = self
            .throttles
            .admit(key, peer, known, now_ms)
            .map_err(|h| self.held(h))?;
        Ok((row, attempt))
    }

    /// Steps 1 to 7 of a sign-in: the row of a right, unlocked password,
    /// and its attempt, still to be settled.
    async fn check_password(
        &self,
        key: &NameKey,
        pw: &Secret,
        peer: Peer,
        now_ms: u64,
    ) -> Result<(AccountRow, Attempt<'_>), AuthError> {
        let (row, attempt) = self.admit(key, peer, now_ms).await?;
        let ok = self
            .hasher
            .verify(key, pw, row.as_ref().map(|r| r.password_hash.as_str()))
            .await?;
        let row = match row {
            Some(r) if ok => r,
            _ => return Err(self.wrong(key, attempt)),
        };
        if row.locked {
            return Err(AuthError::Locked);
        }
        Ok((row, attempt))
    }

    /// Settles a refused password.
    fn wrong(&self, key: &NameKey, attempt: Attempt<'_>) -> AuthError {
        attempt.failed();
        bump(&self.stats.failed);
        tracing::info!(target: "mud::auth", name = %key, "a password was refused");
        AuthError::WrongPassword
    }

    fn mint(
        key: &NameKey,
        name: &str,
        holder: Holder,
        via: AuthVia,
        now_ms: u64,
    ) -> Result<Authenticated, AuthError> {
        if !may_bind(holder, true) {
            tracing::warn!(target: "mud::auth", name = %key, "a proved login names a character no session may bind to; refused");
            return Err(AuthError::WrongPassword);
        }
        Ok(Authenticated {
            key: key.clone(),
            name: name.to_owned(),
            entity: key.entity(),
            via,
            since_ms: now_ms,
            series: None,
        })
    }

    /// Records a good login: throttle, time, address, a re-hash if due.
    async fn record_login(
        &self,
        key: &NameKey,
        pw: Option<&Secret>,
        stored_hash: Option<&str>,
        peer: Peer,
        now_ms: u64,
    ) -> Result<(), AuthError> {
        let rehash = match (pw, stored_hash) {
            (Some(pw), Some(h)) if self.hasher.needs_rehash(h) => {
                Some((cost_of(h), self.hasher.hash(pw).await?))
            }
            _ => None,
        };
        let (k, bh, cap) = (
            key.clone(),
            self.bucket_hash(peer),
            self.cfg.known_addresses,
        );
        let new_hash = rehash.as_ref().map(|(_, h)| h.clone());
        self.store
            .call(move |c| {
                store::record_login(c, &k, now_ms, bh.as_deref(), cap, new_hash.as_deref())
            })
            .await?;
        if let Some((old, _)) = rehash {
            self.hasher.note_cost(old, self.hasher.cost());
        }
        bump(&self.stats.ok);
        Ok(())
    }

    /// Checks a password at the `Password:` prompt. Every answer that
    /// passes the throttles costs exactly one bcrypt verify, whoever holds
    /// the name.
    ///
    /// # Errors
    /// A wrong password (or a name no session may bind to), a hold, a
    /// lock, or the service failed.
    pub async fn sign_in(
        &self,
        key: &NameKey,
        holder: Holder,
        pw: &Secret,
        peer: Peer,
        now_ms: u64,
    ) -> Result<SignIn, AuthError> {
        let (row, attempt) = self.check_password(key, pw, peer, now_ms).await?;
        if !may_bind(holder, true) {
            tracing::warn!(target: "mud::auth", name = %key, "an account's password matched, but the name is not a human's; refused");
            return Err(self.wrong(key, attempt));
        }
        attempt.succeeded();
        self.record_login(key, Some(pw), Some(&row.password_hash), peer, now_ms)
            .await?;
        if row.must_change {
            return Ok(SignIn::MustChange(ResetTicket {
                key: key.clone(),
                name: row.name,
                holder,
                hash: row.password_hash,
            }));
        }
        Self::mint(key, &row.name, holder, AuthVia::Password, now_ms).map(SignIn::Ok)
    }

    /// Answers the claim route's `Password:` prompt: one dummy verify (so
    /// it costs what a sign-in costs), then the policy.
    ///
    /// # Errors
    /// A hold, or the service failed.
    pub async fn answer_claim(
        &self,
        key: &NameKey,
        name: &str,
        pw: &Secret,
        peer: Peer,
        now_ms: u64,
    ) -> Result<Result<(), PasswordRejection>, AuthError> {
        let (_row, attempt) = self.admit(key, peer, now_ms).await?;
        let _ = self.hasher.verify(key, pw, None).await?;
        // A claim answer is not a guess: only its work stays charged.
        attempt.refund();
        Ok(self.check_new_password(pw, name).await)
    }

    /// Checks a new password against the policy.
    ///
    /// # Errors
    /// The rule it breaks.
    pub async fn check_new_password(
        &self,
        pw: &Secret,
        name: &str,
    ) -> Result<(), PasswordRejection> {
        check_new_password(pw, name, &self.policy).await
    }

    /// Makes a new character's account, or claims a legacy character.
    ///
    /// # Errors
    /// The name changed hands, a cap or hold applies, the password breaks
    /// a rule, or the service failed.
    #[expect(clippy::too_many_arguments, reason = "the spec's signature")]
    pub async fn create(
        &self,
        name: &str,
        key: &NameKey,
        holder: Holder,
        pw: &Secret,
        purpose: NewPurpose,
        peer: Peer,
        now_ms: u64,
    ) -> Result<Authenticated, AuthError> {
        let want = match purpose {
            NewPurpose::Create => Route::Create,
            NewPurpose::Claim => Route::Claim,
        };
        if self.route(key, holder, now_ms).await? != want {
            return Err(AuthError::NameJustTaken);
        }
        let slot = match purpose {
            NewPurpose::Create => Some(self.reserve_creation(peer, now_ms)?),
            NewPurpose::Claim => None,
        };
        self.check_new_password(pw, name)
            .await
            .map_err(AuthError::Weak)?;
        self.throttles
            .spend(peer, false, now_ms)
            .map_err(|h| self.held(h))?;
        let hash = self.hasher.hash(pw).await?;
        let origin = match purpose {
            NewPurpose::Create => Origin::Created,
            NewPurpose::Claim => Origin::Claimed,
        };
        let (k, n, bh, cap) = (
            key.clone(),
            name.trim().to_owned(),
            self.bucket_hash(peer),
            self.cfg.known_addresses,
        );
        let inserted = self
            .store
            .call(move |c| {
                let a = NewAccount {
                    key: &k,
                    name: &n,
                    hash: &hash,
                    origin,
                    now: now_ms,
                    must_change: false,
                };
                store::insert(c, &a, bh.as_deref(), cap)
            })
            .await?;
        if !inserted {
            return Err(AuthError::NameJustTaken);
        }
        self.hasher.note_cost(None, self.hasher.cost());
        bump(&self.stats.ok);
        let via = match purpose {
            NewPurpose::Create => {
                bump(&self.stats.created);
                let n = slot.map_or(0, CreationSlot::commit);
                if n.is_multiple_of(50) {
                    tracing::warn!(target: "mud::auth", "{n} new characters today");
                }
                AuthVia::Created
            }
            NewPurpose::Claim => {
                tracing::info!(target: "mud::auth", name = %key, "a legacy character was claimed");
                AuthVia::Claimed
            }
        };
        Self::mint(key, name.trim(), holder, via, now_ms)
    }

    /// Sets the new password a reset demanded, if the account is still
    /// the one the ticket was issued for: unlocked, still due a change,
    /// and still on the password the ticket proved.
    ///
    /// # Errors
    /// [`AuthError::Locked`] or [`AuthError::ResetStale`] when the account
    /// changed since the ticket; the password breaks a rule, a hold
    /// applies, or the service failed.
    pub async fn finish_reset(
        &self,
        ticket: ResetTicket,
        pw: &Secret,
        peer: Peer,
        now_ms: u64,
    ) -> Result<Authenticated, AuthError> {
        self.check_new_password(pw, &ticket.name)
            .await
            .map_err(AuthError::Weak)?;
        self.throttles
            .spend(peer, false, now_ms)
            .map_err(|h| self.held(h))?;
        let hash = self.hasher.hash(pw).await?;
        let (k, proved) = (ticket.key.clone(), ticket.hash.clone());
        let (written, locked) = self
            .store
            .call(move |c| {
                let n = store::finish_reset(c, &k, &proved, &hash, now_ms)?;
                let locked = n == 0 && store::get(c, &k)?.is_some_and(|r| r.locked);
                Ok((n > 0, locked))
            })
            .await?;
        if !written {
            tracing::info!(target: "mud::auth", name = %ticket.key, "a reset ticket outlived its account's state; refused");
            return Err(if locked {
                AuthError::Locked
            } else {
                AuthError::ResetStale
            });
        }
        self.hasher
            .note_cost(cost_of(&ticket.hash), self.hasher.cost());
        Self::mint(
            &ticket.key,
            &ticket.name,
            ticket.holder,
            AuthVia::Reset,
            now_ms,
        )
    }

    /// Changes a live session's password. Every other session of the
    /// account, and every other remembered browser, is signed out.
    ///
    /// # Errors
    /// The current password is wrong, the new one breaks a rule, a hold
    /// applies, or the service failed.
    pub async fn change_password(
        &self,
        auth: &mut Authenticated,
        current: &Secret,
        new: &Secret,
        peer: Peer,
        now_ms: u64,
    ) -> Result<(), AuthError> {
        let (row, attempt) = self
            .check_password(&auth.key, current, peer, now_ms)
            .await?;
        attempt.succeeded();
        self.check_new_password(new, &auth.name)
            .await
            .map_err(AuthError::Weak)?;
        self.throttles
            .spend(peer, false, now_ms)
            .map_err(|h| self.held(h))?;
        let hash = self.hasher.hash(new).await?;
        let (k, keep) = (auth.key.clone(), auth.series.clone());
        self.store
            .call(move |c| {
                store::set_password(c, &k, &hash, now_ms, false)?;
                store::delete_devices(c, &k, keep.as_ref())?;
                Ok(())
            })
            .await?;
        self.hasher
            .note_cost(cost_of(&row.password_hash), self.hasher.cost());
        auth.since_ms = now_ms;
        tracing::info!(target: "mud::auth", name = %auth.key, "a password was changed");
        Ok(())
    }

    /// A token's life: "stay signed in" or a tab token. `None`: both off.
    const fn ttl(&self, persist: bool) -> Option<(u64, bool)> {
        let days = self.cfg.remember_days as u64 * DAY_MS;
        let tab = self.cfg.reconnect_minutes as u64 * 60_000;
        if persist && days > 0 {
            Some((days, true))
        } else if tab > 0 {
            Some((tab, false))
        } else {
            None
        }
    }

    /// Issues a resume token for a web session.
    ///
    /// # Errors
    /// The store failed.
    pub async fn remember(
        &self,
        auth: &mut Authenticated,
        persist: bool,
        now_ms: u64,
    ) -> Result<Option<IssuedToken>, AccountError> {
        let Some((ttl, persist)) = self.ttl(persist) else {
            return Ok(None);
        };
        let f = device::fresh();
        let (k, series, hash, cap) = (
            auth.key.clone(),
            f.series.clone(),
            f.token_hash,
            self.cfg.max_remembered,
        );
        self.store
            .call(move |c| {
                let d = NewDevice {
                    key: &k,
                    series: &series,
                    token_hash: &hash,
                    persist,
                    now: now_ms,
                    expires_ms: now_ms.saturating_add(ttl),
                };
                store::insert_device(c, &d, cap)
            })
            .await?;
        auth.series = Some(f.series.clone());
        Ok(Some(IssuedToken {
            value: f.value,
            series: f.series,
            persist,
        }))
    }

    /// Resumes a web session from a token. Never throttled: a guess needs
    /// a 128-bit series and a 128-bit token, and costs one primary-key
    /// read; the connection limiter bounds the rate.
    ///
    /// The caller still checks `may_bind` against who holds the name now.
    ///
    /// # Errors
    /// [`AuthError::ResumeRejected`] for a malformed, unknown, expired or
    /// wrong token; [`AuthError::Locked`]; or the service failed.
    pub async fn resume(
        &self,
        credential: &Secret,
        peer: Peer,
        now_ms: u64,
    ) -> Result<Authenticated, AuthError> {
        let Some((series, token)) = device::parse(credential) else {
            bump(&self.stats.resume_rejected);
            return Err(AuthError::ResumeRejected);
        };
        let s = series.clone();
        let found = self
            .store
            .call(move |c| {
                let Some(d) = store::device(c, &s)? else {
                    return Ok(None);
                };
                Ok(Some((store::get(c, &d.key)?, d)))
            })
            .await?;
        let Some((row, dev)) = found else {
            bump(&self.stats.resume_rejected);
            return Err(AuthError::ResumeRejected);
        };
        if dev.expires_ms <= now_ms {
            bump(&self.stats.resume_rejected);
            return Err(AuthError::ResumeRejected);
        }
        if !device::matches(&token, &dev.token_hash) {
            bump(&self.stats.resume_rejected);
            let s = series.clone();
            self.store
                .call(move |c| store::delete_device(c, &s))
                .await?;
            tracing::warn!(target: "mud::auth", name = %dev.key, "a resume token did not match its series; the device is signed out");
            return Err(AuthError::ResumeRejected);
        }
        let Some(row) = row else {
            bump(&self.stats.resume_rejected);
            return Err(AuthError::ResumeRejected);
        };
        if row.locked {
            return Err(AuthError::Locked);
        }
        let ttl = self.ttl(dev.persist).map_or(0, |(t, _)| t);
        let (k, s, bh, cap) = (
            row.key.clone(),
            series.clone(),
            self.bucket_hash(peer),
            self.cfg.known_addresses,
        );
        self.store
            .call(move |c| {
                store::record_login(c, &k, now_ms, bh.as_deref(), cap, None)?;
                store::slide_device(c, &s, now_ms, now_ms.saturating_add(ttl))
            })
            .await?;
        bump(&self.stats.resumed);
        Ok(Authenticated {
            key: row.key,
            entity: NameKey::entity(&dev.key),
            name: row.name,
            via: AuthVia::Resumed,
            since_ms: now_ms,
            series: Some(series),
        })
    }

    /// Forgets one remembered browser.
    ///
    /// # Errors
    /// The store failed.
    pub async fn forget(&self, series: &Series) -> Result<(), AccountError> {
        let s = series.clone();
        self.store.call(move |c| store::delete_device(c, &s)).await
    }

    /// Whether a live session may go on: its account exists, is not
    /// locked, and was not revoked after the session began. Expiry and the
    /// device row do not matter. A live tab keeps its token fresh: past
    /// half its life, this slides it.
    ///
    /// # Errors
    /// The store failed.
    pub async fn revalidate(
        &self,
        auth: &Authenticated,
        now_ms: u64,
    ) -> Result<bool, AccountError> {
        let (k, series, since) = (auth.key.clone(), auth.series.clone(), auth.since_ms);
        let ttls = (self.ttl(true), self.ttl(false));
        self.store
            .call(move |c| {
                let Some(row) = store::get(c, &k)? else {
                    return Ok(false);
                };
                let live = !row.locked && row.sessions_revoked_ms <= since;
                if live
                    && let Some(s) = series
                    && let Some(d) = store::device(c, &s)?
                {
                    let ttl = if d.persist { ttls.0 } else { ttls.1 }.map_or(0, |(t, _)| t);
                    if d.expires_ms > now_ms && d.expires_ms - now_ms < ttl / 2 {
                        store::slide_device(c, &s, now_ms, now_ms.saturating_add(ttl))?;
                    }
                }
                Ok(live)
            })
            .await
    }

    /// Closes `accounts.db` at shutdown ([`AccountStore::close`]): its
    /// write-ahead log goes into the main file. Later calls fail with
    /// [`AccountError::Closed`].
    ///
    /// # Errors
    /// The checkpoint or the close failed.
    pub async fn close(&self) -> Result<(), AccountError> {
        self.store.close().await
    }

    /// Login counts since start.
    #[must_use]
    pub fn stats(&self) -> AccountStatsView {
        let g = |n: &AtomicU64| n.load(Ordering::Relaxed);
        AccountStatsView {
            ok: g(&self.stats.ok),
            failed: g(&self.stats.failed),
            held: g(&self.stats.held),
            created: g(&self.stats.created),
            resumed: g(&self.stats.resumed),
            resume_rejected: g(&self.stats.resume_rejected),
        }
    }

    /// bcrypt verifies run so far.
    #[must_use]
    pub fn verifications(&self) -> u64 {
        self.hasher.verifications()
    }

    /// `mud account reset`: a temporary password, returned once, that must
    /// be changed at the next login. Every session and browser is signed
    /// out.
    ///
    /// # Errors
    /// [`AccountError::NotHuman`] unless a human holds the name or it has
    /// an account; or the store failed.
    pub async fn admin_reset(
        &self,
        name: &str,
        key: &NameKey,
        holder: Holder,
        now_ms: u64,
    ) -> Result<Secret, AccountError> {
        let row = self.row(key).await?;
        if holder != Holder::Human && row.is_none() {
            return Err(AccountError::NotHuman(name.trim().to_owned()));
        }
        let pw = temporary_password()?;
        let hash = self.hasher.hash(&pw).await?;
        let (k, n) = (key.clone(), name.trim().to_owned());
        let old = row.as_ref().and_then(|r| cost_of(&r.password_hash));
        self.store
            .call(move |c| {
                if store::get(c, &k)?.is_some() {
                    store::set_password(c, &k, &hash, now_ms, true)?;
                } else {
                    let a = NewAccount {
                        key: &k,
                        name: &n,
                        hash: &hash,
                        origin: Origin::Reset,
                        now: now_ms,
                        must_change: true,
                    };
                    store::insert(c, &a, None, 0)?;
                }
                store::delete_devices(c, &k, None)?;
                Ok(())
            })
            .await?;
        self.hasher.note_cost(old, self.hasher.cost());
        tracing::info!(target: "mud::auth", name = %key, "an operator reset a password");
        Ok(pw)
    }

    /// `mud account lock` / `unlock`. A lock ends live sessions at their
    /// next revalidation and forgets every browser.
    ///
    /// # Errors
    /// No such account, or the store failed.
    pub async fn admin_set_locked(
        &self,
        key: &NameKey,
        locked: bool,
        now_ms: u64,
    ) -> Result<(), AccountError> {
        let k = key.clone();
        let n = self
            .store
            .call(move |c| {
                let n = store::set_locked(c, &k, locked, now_ms)?;
                if locked && n > 0 {
                    store::delete_devices(c, &k, None)?;
                }
                Ok(n)
            })
            .await?;
        if n == 0 {
            return Err(AccountError::NoAccount(key.to_string()));
        }
        Ok(())
    }

    /// `mud account signout`: forgets every browser and ends live
    /// sessions at their next revalidation. Returns the browsers forgotten.
    ///
    /// # Errors
    /// No such account, or the store failed.
    pub async fn admin_signout(&self, key: &NameKey, now_ms: u64) -> Result<usize, AccountError> {
        let k = key.clone();
        let (n, gone) = self
            .store
            .call(move |c| {
                let n = store::revoke(c, &k, now_ms)?;
                Ok((n, store::delete_devices(c, &k, None)?))
            })
            .await?;
        if n == 0 {
            return Err(AccountError::NoAccount(key.to_string()));
        }
        Ok(gone)
    }

    /// Every account.
    ///
    /// # Errors
    /// The store failed.
    pub async fn admin_list(&self) -> Result<Vec<AccountSummary>, AccountError> {
        self.store.call(|c| store::list(c)).await
    }

    /// How many of `humans` have no account yet (legacy characters).
    ///
    /// # Errors
    /// The store failed.
    pub async fn legacy_count(&self, humans: &[EntityId]) -> Result<usize, AccountError> {
        let have = self.store.call(|c| store::entities(c)).await?;
        Ok(humans.iter().filter(|h| !have.contains(h.as_str())).count())
    }
}

#[cfg(test)]
mod tests;
