//! Connection admission: a cap on open connections in total and per
//! client address, shared by the web terminal and telnet.
//!
//! The counting step [`conn_step`] is proved in `formal/verus/src/connections.rs`:
//! an open is admitted iff both counts are under their caps, a close
//! never underflows, and every step keeps `here <= total`,
//! `total <= max_total` and `here <= max_here`.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};

/// A connection event, as the counter sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// mirror-begin conn_op
pub enum ConnOp {
    /// A client connects.
    Open,
    /// A client goes away.
    Close,
}
// mirror-end conn_op

// mirror-begin conn_step
/// One step over the total count and one address's count: the new total,
/// the new count for the address, and whether the step took effect.
#[must_use]
pub const fn conn_step(
    total: u32,
    here: u32,
    op: ConnOp,
    max_total: u32,
    max_here: u32,
) -> (u32, u32, bool) {
    match op {
        ConnOp::Open => {
            if total < max_total && here < max_here {
                (total + 1, here + 1, true)
            } else {
                (total, here, false)
            }
        }
        ConnOp::Close => {
            if here > 0 && total > 0 {
                (total - 1, here - 1, true)
            } else {
                (total, here, false)
            }
        }
    }
}
// mirror-end conn_step

/// Why a connection was turned away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// Every slot is taken.
    #[error("The server is full. Try again later.")]
    Full,
    /// This address already holds its share of slots.
    #[error("Too many connections from your address. Try again later.")]
    TooMany,
}

/// The address a connection counts against.
///
/// IPv4 and IPv4-mapped IPv6 addresses count as themselves; any other IPv6
/// address counts as its `/64`, the block one subscriber is usually given,
/// so rotating through one block does not multiply the share. `None`: the
/// address is unknown, and all such connections share one bucket.
#[must_use]
pub fn bucket(addr: Option<IpAddr>) -> Option<IpAddr> {
    match addr? {
        IpAddr::V4(v4) => Some(IpAddr::V4(v4)),
        IpAddr::V6(v6) => Some(v6.to_ipv4_mapped().map_or_else(
            || {
                let s = v6.segments();
                IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
            },
            IpAddr::V4,
        )),
    }
}

/// The client address of a web request: the one the trusted-proxy layer
/// resolved from `X-Forwarded-For`, else the TCP peer.
#[must_use]
pub fn client_addr(resolved: Option<IpAddr>, peer: Option<SocketAddr>) -> Option<IpAddr> {
    resolved.or_else(|| peer.map(|p| p.ip()))
}

/// What the login throttles key a web client by: the address the
/// trusted-proxy layer resolved, else the TCP peer, bucketed; with
/// neither, [`Peer::Shared`].
///
/// A client's own `X-Forwarded-For`, `Forwarded` or `X-Real-IP` never
/// changes the key: with no trusted proxy the layer resolves the TCP peer,
/// so a spoofed header lands in the sender's own buckets. A request that
/// carries such a header while no proxy is trusted logs one warning,
/// because behind a real proxy every player then shares its address
/// (stricter limits, never looser ones; an https `public_url` without
/// `trusted_proxies` is refused at start).
///
/// [`Peer::Shared`]: mud_accounts::Peer::Shared
#[must_use]
pub fn account_peer(
    resolved: Option<IpAddr>,
    peer: Option<SocketAddr>,
    forwarded: bool,
    trusted: bool,
) -> mud_accounts::Peer {
    use mud_accounts::Peer;
    if forwarded && !trusted {
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            tracing::warn!(
                "a web client sent a forwarding header, but [server] trusted_proxies is empty; login limits key it by its TCP peer"
            );
        });
    }
    bucket(client_addr(resolved, peer)).map_or(Peer::Shared, Peer::Addr)
}

#[derive(Debug, Default)]
struct Counts {
    total: u32,
    per: HashMap<Option<IpAddr>, u32>,
}

/// Admits connections up to a total and a per-address cap.
#[derive(Debug)]
pub struct ConnLimiter {
    max_total: u32,
    max_here: u32,
    counts: Mutex<Counts>,
}

/// A held slot; dropping it frees the slot.
#[derive(Debug)]
pub struct ConnSlot {
    limiter: Arc<ConnLimiter>,
    bucket: Option<IpAddr>,
}

/// A cap from config: `0` means none.
fn cap(n: usize) -> u32 {
    if n == 0 {
        u32::MAX
    } else {
        u32::try_from(n).unwrap_or(u32::MAX)
    }
}

impl ConnLimiter {
    /// A limiter for at most `max_total` connections, `max_per_address`
    /// of them from one address. `0` lifts a cap.
    #[must_use]
    pub fn new(max_total: usize, max_per_address: usize) -> Arc<Self> {
        Arc::new(Self {
            max_total: cap(max_total),
            max_here: cap(max_per_address),
            counts: Mutex::new(Counts::default()),
        })
    }

    /// Takes a slot for a client at `addr`.
    ///
    /// # Errors
    /// [`Refusal::Full`] when every slot is taken, else
    /// [`Refusal::TooMany`] when `addr` holds its share.
    pub fn try_acquire(self: &Arc<Self>, addr: Option<IpAddr>) -> Result<ConnSlot, Refusal> {
        let bucket = bucket(addr);
        let mut c = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        let here = c.per.get(&bucket).copied().unwrap_or(0);
        let (total, here, ok) =
            conn_step(c.total, here, ConnOp::Open, self.max_total, self.max_here);
        if !ok {
            return Err(if c.total >= self.max_total {
                Refusal::Full
            } else {
                Refusal::TooMany
            });
        }
        c.total = total;
        c.per.insert(bucket, here);
        drop(c);
        Ok(ConnSlot {
            limiter: Arc::clone(self),
            bucket,
        })
    }

    /// Open connections in total.
    #[must_use]
    pub fn open(&self) -> u32 {
        self.counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .total
    }

    fn release(&self, bucket: Option<IpAddr>) {
        let mut c = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        let here = c.per.get(&bucket).copied().unwrap_or(0);
        let (total, here, _) =
            conn_step(c.total, here, ConnOp::Close, self.max_total, self.max_here);
        c.total = total;
        if here == 0 {
            c.per.remove(&bucket);
        } else {
            c.per.insert(bucket, here);
        }
    }
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        self.limiter.release(self.bucket);
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use proptest::prelude::*;

    use super::*;

    #[expect(clippy::unnecessary_wraps, reason = "matches try_acquire's argument")]
    fn v4(last: u8) -> Option<IpAddr> {
        Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, last)))
    }

    #[test]
    fn one_address_gets_its_share_and_no_more() {
        let l = ConnLimiter::new(10, 2);
        let a = l.try_acquire(v4(1)).unwrap();
        let _b = l.try_acquire(v4(1)).unwrap();
        assert_eq!(l.try_acquire(v4(1)).unwrap_err(), Refusal::TooMany);
        let _other = l.try_acquire(v4(2)).unwrap();
        drop(a);
        assert!(l.try_acquire(v4(1)).is_ok(), "a dropped slot is free again");
    }

    #[test]
    fn the_total_cap_says_full() {
        let l = ConnLimiter::new(2, 5);
        let _a = l.try_acquire(v4(1)).unwrap();
        let _b = l.try_acquire(v4(2)).unwrap();
        assert_eq!(l.try_acquire(v4(3)).unwrap_err(), Refusal::Full);
        assert_eq!(l.open(), 2);
    }

    #[test]
    fn zero_lifts_a_cap() {
        let l = ConnLimiter::new(0, 0);
        let held: Vec<_> = (0..300).map(|_| l.try_acquire(v4(1)).unwrap()).collect();
        assert_eq!(l.open(), 300);
        drop(held);
        assert_eq!(l.open(), 0);
        assert!(l.counts.lock().unwrap().per.is_empty(), "no stale buckets");
    }

    #[test]
    fn an_ipv6_block_counts_as_one_address() {
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        let other: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(bucket(Some(a)), bucket(Some(b)));
        assert_ne!(bucket(Some(a)), bucket(Some(other)));
        let mapped: IpAddr = "::ffff:203.0.113.7".parse().unwrap();
        assert_eq!(bucket(Some(mapped)), v4(7));
        let l = ConnLimiter::new(10, 1);
        let _x = l.try_acquire(Some(a)).unwrap();
        assert_eq!(l.try_acquire(Some(b)).unwrap_err(), Refusal::TooMany);
    }

    #[test]
    fn unknown_peers_share_one_bucket() {
        let l = ConnLimiter::new(10, 1);
        let _x = l.try_acquire(None).unwrap();
        assert_eq!(l.try_acquire(None).unwrap_err(), Refusal::TooMany);
        assert!(l.try_acquire(v4(1)).is_ok());
    }

    #[test]
    fn a_resolved_client_address_wins_over_the_peer() {
        let peer: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let real: IpAddr = "198.51.100.4".parse().unwrap();
        assert_eq!(client_addr(Some(real), Some(peer)), Some(real));
        assert_eq!(client_addr(None, Some(peer)), Some(peer.ip()));
        assert_eq!(client_addr(None, None), None);
    }

    #[test]
    fn account_peer_cases() {
        use mud_accounts::Peer;
        let real: IpAddr = "203.0.113.7".parse().unwrap();
        let proxy: SocketAddr = "10.0.0.2:5555".parse().unwrap();
        assert_eq!(
            account_peer(Some(real), Some(proxy), true, true),
            Peer::Addr(real)
        );
        // Untrusted: a client's own header never lowers its key; the TCP
        // peer (the proxy, when one is in front) is the key.
        assert_eq!(
            account_peer(None, Some(proxy), true, false),
            Peer::Addr(proxy.ip())
        );
        assert_eq!(
            account_peer(Some(proxy.ip()), Some(proxy), true, false),
            Peer::Addr(proxy.ip()),
        );
        assert_eq!(
            account_peer(None, Some(proxy), false, false),
            Peer::Addr(proxy.ip())
        );
        let v6: SocketAddr = "[2001:db8:1:2:3:4:5:6]:1".parse().unwrap();
        assert_eq!(
            account_peer(None, Some(v6), false, false),
            Peer::Addr("2001:db8:1:2::".parse().unwrap())
        );
        assert_eq!(account_peer(None, None, false, false), Peer::Shared);
    }

    #[test]
    fn a_close_with_nothing_open_does_nothing() {
        assert_eq!(conn_step(0, 0, ConnOp::Close, 5, 5), (0, 0, false));
        assert_eq!(conn_step(3, 0, ConnOp::Close, 5, 5), (3, 0, false));
        assert_eq!(
            conn_step(u32::MAX, 0, ConnOp::Open, u32::MAX, 5),
            (u32::MAX, 0, false)
        );
    }

    proptest! {
        /// Any interleaving of opens and closes over a few addresses keeps
        /// every count within its cap and the total equal to the sum.
        #[test]
        fn counts_stay_within_caps(
            ops in proptest::collection::vec((any::<bool>(), 0u8..4), 0..200),
            max_total in 1usize..12,
            max_here in 1usize..5,
        ) {
            let l = ConnLimiter::new(max_total, max_here);
            let mut held: Vec<Vec<ConnSlot>> = (0..4).map(|_| Vec::new()).collect();
            for (open, who) in ops {
                let i = usize::from(who);
                if open {
                    if let Ok(s) = l.try_acquire(v4(who)) {
                        held[i].push(s);
                    }
                } else {
                    held[i].pop();
                }
                let c = l.counts.lock().unwrap();
                let sum: u32 = c.per.values().sum();
                prop_assert_eq!(c.total, sum);
                prop_assert!(c.total as usize <= max_total);
                prop_assert!(c.per.values().all(|n| *n as usize <= max_here && *n > 0));
                drop(c);
                for (j, h) in held.iter().enumerate() {
                    let n = l.counts.lock().unwrap().per.get(&v4(u8::try_from(j).unwrap())).copied().unwrap_or(0);
                    prop_assert_eq!(n as usize, h.len());
                }
            }
        }
    }
}
