//! What is known about peer addresses before dialing them: BEP 11 PEX flags
//! and what earlier connections showed.

use std::collections::HashMap;
use std::net::SocketAddr;

use crate::pool::MAX_POOL;
use crate::types::{DialHints, PeerKey, PeerLink};
use crate::web_seed::is_web_seed_addr;
use dravity_wire::Reserved;

/// Most addresses remembered (twice the dial pool, so hints outlive a
/// pool refill).
pub(crate) const MAX_HINTS: usize = 2 * MAX_POOL;

/// Dial hints by address, bounded by [`MAX_HINTS`].
#[derive(Debug, Default, Clone)]
pub(crate) struct HintBook {
    map: HashMap<SocketAddr, DialHints>,
}

impl HintBook {
    /// What is known about `addr`.
    pub(crate) fn get(&self, addr: SocketAddr) -> DialHints {
        self.map.get(&addr).copied().unwrap_or_default()
    }

    /// PEX flags from another peer: they only fill in what our own
    /// connections have not shown.
    pub(crate) fn learn_pex(&mut self, addr: SocketAddr, flags: Option<u8>) {
        self.merge(addr, DialHints::from_pex_flags(flags), false);
    }

    /// A connection we made to `addr`: what it showed replaces what PEX
    /// said.
    pub(crate) fn observe(&mut self, addr: SocketAddr, link: PeerLink) {
        self.merge(addr, DialHints::from_link(link), true);
    }

    /// Hints carried over from another state (a magnet's metadata fetch).
    pub(crate) fn extend(&mut self, hints: impl IntoIterator<Item = (SocketAddr, DialHints)>) {
        for (addr, h) in hints {
            self.merge(addr, h, true);
        }
    }

    /// Every remembered address, sorted.
    pub(crate) fn entries(&self) -> Vec<(SocketAddr, DialHints)> {
        let mut out: Vec<_> = self.map.iter().map(|(a, h)| (*a, *h)).collect();
        out.sort_unstable_by_key(|(a, _)| *a);
        out
    }

    fn merge(&mut self, addr: SocketAddr, new: DialHints, authoritative: bool) {
        if new == DialHints::default() || is_web_seed_addr(addr) {
            return;
        }
        if let Some(old) = self.map.get_mut(&addr) {
            *old = if authoritative {
                new.or(*old)
            } else {
                old.or(new)
            };
        } else if self.map.len() < MAX_HINTS {
            self.map.insert(addr, new);
        }
    }
}

/// A connection the driver reported, dialed or accepted.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Joined {
    pub(crate) key: PeerKey,
    pub(crate) addr: SocketAddr,
    pub(crate) reserved: Reserved,
    pub(crate) link: PeerLink,
    pub(crate) incoming: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: u16) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, 1], n))
    }

    #[test]
    fn connections_override_pex_and_pex_fills_gaps() {
        let mut book = HintBook::default();
        assert_eq!(book.get(addr(1)), DialHints::default());
        book.learn_pex(addr(1), Some(dravity_wire::PEX_UTP));
        assert_eq!(
            book.get(addr(1)),
            DialHints {
                encryption: Some(false),
                utp: Some(true)
            }
        );
        // We saw it complete MSE over TCP: encryption is now known; uTP
        // support stays as PEX said.
        book.observe(
            addr(1),
            PeerLink {
                utp: false,
                mse: Some(true),
            },
        );
        assert_eq!(
            book.get(addr(1)),
            DialHints {
                encryption: Some(true),
                utp: Some(true)
            }
        );
        // Later PEX gossip does not undo what we saw.
        book.learn_pex(addr(1), Some(0));
        assert_eq!(book.get(addr(1)).encryption, Some(true));
        // No flags: nothing learned.
        book.learn_pex(addr(2), None);
        assert!(book.entries().iter().all(|(a, _)| *a != addr(2)));
    }

    #[test]
    fn bounded() {
        let mut book = HintBook::default();
        for n in 0..=MAX_HINTS {
            let ip = std::net::Ipv4Addr::from(u32::try_from(n).unwrap_or(0));
            book.learn_pex(SocketAddr::from((ip, 1)), Some(1));
        }
        assert_eq!(book.entries().len(), MAX_HINTS);
    }
}
