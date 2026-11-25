//! What the throttles key an attempt by.

use std::net::{IpAddr, Ipv6Addr};

/// Where a login attempt comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Peer {
    /// A real client address, already bucketed (IPv4 `/32`, IPv6 `/64`).
    Addr(IpAddr),
    /// No address is known (or an operator's own CLI): address-keyed
    /// throttles and caps are off for it. A client never chooses it: a
    /// web client is keyed by its TCP peer unless a trusted proxy names
    /// it.
    Shared,
}

impl Peer {
    /// The bucket new characters are counted in: IPv4 `/32`, IPv6 `/48`
    /// (the block one site is usually given). `None` for [`Peer::Shared`].
    #[must_use]
    pub fn creation_bucket(self) -> Option<IpAddr> {
        match self {
            Self::Shared => None,
            Self::Addr(IpAddr::V4(v4)) => Some(IpAddr::V4(v4)),
            Self::Addr(IpAddr::V6(v6)) => Some(v6.to_ipv4_mapped().map_or_else(
                || {
                    let s = v6.segments();
                    IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], 0, 0, 0, 0, 0))
                },
                IpAddr::V4,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_buckets_are_slash_32_and_slash_48() {
        let v4: IpAddr = "10.1.2.3".parse().unwrap();
        assert_eq!(Peer::Addr(v4).creation_bucket(), Some(v4));
        let v6: IpAddr = "2001:db8:1:2::9".parse().unwrap();
        assert_eq!(
            Peer::Addr(v6).creation_bucket(),
            Some("2001:db8:1::".parse().unwrap())
        );
        let mapped: IpAddr = "::ffff:10.1.2.3".parse().unwrap();
        assert_eq!(Peer::Addr(mapped).creation_bucket(), Some(v4));
        assert_eq!(Peer::Shared.creation_bucket(), None);
    }
}
