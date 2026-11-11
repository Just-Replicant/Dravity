//! The session-wide ban list.
//!
//! A torrent that bans an address for bad data (hash-failure strikes or a
//! smart-ban conviction, [`dravity_engine::Alert::PeerBanned`]) adds it here;
//! every other running torrent is told with
//! [`dravity_engine::Event::BanIp`], new torrents start with the list, and
//! the listener refuses the address before the handshake. A poisoner caught
//! on one torrent is thus refused on all of them for the life of the
//! session.

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::{Arc, PoisonError, RwLock};

use crate::IpFilter;

/// Shared handle to the session's ban list.
pub(crate) type Bans = Arc<BanList>;

/// Addresses banned in this session (IPv4-mapped IPv6 addresses are kept as
/// their IPv4 address).
#[derive(Debug, Default)]
pub(crate) struct BanList {
    ips: RwLock<HashSet<IpAddr>>,
    /// The user's IP filter ([`crate::Session::set_ip_filter`]).
    filter: RwLock<Arc<IpFilter>>,
}

/// The key an address is banned under: IPv4-mapped IPv6 is its IPv4 address.
pub(crate) fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
    }
}

impl BanList {
    /// An empty ban list with `filter` in force.
    pub(crate) fn with_filter(filter: Arc<IpFilter>) -> Self {
        Self {
            ips: RwLock::default(),
            filter: RwLock::new(filter),
        }
    }

    /// Bans `ip`. Returns whether it was not banned before.
    pub(crate) fn insert(&self, ip: IpAddr) -> bool {
        self.ips
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(canonical(ip))
    }

    /// Lifts the ban on `ip`. Returns whether it was banned.
    pub(crate) fn remove(&self, ip: IpAddr) -> bool {
        self.ips
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&canonical(ip))
    }

    /// Whether `ip` is banned or blocked by the IP filter.
    pub(crate) fn contains(&self, ip: IpAddr) -> bool {
        self.ips
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&canonical(ip))
            || self.filter().is_blocked(ip)
    }

    /// The IP filter in force.
    pub(crate) fn filter(&self) -> Arc<IpFilter> {
        Arc::clone(&self.filter.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Replaces the IP filter.
    pub(crate) fn set_filter(&self, filter: Arc<IpFilter>) {
        *self.filter.write().unwrap_or_else(PoisonError::into_inner) = filter;
    }

    /// Every banned address, sorted.
    pub(crate) fn snapshot(&self) -> Vec<IpAddr> {
        let mut ips: Vec<IpAddr> = self
            .ips
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .copied()
            .collect();
        ips.sort_unstable();
        ips
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn mapped_addresses_share_a_key() {
        let list = BanList::default();
        let v4: IpAddr = "10.1.2.3".parse().unwrap();
        let mapped: IpAddr = "::ffff:10.1.2.3".parse().unwrap();
        assert!(list.insert(mapped));
        assert!(!list.insert(v4));
        assert!(list.contains(v4) && list.contains(mapped));
        assert_eq!(list.snapshot(), [v4]);
        assert!(list.remove(v4));
        assert!(!list.contains(mapped));
    }
}
