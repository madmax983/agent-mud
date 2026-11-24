//! Login throttles: exponential backoff per key, checked before any
//! hashing.
//!
//! The step [`throttle_step`] is proved in `formal/verus/src/throttle.rs`:
//! no overflow; a success clears a key; a failure never delays past the
//! cap (even from a larger stored delay); free failures never delay; past
//! free, a failure delays at least `min(base, max)`; a quiet spell of
//! `forget_ms` restarts the count. Its lemma `lemma_rate_bound` bounds the
//! failures one key admits in a window.
//!
//! [`Throttles`] keeps four tables of keys. The tight key, (name,
//! address), always applies. The loose keys (name, address, and bcrypt
//! work per address) are skipped for a (name, address) pair that has
//! logged in before, so an attacker elsewhere cannot hold the owner out at
//! an address the owner uses. Every key exists with or without an account:
//! an agent PC throttles exactly like a human account.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::net::IpAddr;
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::name::NameKey;
use crate::peer::Peer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// mirror-begin throttle_op
/// What happened at a key.
pub enum ThrottleOp {
    /// A wrong password, or a unit of work.
    Fail,
    /// A right password.
    Succeed,
}
// mirror-end throttle_op

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// mirror-begin throttle_policy
/// The backoff of one kind of key.
pub struct ThrottlePolicy {
    /// Failures that never delay.
    pub free: u32,
    /// The first delay, in ms.
    pub base_ms: u64,
    /// The longest delay, in ms.
    pub max_ms: u64,
    /// A quiet spell this long forgets every failure, in ms.
    pub forget_ms: u64,
}
// mirror-end throttle_policy

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
// mirror-begin throttle_state
/// The backoff state of one key.
pub struct Throttle {
    /// Failures since the last quiet spell or success.
    pub failures: u32,
    /// The delay the last failure set, in ms.
    pub delay_ms: u64,
    /// No check passes before this time.
    pub locked_until_ms: u64,
    /// When the last failure happened.
    pub last_failure_ms: u64,
}
// mirror-end throttle_state

// mirror-begin throttle_allows
/// Whether a key allows a check at `now_ms`.
#[must_use]
pub const fn throttle_allows(t: &Throttle, now_ms: u64) -> bool {
    now_ms >= t.locked_until_ms
}
// mirror-end throttle_allows

// mirror-begin throttle_step
/// One step of a key's backoff.
#[must_use]
pub const fn throttle_step(
    t: Throttle,
    op: ThrottleOp,
    now_ms: u64,
    p: &ThrottlePolicy,
) -> Throttle {
    match op {
        ThrottleOp::Succeed => Throttle {
            failures: 0,
            delay_ms: 0,
            locked_until_ms: 0,
            last_failure_ms: t.last_failure_ms,
        },
        ThrottleOp::Fail => {
            let stale = now_ms >= t.last_failure_ms && now_ms - t.last_failure_ms >= p.forget_ms;
            let failures = if stale {
                1
            } else {
                t.failures.saturating_add(1)
            };
            let first = if p.base_ms < p.max_ms {
                p.base_ms
            } else {
                p.max_ms
            };
            let delay_ms = if failures <= p.free {
                0
            } else if stale || t.delay_ms == 0 {
                first
            } else if t.delay_ms > p.max_ms / 2 {
                p.max_ms
            } else if t.delay_ms * 2 < first {
                first
            } else {
                t.delay_ms * 2
            };
            let locked_until_ms = now_ms.saturating_add(delay_ms);
            Throttle {
                failures,
                delay_ms,
                locked_until_ms,
                last_failure_ms: now_ms,
            }
        }
    }
}
// mirror-end throttle_step

/// The most keys one table holds.
pub const TABLE_CAP: usize = 65_536;

/// Why a check may not go on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Held {
    /// The name, from this address or from everywhere, is locked.
    Name {
        /// How long until it opens.
        wait_ms: u64,
    },
    /// This address is locked over names it never logged in as.
    Address {
        /// How long until it opens.
        wait_ms: u64,
    },
    /// This address has spent its bcrypt work.
    Work {
        /// How long until it opens.
        wait_ms: u64,
    },
    /// A table is full of live keys, or this (name, address) is already
    /// being checked.
    Busy,
}

/// Why a new character was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateRefusal {
    /// This address made its share today.
    PerAddress,
    /// The world made its share today.
    PerDay,
}

/// What one table says about a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    Open,
    Locked(u64),
    Full,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One kind of key and its backoff states.
#[derive(Debug)]
struct KeyTable<K> {
    map: Mutex<HashMap<K, Throttle>>,
    policy: ThrottlePolicy,
    cap: usize,
}

impl<K: Eq + Hash + Clone> KeyTable<K> {
    fn new(policy: ThrottlePolicy, cap: usize) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            policy,
            cap: cap.max(1),
        }
    }

    /// The evictable key with the oldest failure: one quiet for
    /// `forget_ms`, or one within its free count. A key past free inside
    /// `forget_ms` is never evicted, so the rate bound holds for it.
    fn victim(&self, map: &HashMap<K, Throttle>, now: u64) -> Option<K> {
        map.iter()
            .filter(|(_, t)| {
                (now >= t.last_failure_ms && now - t.last_failure_ms >= self.policy.forget_ms)
                    || t.failures <= self.policy.free
            })
            .min_by_key(|(_, t)| t.last_failure_ms)
            .map(|(k, _)| k.clone())
    }

    fn gate_in(&self, map: &HashMap<K, Throttle>, k: &K, now: u64) -> Gate {
        match map.get(k) {
            Some(t) if throttle_allows(t, now) => Gate::Open,
            Some(t) => Gate::Locked(t.locked_until_ms.saturating_sub(now)),
            None if map.len() < self.cap || self.victim(map, now).is_some() => Gate::Open,
            None => Gate::Full,
        }
    }

    fn gate(&self, k: &K, now: u64) -> Gate {
        self.gate_in(&lock(&self.map), k, now)
    }

    fn fail_in(&self, map: &mut HashMap<K, Throttle>, k: &K, now: u64) {
        let t = if let Some(t) = map.get(k) {
            *t
        } else {
            if map.len() >= self.cap {
                let Some(v) = self.victim(map, now) else {
                    // No free slot: this failure is not recorded.
                    return;
                };
                map.remove(&v);
            }
            Throttle::default()
        };
        map.insert(
            k.clone(),
            throttle_step(t, ThrottleOp::Fail, now, &self.policy),
        );
    }

    fn fail(&self, k: &K, now: u64) {
        self.fail_in(&mut lock(&self.map), k, now);
    }

    /// Checks `k` and charges it one failure, under one lock.
    fn charge(&self, k: &K, now: u64) -> Gate {
        let mut map = lock(&self.map);
        let g = self.gate_in(&map, k, now);
        if g == Gate::Open {
            self.fail_in(&mut map, k, now);
        }
        drop(map);
        g
    }

    /// Charges `k` one failure inside a held lock and returns what undoes
    /// it, if it was recorded.
    fn charge_in(&self, map: &mut HashMap<K, Throttle>, k: &K, now: u64) -> Option<Undo<K>> {
        let prev = map.get(k).copied();
        self.fail_in(map, k, now);
        map.get(k).copied().map(|post| Undo {
            key: k.clone(),
            prev,
            post,
        })
    }

    /// Takes back a charge if nothing stepped the key since: the key is
    /// then as if the charge never happened. Otherwise the charge stays,
    /// which only ever holds more.
    fn undo(&self, u: Undo<K>) {
        let mut map = lock(&self.map);
        if map.get(&u.key) == Some(&u.post) {
            match u.prev {
                Some(t) => {
                    map.insert(u.key, t);
                }
                None => {
                    map.remove(&u.key);
                }
            }
        }
    }

    fn succeed(&self, k: &K, now: u64) {
        if let Some(t) = lock(&self.map).get_mut(k) {
            *t = throttle_step(*t, ThrottleOp::Succeed, now, &self.policy);
        }
    }

    #[cfg(test)]
    fn get(&self, k: &K) -> Option<Throttle> {
        lock(&self.map).get(k).copied()
    }
}

/// A charge on one key, and the state it replaced.
#[derive(Debug)]
struct Undo<K> {
    key: K,
    prev: Option<Throttle>,
    post: Throttle,
}

/// The policies of the four keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThrottlePolicies {
    /// One name from one address.
    pub name_address: ThrottlePolicy,
    /// One name from addresses it never logged in from.
    pub name: ThrottlePolicy,
    /// One address over names it never logged in as.
    pub address: ThrottlePolicy,
    /// bcrypt checks from one address, pass or fail.
    pub work: ThrottlePolicy,
}

impl Default for ThrottlePolicies {
    fn default() -> Self {
        Self {
            name_address: ThrottlePolicy {
                free: 5,
                base_ms: 1_000,
                max_ms: 900_000,
                forget_ms: 86_400_000,
            },
            name: ThrottlePolicy {
                free: 10,
                base_ms: 1_000,
                max_ms: 300_000,
                forget_ms: 86_400_000,
            },
            address: ThrottlePolicy {
                free: 20,
                base_ms: 1_000,
                max_ms: 900_000,
                forget_ms: 3_600_000,
            },
            work: ThrottlePolicy {
                free: 60,
                base_ms: 1_000,
                max_ms: 60_000,
                forget_ms: 600_000,
            },
        }
    }
}

/// New characters made today, per creation bucket and in all.
#[derive(Debug, Default)]
struct Creations {
    day: u64,
    per: HashMap<IpAddr, u32>,
    total: u64,
}

const DAY_MS: u64 = 86_400_000;

impl Creations {
    fn roll(&mut self, now_ms: u64) {
        let day = now_ms / DAY_MS;
        if day != self.day {
            self.day = day;
            self.per.clear();
            self.total = 0;
        }
    }
}

/// Every login throttle. State is in memory and resets on restart.
#[derive(Debug)]
pub struct Throttles {
    name_address: KeyTable<(NameKey, Peer)>,
    name: KeyTable<NameKey>,
    address: KeyTable<IpAddr>,
    work: KeyTable<IpAddr>,
    created: Mutex<Creations>,
    per_address_per_day: u32,
    per_day: u64,
    in_flight: Mutex<HashSet<(NameKey, Peer)>>,
}

/// A password check admitted by [`Throttles::admit`].
///
/// Its keys were gated and charged one failure under one lock, so checks
/// that start together are counted one after another, as the proved rate
/// bound assumes. Settle it with [`Attempt::failed`], [`Attempt::succeeded`]
/// or [`Attempt::refund`]; dropped unsettled, the charges stay.
#[derive(Debug)]
pub struct Attempt<'a> {
    throttles: &'a Throttles,
    key: NameKey,
    peer: Peer,
    known: bool,
    now_ms: u64,
    name_address: Option<Undo<(NameKey, Peer)>>,
    name: Option<Undo<NameKey>>,
    address: Option<Undo<IpAddr>>,
    _flight: InFlight<'a>,
}

impl Attempt<'_> {
    /// A wrong password. Its charges stand; a known pair, whose loose
    /// keys were not gated, records its failure on them now.
    pub fn failed(self) {
        if self.known {
            self.throttles.name.fail(&self.key, self.now_ms);
            if let Peer::Addr(ip) = self.peer {
                self.throttles.address.fail(&ip, self.now_ms);
            }
        }
    }

    /// A right password: clears the (name, address) key and takes back
    /// the loose keys' charges, so a success is never a failure there.
    pub fn succeeded(self) {
        let t = self.throttles;
        t.name_address
            .succeed(&(self.key.clone(), self.peer), self.now_ms);
        if let Some(u) = self.name {
            t.name.undo(u);
        }
        if let Some(u) = self.address {
            t.address.undo(u);
        }
    }

    /// Not a password check after all (a claim's answer): takes back
    /// every charge but the work.
    pub fn refund(self) {
        let t = self.throttles;
        if let Some(u) = self.name_address {
            t.name_address.undo(u);
        }
        if let Some(u) = self.name {
            t.name.undo(u);
        }
        if let Some(u) = self.address {
            t.address.undo(u);
        }
    }
}

/// A new character's place in today's counts, taken before hashing.
/// Dropped without [`CreationSlot::commit`], it is given back.
#[derive(Debug)]
pub struct CreationSlot<'a> {
    throttles: &'a Throttles,
    bucket: Option<IpAddr>,
    day: u64,
    committed: bool,
}

impl CreationSlot<'_> {
    /// The character was made. Returns today's count across addresses.
    #[must_use]
    pub fn commit(mut self) -> u64 {
        self.committed = true;
        lock(&self.throttles.created).total
    }
}

impl Drop for CreationSlot<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut c = lock(&self.throttles.created);
        if c.day != self.day {
            return;
        }
        if let Some(b) = self.bucket
            && let Some(n) = c.per.get_mut(&b)
        {
            *n = n.saturating_sub(1);
        }
        c.total = c.total.saturating_sub(1);
    }
}

/// A (name, address) check in progress; dropping it ends the check.
#[derive(Debug)]
pub struct InFlight<'a> {
    throttles: &'a Throttles,
    key: (NameKey, Peer),
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        lock(&self.throttles.in_flight).remove(&self.key);
    }
}

impl Throttles {
    /// Throttles with `policies`, at most `per_address_per_day` new
    /// characters per creation bucket a day and `per_day` in all (`0`:
    /// no limit).
    #[must_use]
    pub fn new(policies: ThrottlePolicies, per_address_per_day: u32, per_day: u32) -> Self {
        Self::with_cap(policies, per_address_per_day, per_day, TABLE_CAP)
    }

    /// [`Throttles::new`] with tables of `cap` keys.
    #[must_use]
    pub fn with_cap(
        policies: ThrottlePolicies,
        per_address_per_day: u32,
        per_day: u32,
        cap: usize,
    ) -> Self {
        Self {
            name_address: KeyTable::new(policies.name_address, cap),
            name: KeyTable::new(policies.name, cap),
            address: KeyTable::new(policies.address, cap),
            work: KeyTable::new(policies.work, cap),
            created: Mutex::new(Creations::default()),
            per_address_per_day,
            per_day: u64::from(per_day),
            in_flight: Mutex::new(HashSet::new()),
        }
    }

    /// Checks a login before any hashing. `known`: this (name, address)
    /// logged in before.
    ///
    /// # Errors
    /// The hold that applies.
    pub fn check(&self, key: &NameKey, peer: Peer, known: bool, now_ms: u64) -> Result<(), Held> {
        match self.name_address.gate(&(key.clone(), peer), now_ms) {
            Gate::Locked(wait_ms) => return Err(Held::Name { wait_ms }),
            Gate::Full if !known => return Err(Held::Busy),
            Gate::Open | Gate::Full => {}
        }
        if known {
            return Ok(());
        }
        if let Peer::Addr(ip) = peer {
            match self.address.gate(&ip, now_ms) {
                Gate::Locked(wait_ms) => return Err(Held::Address { wait_ms }),
                Gate::Full => return Err(Held::Busy),
                Gate::Open => {}
            }
        }
        match self.name.gate(key, now_ms) {
            Gate::Locked(wait_ms) => Err(Held::Name { wait_ms }),
            Gate::Full => Err(Held::Busy),
            Gate::Open => Ok(()),
        }
    }

    /// Admits one password check: gates the (name, address), address,
    /// name and work keys as [`Throttles::check`] and
    /// [`Throttles::spend`] do, starts the one check of the pair allowed
    /// at a time, and charges every gated key one failure, all under the
    /// tables' locks, so no other check passes a key in between. `known`:
    /// this pair logged in before (the loose keys are skipped).
    ///
    /// # Errors
    /// The hold that applies; nothing is charged.
    pub fn admit(
        &self,
        key: &NameKey,
        peer: Peer,
        known: bool,
        now_ms: u64,
    ) -> Result<Attempt<'_>, Held> {
        // One lock order everywhere several are held: name_address,
        // address, name, work, then in_flight.
        let mut na = lock(&self.name_address.map);
        let mut addr = lock(&self.address.map);
        let mut name = lock(&self.name.map);
        let mut work = lock(&self.work.map);
        let pair = (key.clone(), peer);
        match self.name_address.gate_in(&na, &pair, now_ms) {
            Gate::Locked(wait_ms) => return Err(Held::Name { wait_ms }),
            Gate::Full if !known => return Err(Held::Busy),
            Gate::Open | Gate::Full => {}
        }
        let ip = match peer {
            Peer::Addr(ip) if !known => Some(ip),
            _ => None,
        };
        if !known {
            if let Some(ip) = ip {
                match self.address.gate_in(&addr, &ip, now_ms) {
                    Gate::Locked(wait_ms) => return Err(Held::Address { wait_ms }),
                    Gate::Full => return Err(Held::Busy),
                    Gate::Open => {}
                }
            }
            match self.name.gate_in(&name, key, now_ms) {
                Gate::Locked(wait_ms) => return Err(Held::Name { wait_ms }),
                Gate::Full => return Err(Held::Busy),
                Gate::Open => {}
            }
            if let Some(ip) = ip {
                match self.work.gate_in(&work, &ip, now_ms) {
                    Gate::Locked(wait_ms) => return Err(Held::Work { wait_ms }),
                    Gate::Full => return Err(Held::Busy),
                    Gate::Open => {}
                }
            }
        }
        let flight = self.begin(key, peer)?;
        let name_address = self.name_address.charge_in(&mut na, &pair, now_ms);
        let (address, name_undo) = if known {
            (None, None)
        } else {
            if let Some(ip) = ip {
                self.work.fail_in(&mut work, &ip, now_ms);
            }
            (
                ip.and_then(|ip| self.address.charge_in(&mut addr, &ip, now_ms)),
                self.name.charge_in(&mut name, key, now_ms),
            )
        };
        drop((na, addr, name, work));
        Ok(Attempt {
            throttles: self,
            key: key.clone(),
            peer,
            known,
            now_ms,
            name_address,
            name: name_undo,
            address,
            _flight: flight,
        })
    }

    /// Starts the one check of (name, address) allowed at a time.
    ///
    /// # Errors
    /// [`Held::Busy`] while another check of the same pair runs.
    pub fn begin(&self, key: &NameKey, peer: Peer) -> Result<InFlight<'_>, Held> {
        let pair = (key.clone(), peer);
        if !lock(&self.in_flight).insert(pair.clone()) {
            return Err(Held::Busy);
        }
        Ok(InFlight {
            throttles: self,
            key: pair,
        })
    }

    /// Charges one bcrypt job to the address, after checking it.
    ///
    /// # Errors
    /// [`Held::Work`] or [`Held::Busy`].
    pub fn spend(&self, peer: Peer, known: bool, now_ms: u64) -> Result<(), Held> {
        let Peer::Addr(ip) = peer else {
            return Ok(());
        };
        if known {
            return Ok(());
        }
        match self.work.charge(&ip, now_ms) {
            Gate::Open => Ok(()),
            Gate::Locked(wait_ms) => Err(Held::Work { wait_ms }),
            Gate::Full => Err(Held::Busy),
        }
    }

    /// A wrong password: a failure on the (name, address), name and
    /// address keys.
    pub fn fail(&self, key: &NameKey, peer: Peer, now_ms: u64) {
        self.name_address.fail(&(key.clone(), peer), now_ms);
        self.name.fail(key, now_ms);
        if let Peer::Addr(ip) = peer {
            self.address.fail(&ip, now_ms);
        }
    }

    /// A right password: clears the (name, address) key only, so nobody
    /// resets the loose keys by logging into their own account.
    pub fn succeed(&self, key: &NameKey, peer: Peer, now_ms: u64) {
        self.name_address.succeed(&(key.clone(), peer), now_ms);
    }

    /// Whether `peer` may make a new character today.
    ///
    /// # Errors
    /// The cap that applies.
    pub fn may_create(&self, peer: Peer, now_ms: u64) -> Result<(), CreateRefusal> {
        let (here, total) = {
            let mut c = lock(&self.created);
            c.roll(now_ms);
            let here = peer
                .creation_bucket()
                .map_or(0, |b| c.per.get(&b).copied().unwrap_or(0));
            (here, c.total)
        };
        if peer.creation_bucket().is_some()
            && self.per_address_per_day > 0
            && here >= self.per_address_per_day
        {
            return Err(CreateRefusal::PerAddress);
        }
        if self.per_day > 0 && total >= self.per_day {
            return Err(CreateRefusal::PerDay);
        }
        Ok(())
    }

    /// Takes a new character's place in today's counts, checking the caps
    /// and counting it under one lock, so creations that start together
    /// cannot all pass a cap.
    ///
    /// # Errors
    /// The cap that applies; nothing is counted.
    pub fn reserve_creation(
        &self,
        peer: Peer,
        now_ms: u64,
    ) -> Result<CreationSlot<'_>, CreateRefusal> {
        let mut c = lock(&self.created);
        c.roll(now_ms);
        let bucket = peer.creation_bucket();
        let here = bucket.map_or(0, |b| c.per.get(&b).copied().unwrap_or(0));
        if bucket.is_some() && self.per_address_per_day > 0 && here >= self.per_address_per_day {
            return Err(CreateRefusal::PerAddress);
        }
        if self.per_day > 0 && c.total >= self.per_day {
            return Err(CreateRefusal::PerDay);
        }
        if let Some(b) = bucket {
            *c.per.entry(b).or_default() += 1;
        }
        c.total += 1;
        Ok(CreationSlot {
            throttles: self,
            bucket,
            day: c.day,
            committed: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv6Addr;

    use proptest::prelude::*;

    use super::*;

    const P: ThrottlePolicy = ThrottlePolicy {
        free: 2,
        base_ms: 1_000,
        max_ms: 8_000,
        forget_ms: 100_000,
    };

    fn fails(n: usize, gap: u64) -> Throttle {
        let mut t = Throttle::default();
        for i in 0..n {
            t = throttle_step(t, ThrottleOp::Fail, 10 + gap * i as u64, &P);
        }
        t
    }

    #[test]
    fn free_failures_do_not_lock() {
        let t = fails(2, 1);
        assert_eq!((t.failures, t.delay_ms), (2, 0));
        assert!(throttle_allows(&t, t.last_failure_ms));
    }

    #[test]
    fn the_first_failure_past_free_locks_for_base() {
        let t = fails(3, 1);
        assert_eq!(t.delay_ms, 1_000);
        assert_eq!(t.locked_until_ms, t.last_failure_ms + 1_000);
        assert!(!throttle_allows(&t, t.last_failure_ms + 999));
        assert!(throttle_allows(&t, t.last_failure_ms + 1_000));
    }

    #[test]
    fn delays_double_up_to_the_cap() {
        let delays: Vec<u64> = (3..=8).map(|n| fails(n, 1).delay_ms).collect();
        assert_eq!(delays, [1_000, 2_000, 4_000, 8_000, 8_000, 8_000]);
    }

    #[test]
    fn success_clears_the_lock() {
        let t = throttle_step(fails(5, 1), ThrottleOp::Succeed, 50, &P);
        assert_eq!((t.failures, t.delay_ms, t.locked_until_ms), (0, 0, 0));
        assert!(throttle_allows(&t, 0));
    }

    #[test]
    fn a_quiet_spell_forgets() {
        let t = fails(6, 1);
        let later = throttle_step(t, ThrottleOp::Fail, t.last_failure_ms + 100_000, &P);
        assert_eq!((later.failures, later.delay_ms), (1, 0));
    }

    #[test]
    fn huge_times_saturate() {
        let mut t = Throttle {
            failures: u32::MAX,
            delay_ms: 8_000,
            locked_until_ms: 0,
            last_failure_ms: u64::MAX - 1,
        };
        t = throttle_step(t, ThrottleOp::Fail, u64::MAX - 1, &P);
        assert_eq!(t.failures, u32::MAX);
        assert_eq!(t.locked_until_ms, u64::MAX);
        let wide = ThrottlePolicy {
            max_ms: u64::MAX,
            ..P
        };
        let big = Throttle {
            failures: 10,
            delay_ms: u64::MAX / 2 + 1,
            locked_until_ms: 0,
            last_failure_ms: 5,
        };
        assert_eq!(
            throttle_step(big, ThrottleOp::Fail, 6, &wide).delay_ms,
            u64::MAX
        );
    }

    #[test]
    fn a_lowered_cap_or_raised_base_applies_to_a_stored_delay() {
        let stored = Throttle {
            failures: 9,
            delay_ms: 900_000,
            locked_until_ms: 0,
            last_failure_ms: 0,
        };
        assert_eq!(
            throttle_step(stored, ThrottleOp::Fail, 1, &P).delay_ms,
            8_000
        );
        let small = Throttle {
            delay_ms: 10,
            ..stored
        };
        assert_eq!(
            throttle_step(small, ThrottleOp::Fail, 1, &P).delay_ms,
            1_000
        );
    }

    proptest! {
        #[test]
        fn never_locked_longer_than_the_cap(
            failures in any::<u32>(), delay in any::<u64>(), locked in any::<u64>(),
            last in any::<u64>(), now in 0u64..u64::MAX / 2,
            free in 0u32..10, base in 1u64..1_000_000, max in 1u64..86_400_000,
        ) {
            let p = ThrottlePolicy { free, base_ms: base.min(max), max_ms: max, forget_ms: max + 1 };
            let t = Throttle { failures, delay_ms: delay, locked_until_ms: locked, last_failure_ms: last };
            let r = throttle_step(t, ThrottleOp::Fail, now, &p);
            prop_assert!(r.delay_ms <= p.max_ms);
            prop_assert!(r.locked_until_ms <= now + p.max_ms);
            prop_assert!(throttle_allows(&r, now + p.max_ms));
        }

        /// `lemma_rate_bound`, with its exact indexing: `ts[0]` unused;
        /// failure `j` at `ts[j]` admitted by the state after `j - 1`.
        #[test]
        fn checked_failures_are_spaced(
            gaps in proptest::collection::vec(0u64..20_000, 1..40),
            free in 0u32..5,
        ) {
            let p = ThrottlePolicy { free, base_ms: 1_000, max_ms: 16_000, forget_ms: 60_000 };
            let mut ts = vec![0u64, 1];
            let mut s = Throttle::default();
            let mut states = vec![s];
            s = throttle_step(s, ThrottleOp::Fail, ts[1], &p);
            states.push(s);
            for g in gaps {
                // The next admitted time at or after the gap.
                let mut t = ts[ts.len() - 1] + g;
                if !throttle_allows(&s, t) { t = s.locked_until_ms; }
                if t - ts[ts.len() - 1] >= p.forget_ms { break; }
                ts.push(t);
                s = throttle_step(s, ThrottleOp::Fail, t, &p);
                states.push(s);
            }
            let n = ts.len() - 1;
            for j in 1..=n {
                prop_assert_eq!(states[j].failures as usize, j);
                prop_assert!(throttle_allows(&states[j - 1], ts[j]));
            }
            for j in 2..=n {
                if j - 1 > free as usize {
                    prop_assert!(ts[j] - ts[j - 1] >= p.base_ms.min(p.max_ms));
                }
            }
        }
    }

    fn key(s: &str) -> NameKey {
        NameKey::parse(s).unwrap()
    }

    fn addr(s: &str) -> Peer {
        Peer::Addr(s.parse().unwrap())
    }

    fn tight() -> ThrottlePolicies {
        let p = ThrottlePolicy {
            free: 1,
            base_ms: 1_000,
            max_ms: 10_000,
            forget_ms: 100_000,
        };
        ThrottlePolicies {
            name_address: p,
            name: p,
            address: p,
            work: p,
        }
    }

    #[test]
    fn a_success_clears_only_the_name_address_key() {
        let t = Throttles::new(tight(), 0, 0);
        let (ada, here) = (key("ada"), addr("10.0.0.1"));
        for _ in 0..3 {
            t.fail(&ada, here, 5);
        }
        t.succeed(&ada, here, 6);
        assert_eq!(
            t.name_address.get(&(ada.clone(), here)).unwrap().failures,
            0
        );
        assert_eq!(t.name.get(&ada).unwrap().failures, 3);
        let ip = "10.0.0.1".parse().unwrap();
        assert_eq!(t.address.get(&ip).unwrap().failures, 3);
        assert_eq!(
            t.check(&ada, here, false, 7),
            Err(Held::Address { wait_ms: 1_998 })
        );
    }

    #[test]
    fn a_known_pair_skips_the_name_address_and_work_keys() {
        // "the name, address and work keys": the loose ones.
        let t = Throttles::new(tight(), 0, 0);
        let (ada, home, away) = (key("ada"), addr("10.0.0.1"), addr("10.0.0.2"));
        for _ in 0..3 {
            t.fail(&ada, away, 5);
            let _ = t.spend(home, false, 5);
        }
        assert!(matches!(t.spend(home, false, 6), Err(Held::Work { .. })));
        assert!(matches!(
            t.check(&ada, home, false, 6),
            Err(Held::Name { .. })
        ));
        assert_eq!(t.check(&ada, home, true, 6), Ok(()));
        for _ in 0..5 {
            assert_eq!(t.spend(home, true, 6), Ok(()));
        }
    }

    #[test]
    fn shared_peers_have_no_address_or_work_key() {
        let t = Throttles::new(tight(), 1, 0);
        let ada = key("ada");
        for _ in 0..10 {
            assert_eq!(t.spend(Peer::Shared, false, 5), Ok(()));
            t.fail(&ada, Peer::Shared, 5);
        }
        assert!(lock(&t.address.map).is_empty() && lock(&t.work.map).is_empty());
        assert!(matches!(
            t.check(&ada, Peer::Shared, false, 6),
            Err(Held::Name { .. })
        ));
        for _ in 0..3 {
            assert_eq!(t.may_create(Peer::Shared, 5), Ok(()));
            let _ = t.reserve_creation(Peer::Shared, 5).unwrap().commit();
        }
    }

    #[test]
    fn eviction_never_drops_a_key_past_free() {
        let t = Throttles::with_cap(tight(), 0, 0, 2);
        let here = addr("10.0.0.1");
        for n in ["aa", "bb"] {
            t.fail(&key(n), here, 5);
            t.fail(&key(n), here, 5);
        }
        // Both keys are past free and live: a new key has no room.
        assert_eq!(
            t.check(&key("cc"), addr("10.0.0.9"), false, 20_000),
            Err(Held::Busy)
        );
        // A known pair is never Busy, and its failure is simply not kept.
        assert_eq!(t.check(&key("cc"), here, true, 20_000), Ok(()));
        t.fail(&key("cc"), here, 20_000);
        assert!(t.name_address.get(&(key("cc"), here)).is_none());
        // Once quiet for forget_ms they may go.
        assert_eq!(
            t.check(&key("cc"), addr("10.0.0.9"), false, 200_000),
            Ok(())
        );
        assert_eq!(t.spend(addr("10.0.0.7"), false, 200_000), Ok(()));
        assert_eq!(t.spend(addr("10.0.0.8"), false, 200_000), Ok(()));
    }

    #[test]
    fn one_check_per_name_and_address_at_a_time() {
        let t = Throttles::new(tight(), 0, 0);
        let ada = key("ada");
        let first = t.begin(&ada, addr("10.0.0.1")).unwrap();
        assert_eq!(t.begin(&ada, addr("10.0.0.1")).unwrap_err(), Held::Busy);
        let other = t.begin(&ada, addr("10.0.0.2")).unwrap();
        drop(first);
        assert!(t.begin(&ada, addr("10.0.0.1")).is_ok());
        drop(other);
    }

    #[test]
    fn admit_gates_and_charges_under_one_lock() {
        let t = Throttles::new(tight(), 0, 0);
        let ada = key("ada");
        let (a, b, c) = (addr("10.0.0.1"), addr("10.0.0.2"), addr("10.0.0.3"));
        // Two checks admitted before either settles: the name key counts
        // both (free 1, so the second sets the first delay).
        let first = t.admit(&ada, a, false, 5).unwrap();
        let second = t.admit(&ada, b, false, 5).unwrap();
        assert_eq!(
            t.admit(&ada, c, false, 5).unwrap_err(),
            Held::Name { wait_ms: 1_000 }
        );
        assert_eq!(t.name.get(&ada).unwrap().failures, 2);
        // A held admit charges nothing, and the pair in flight is Busy.
        assert_eq!(t.address.get(&"10.0.0.3".parse().unwrap()), None);
        assert_eq!(t.admit(&ada, a, true, 5).unwrap_err(), Held::Busy);
        // The first was wrong: its charges stand. Nothing stepped the
        // name key after the second's charge, so its success takes that
        // charge back.
        first.failed();
        second.succeeded();
        assert_eq!(t.name.get(&ada).unwrap().failures, 1);
        assert_eq!(t.address.get(&"10.0.0.2".parse().unwrap()), None);
        assert_eq!(t.name_address.get(&(ada.clone(), b)).unwrap().failures, 0);
        assert_eq!(
            t.work.get(&"10.0.0.2".parse().unwrap()).unwrap().failures,
            1
        );
    }

    #[test]
    fn a_charge_stepped_over_is_never_taken_back() {
        let t = Throttles::new(tight(), 0, 0);
        let ada = key("ada");
        let first = t.admit(&ada, addr("10.0.0.1"), false, 5).unwrap();
        let second = t.admit(&ada, addr("10.0.0.2"), false, 5).unwrap();
        first.succeeded();
        assert_eq!(t.name.get(&ada).unwrap().failures, 2, "stepped over: kept");
        second.refund();
        assert_eq!(t.name.get(&ada).unwrap().failures, 1);
        assert_eq!(
            t.name_address.get(&(ada.clone(), addr("10.0.0.2"))),
            None,
            "a refund takes back the pair's charge too"
        );
    }

    #[test]
    fn a_known_pair_is_charged_on_its_own_key_and_fails_onto_the_loose_ones() {
        let t = Throttles::new(tight(), 0, 0);
        let (ada, home) = (key("ada"), addr("10.0.0.1"));
        let ip = "10.0.0.1".parse().unwrap();
        t.admit(&ada, home, true, 5).unwrap().failed();
        assert_eq!(
            t.name_address.get(&(ada.clone(), home)).unwrap().failures,
            1
        );
        assert_eq!(t.name.get(&ada).unwrap().failures, 1);
        assert_eq!(t.address.get(&ip).unwrap().failures, 1);
        assert_eq!(t.work.get(&ip), None, "a known pair spends no work");
        t.admit(&ada, home, true, 6).unwrap().succeeded();
        assert_eq!(t.name_address.get(&(ada, home)).unwrap().failures, 0);
        // Shared peers have no address or work key to charge.
        let bo = key("bo");
        t.admit(&bo, Peer::Shared, false, 7).unwrap().failed();
        assert_eq!(t.name.get(&bo).unwrap().failures, 1);
    }

    #[test]
    fn admit_holds_on_the_address_and_the_work() {
        let t = Throttles::new(tight(), 0, 0);
        let here = addr("10.0.0.1");
        for n in ["aa", "bb"] {
            t.admit(&key(n), here, false, 5).unwrap().failed();
        }
        assert!(matches!(
            t.admit(&key("cc"), here, false, 5),
            Err(Held::Address { .. })
        ));
        let w = Throttles::new(tight(), 0, 0);
        let away = addr("10.0.0.9");
        for n in ["aa", "bb"] {
            w.admit(&key(n), away, false, 5).unwrap().succeeded();
        }
        assert!(matches!(
            w.admit(&key("cc"), away, false, 5),
            Err(Held::Work { .. })
        ));
        let full = Throttles::with_cap(tight(), 0, 0, 1);
        for n in ["aa", "aa"] {
            let _ = full.admit(&key(n), here, false, 5).map(Attempt::failed);
        }
        assert_eq!(
            full.admit(&key("zz"), addr("10.0.0.2"), false, 5)
                .unwrap_err(),
            Held::Busy
        );
    }

    #[test]
    fn creation_slots_count_at_once_and_come_back_unless_committed() {
        let t = Throttles::new(tight(), 1, 0);
        let p = addr("10.0.0.1");
        let slot = t.reserve_creation(p, 5).unwrap();
        assert_eq!(
            t.reserve_creation(p, 5).unwrap_err(),
            CreateRefusal::PerAddress
        );
        drop(slot);
        let again = t.reserve_creation(p, 5).unwrap();
        assert_eq!(again.commit(), 1);
        assert_eq!(
            t.reserve_creation(p, 6).unwrap_err(),
            CreateRefusal::PerAddress
        );
        // A slot from yesterday gives nothing back to today.
        let late = t.reserve_creation(addr("10.0.0.2"), DAY_MS - 1).unwrap();
        let today = t.reserve_creation(addr("10.0.0.3"), DAY_MS).unwrap();
        drop(late);
        assert_eq!(today.commit(), 1);
        let g = Throttles::new(tight(), 0, 1);
        let only = g.reserve_creation(Peer::Shared, 1).unwrap();
        assert_eq!(g.reserve_creation(p, 1).unwrap_err(), CreateRefusal::PerDay);
        drop(only);
        assert!(g.reserve_creation(p, 1).is_ok());
    }

    #[test]
    fn new_characters_are_capped_per_address_and_per_day() {
        let t = Throttles::new(tight(), 5, 0);
        let day = 86_400_000;
        let a = Peer::Addr(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 1, 0, 0, 0, 1)));
        let b = Peer::Addr(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 2, 0, 0, 0, 1)));
        for i in 0..5 {
            let p = if i % 2 == 0 { a } else { b };
            assert_eq!(t.may_create(p, day), Ok(()));
            let _ = t.reserve_creation(p, day).unwrap().commit();
        }
        assert_eq!(t.may_create(b, day + 1), Err(CreateRefusal::PerAddress));
        assert_eq!(t.may_create(addr("10.0.0.1"), day + 1), Ok(()));
        assert_eq!(t.may_create(b, 2 * day), Ok(()), "the next UTC day");
        let g = Throttles::new(tight(), 0, 2);
        assert_eq!(g.reserve_creation(addr("10.0.0.1"), 1).unwrap().commit(), 1);
        assert_eq!(g.reserve_creation(addr("10.0.0.2"), 1).unwrap().commit(), 2);
        assert_eq!(
            g.may_create(addr("10.0.0.3"), 1),
            Err(CreateRefusal::PerDay)
        );
        let off = Throttles::new(tight(), 0, 0);
        for i in 0..100 {
            assert_eq!(off.may_create(Peer::Shared, 1), Ok(()));
            assert_eq!(
                off.reserve_creation(Peer::Shared, 1).unwrap().commit(),
                i + 1
            );
        }
    }
}
