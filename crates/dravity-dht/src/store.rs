//! What a DHT node keeps for others, sans-IO: announce tokens (BEP 5)
//! and the peers announced to it. Callers pass the time and the secrets.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use sha1::{Digest, Sha1};

/// How often the token secret should be rotated; a token stays valid for
/// one rotation after it was handed out (BEP 5: about 10 minutes).
pub const TOKEN_ROTATION: Duration = Duration::from_secs(5 * 60);
/// How long an announced peer is kept (BEP 5 suggests 30 minutes).
pub const PEER_TTL: Duration = Duration::from_secs(30 * 60);

/// Announce tokens bound to the querier's IP address.
#[derive(Debug, Clone)]
pub struct Tokens {
    current: [u8; 16],
    previous: [u8; 16],
}

impl Tokens {
    /// Tokens from a random `secret`.
    #[must_use]
    pub const fn new(secret: [u8; 16]) -> Self {
        Self {
            current: secret,
            previous: secret,
        }
    }

    /// Starts a new period with `secret`; tokens of the last period
    /// still validate.
    pub const fn rotate(&mut self, secret: [u8; 16]) {
        self.previous = self.current;
        self.current = secret;
    }

    fn make(secret: &[u8; 16], ip: IpAddr) -> Vec<u8> {
        let mut h = Sha1::new();
        h.update(secret);
        match ip {
            IpAddr::V4(v4) => h.update(v4.octets()),
            IpAddr::V6(v6) => h.update(v6.octets()),
        }
        h.finalize().get(..8).unwrap_or_default().to_vec()
    }

    /// The token to give `ip` in a `get_peers` response.
    #[must_use]
    pub fn issue(&self, ip: IpAddr) -> Vec<u8> {
        Self::make(&self.current, ip)
    }

    /// Whether `token` was issued to `ip` in this period or the last.
    #[must_use]
    pub fn check(&self, token: &[u8], ip: IpAddr) -> bool {
        token == Self::make(&self.current, ip).as_slice()
            || token == Self::make(&self.previous, ip).as_slice()
    }
}

/// Peers announced to us, per info-hash, bounded in both directions.
#[derive(Debug, Clone)]
pub struct PeerStore {
    peers: HashMap<[u8; 20], Vec<(SocketAddr, Instant, bool)>>,
    max_hashes: usize,
    max_per_hash: usize,
}

impl PeerStore {
    /// A store of at most `max_hashes` info-hashes with `max_per_hash`
    /// peers each.
    #[must_use]
    pub fn new(max_hashes: usize, max_per_hash: usize) -> Self {
        Self {
            peers: HashMap::new(),
            max_hashes,
            max_per_hash,
        }
    }

    /// Records `peer` for `info_hash` (refreshing it if known). A new
    /// info-hash beyond the limit is dropped; a new peer beyond the
    /// per-hash limit replaces the oldest one.
    pub fn announce(&mut self, info_hash: [u8; 20], peer: SocketAddr, now: Instant) {
        self.announce_as(info_hash, peer, false, now);
    }

    /// [`Self::announce`], recording whether the peer is a seed (BEP 33).
    pub fn announce_as(&mut self, info_hash: [u8; 20], peer: SocketAddr, seed: bool, now: Instant) {
        self.expire(now);
        if !self.peers.contains_key(&info_hash) && self.peers.len() >= self.max_hashes {
            return;
        }
        let list = self.peers.entry(info_hash).or_default();
        if let Some(entry) = list.iter_mut().find(|(addr, _, _)| *addr == peer) {
            entry.1 = now;
            entry.2 = seed;
            return;
        }
        if list.len() >= self.max_per_hash {
            if let Some(oldest) = list.iter_mut().min_by_key(|(_, at, _)| *at) {
                *oldest = (peer, now, seed);
            }
            return;
        }
        list.push((peer, now, seed));
    }

    /// The BEP 33 bloom filters of live seeds and downloaders for
    /// `info_hash`.
    #[must_use]
    pub fn blooms(
        &self,
        info_hash: &[u8; 20],
        now: Instant,
    ) -> (crate::bloom::Bloom, crate::bloom::Bloom) {
        let (mut seeds, mut peers) = (crate::bloom::Bloom::new(), crate::bloom::Bloom::new());
        for (addr, at, seed) in self.peers.get(info_hash).into_iter().flatten() {
            if now.saturating_duration_since(*at) < PEER_TTL {
                if *seed { &mut seeds } else { &mut peers }.insert(addr.ip());
            }
        }
        (seeds, peers)
    }

    /// Info-hashes stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    /// `true` with nothing stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// Up to `max` stored info-hashes (BEP 51), starting at the
    /// `start`-th one (the caller passes a random number so repeated
    /// samples differ).
    #[must_use]
    pub fn sample(&self, max: usize, start: usize) -> Vec<[u8; 20]> {
        let mut all: Vec<&[u8; 20]> = self.peers.keys().collect();
        all.sort_unstable();
        let n = all.len();
        if n == 0 {
            return Vec::new();
        }
        (0..max.min(n))
            .filter_map(|i| all.get((start + i) % n).map(|h| **h))
            .collect()
    }

    /// Up to `max` live peers for `info_hash`, newest first.
    #[must_use]
    pub fn peers(&self, info_hash: &[u8; 20], now: Instant, max: usize) -> Vec<SocketAddr> {
        let mut live: Vec<&(SocketAddr, Instant, bool)> = self
            .peers
            .get(info_hash)
            .into_iter()
            .flatten()
            .filter(|(_, at, _)| now.saturating_duration_since(*at) < PEER_TTL)
            .collect();
        live.sort_by_key(|(_, at, _)| std::cmp::Reverse(*at));
        live.into_iter()
            .take(max)
            .map(|(addr, _, _)| *addr)
            .collect()
    }

    /// Drops expired peers and empty info-hashes.
    pub fn expire(&mut self, now: Instant) {
        self.peers.retain(|_, list| {
            list.retain(|(_, at, _)| now.saturating_duration_since(*at) < PEER_TTL);
            !list.is_empty()
        });
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn tokens_are_per_ip_and_survive_one_rotation() {
        let ip: IpAddr = "192.0.2.7".parse().unwrap();
        let other: IpAddr = "192.0.2.8".parse().unwrap();
        let mut tokens = Tokens::new([1; 16]);
        let token = tokens.issue(ip);
        assert_eq!(token.len(), 8);
        assert!(tokens.check(&token, ip));
        assert!(!tokens.check(&token, other));
        tokens.rotate([2; 16]);
        assert!(tokens.check(&token, ip), "valid for one more period");
        tokens.rotate([3; 16]);
        assert!(!tokens.check(&token, ip));
    }

    #[test]
    fn the_store_is_bounded_and_expires() {
        let now = Instant::now();
        let mut store = PeerStore::new(1, 2);
        let a: SocketAddr = "10.0.0.1:1".parse().unwrap();
        let b: SocketAddr = "10.0.0.2:1".parse().unwrap();
        let c: SocketAddr = "10.0.0.3:1".parse().unwrap();
        store.announce([1; 20], a, now);
        store.announce([1; 20], b, now + Duration::from_secs(1));
        store.announce([1; 20], c, now + Duration::from_secs(2));
        assert_eq!(
            store.peers(&[1; 20], now + Duration::from_secs(2), 10),
            [c, b]
        );
        store.announce([2; 20], a, now);
        assert!(store.peers(&[2; 20], now, 10).is_empty(), "hash limit");
        assert!(
            store
                .peers(&[1; 20], now + PEER_TTL + Duration::from_secs(3), 10)
                .is_empty()
        );
        store.expire(now + PEER_TTL + Duration::from_secs(3));
        store.announce([2; 20], a, now + PEER_TTL + Duration::from_secs(3));
        assert_eq!(
            store.peers(&[2; 20], now + PEER_TTL + Duration::from_secs(3), 10),
            [a]
        );
    }
}
