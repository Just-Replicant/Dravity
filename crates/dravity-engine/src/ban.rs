//! Banning peers that send data failing the hash check.
//!
//! Every accepted block records the IP address that sent it. When a piece
//! fails, each address that contributed gets a strike, two if it sent every
//! block of the piece alone (it is certainly the culprit). An address with
//! [`Config::hash_fail_ban`](crate::Config::hash_fail_ban) strikes is
//! banned: its connections are closed and it is never dialed or accepted
//! again. Bans are per IP, not per port, so a peer cannot come back from a
//! new source port.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;

/// Contributors of pieces in progress, strikes and bans.
#[derive(Debug, Clone, Default)]
pub(crate) struct HashBans {
    contributors: HashMap<u32, HashSet<IpAddr>>,
    strikes: HashMap<IpAddr, u32>,
    banned: HashSet<IpAddr>,
}

impl HashBans {
    /// `ip` sent a block of `piece`.
    pub(crate) fn record(&mut self, piece: u32, ip: IpAddr) {
        self.contributors.entry(piece).or_default().insert(ip);
    }

    /// `piece` passed: nobody is blamed for it.
    pub(crate) fn passed(&mut self, piece: u32) {
        self.contributors.remove(&piece);
    }

    /// `piece` failed. Returns the addresses banned by it, sorted.
    pub(crate) fn failed(&mut self, piece: u32, threshold: u32) -> Vec<IpAddr> {
        let Some(contributors) = self.contributors.remove(&piece) else {
            return Vec::new();
        };
        let weight = if contributors.len() == 1 { 2 } else { 1 };
        let mut banned = Vec::new();
        for ip in contributors {
            let strikes = self.strikes.entry(ip).or_insert(0);
            *strikes = strikes.saturating_add(weight);
            if threshold > 0 && *strikes >= threshold && self.banned.insert(ip) {
                banned.push(ip);
            }
        }
        banned.sort_unstable();
        banned
    }

    /// Whether `ip` is banned.
    pub(crate) fn is_banned(&self, ip: IpAddr) -> bool {
        self.banned.contains(&ip)
    }

    /// Number of banned addresses.
    pub(crate) fn count(&self) -> usize {
        self.banned.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, n])
    }

    #[test]
    fn a_sole_contributor_is_banned_at_once() {
        let mut bans = HashBans::default();
        bans.record(0, ip(1));
        bans.record(0, ip(1));
        assert_eq!(bans.failed(0, 2), [ip(1)]);
        assert!(bans.is_banned(ip(1)));
        assert_eq!(bans.count(), 1);
    }

    #[test]
    fn shared_failures_need_more_strikes() {
        let mut bans = HashBans::default();
        bans.record(0, ip(1));
        bans.record(0, ip(2));
        assert!(bans.failed(0, 2).is_empty());
        bans.record(1, ip(1));
        bans.record(1, ip(3));
        assert_eq!(bans.failed(1, 2), [ip(1)]);
        assert!(!bans.is_banned(ip(2)) && !bans.is_banned(ip(3)));
    }

    #[test]
    fn passed_pieces_and_threshold_zero() {
        let mut bans = HashBans::default();
        bans.record(0, ip(1));
        bans.passed(0);
        assert!(bans.failed(0, 2).is_empty(), "nothing recorded any more");
        bans.record(1, ip(1));
        assert!(bans.failed(1, 0).is_empty(), "banning disabled");
        assert!(!bans.is_banned(ip(1)));
    }
}
