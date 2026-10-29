//! Banning peers that send data failing the hash check.
//!
//! Every accepted block records the IP address that sent it. When a piece
//! fails, each address that contributed gets a strike, two if it sent every
//! block of the piece alone (it is certainly the culprit). An address with
//! [`Config::hash_fail_ban`](crate::Config::hash_fail_ban) strikes is
//! banned: its connections are closed and it is never dialed or accepted
//! again. Bans are per IP, not per port, so a peer cannot come back from a
//! new source port. An IPv4-mapped IPv6 address is the same key as the IPv4
//! address. Strikes are forgotten after
//! [`Config::hash_fail_quiet`](crate::Config::hash_fail_quiet) with no new
//! failure; a ban lasts for the life of the torrent.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// One address's hash-failure strikes and when the latest one was recorded.
#[derive(Debug, Clone, Copy)]
struct Strike {
    count: u32,
    last: Instant,
}

/// Contributors of pieces in progress, strikes and bans.
#[derive(Debug, Clone, Default)]
pub(crate) struct HashBans {
    contributors: HashMap<u32, HashSet<IpAddr>>,
    strikes: HashMap<IpAddr, Strike>,
    banned: HashSet<IpAddr>,
}

/// The address bans are keyed on. IPv4-mapped IPv6 (`::ffff:a.b.c.d`) is the
/// IPv4 address, so one host cannot collect two strike records.
pub(crate) fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::from),
    }
}

impl HashBans {
    /// `ip` sent a block of `piece`.
    pub(crate) fn record(&mut self, piece: u32, ip: IpAddr) {
        self.contributors
            .entry(piece)
            .or_default()
            .insert(canonical_ip(ip));
    }

    /// `piece` passed: nobody is blamed for it.
    pub(crate) fn passed(&mut self, piece: u32) {
        self.contributors.remove(&piece);
    }

    /// Drops strikes that have been quiet for `quiet`. Bans stay.
    pub(crate) fn expire(&mut self, now: Instant, quiet: Duration) {
        self.strikes
            .retain(|_, strike| now.saturating_duration_since(strike.last) < quiet);
    }

    /// `piece` failed. Returns the addresses banned by it, sorted.
    pub(crate) fn failed(
        &mut self,
        piece: u32,
        threshold: u32,
        now: Instant,
        quiet: Duration,
    ) -> Vec<IpAddr> {
        self.expire(now, quiet);
        let Some(contributors) = self.contributors.remove(&piece) else {
            return Vec::new();
        };
        let weight = if contributors.len() == 1 { 2 } else { 1 };
        let mut banned = Vec::new();
        for ip in contributors {
            let strike = self.strikes.entry(ip).or_insert(Strike {
                count: 0,
                last: now,
            });
            strike.count = strike.count.saturating_add(weight);
            strike.last = now;
            if threshold > 0 && strike.count >= threshold && self.banned.insert(ip) {
                banned.push(ip);
            }
        }
        banned.sort_unstable();
        banned
    }

    /// Whether `ip` is banned.
    pub(crate) fn is_banned(&self, ip: IpAddr) -> bool {
        self.banned.contains(&canonical_ip(ip))
    }

    /// Number of banned addresses.
    pub(crate) fn count(&self) -> usize {
        self.banned.len()
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, n])
    }

    fn fail(bans: &mut HashBans, piece: u32, threshold: u32) -> Vec<IpAddr> {
        bans.failed(
            piece,
            threshold,
            Instant::now(),
            Duration::from_secs(60 * 60),
        )
    }

    #[test]
    fn a_sole_contributor_is_banned_at_once() {
        let mut bans = HashBans::default();
        bans.record(0, ip(1));
        bans.record(0, ip(1));
        assert_eq!(fail(&mut bans, 0, 2), [ip(1)]);
        assert!(bans.is_banned(ip(1)));
        assert_eq!(bans.count(), 1);
    }

    #[test]
    fn shared_failures_need_more_strikes() {
        let mut bans = HashBans::default();
        bans.record(0, ip(1));
        bans.record(0, ip(2));
        assert!(fail(&mut bans, 0, 2).is_empty());
        bans.record(1, ip(1));
        bans.record(1, ip(3));
        assert_eq!(fail(&mut bans, 1, 2), [ip(1)]);
        assert!(!bans.is_banned(ip(2)) && !bans.is_banned(ip(3)));
    }

    #[test]
    fn passed_pieces_and_threshold_zero() {
        let mut bans = HashBans::default();
        bans.record(0, ip(1));
        bans.passed(0);
        assert!(
            fail(&mut bans, 0, 2).is_empty(),
            "nothing recorded any more"
        );
        bans.record(1, ip(1));
        assert!(fail(&mut bans, 1, 0).is_empty(), "banning disabled");
        assert!(!bans.is_banned(ip(1)));
    }

    #[test]
    fn mapped_ipv6_is_the_ipv4_key() {
        let v4 = Ipv4Addr::new(10, 9, 8, 7);
        let mapped = IpAddr::V6(v4.to_ipv6_mapped());
        let mut bans = HashBans::default();
        bans.record(0, IpAddr::V4(v4));
        bans.record(0, mapped);
        // One host, so the sole-contributor weight applies.
        assert_eq!(fail(&mut bans, 0, 2), [IpAddr::V4(v4)]);
        assert!(bans.is_banned(mapped));
        assert!(bans.is_banned(IpAddr::V4(v4)));
        let other = IpAddr::V6(Ipv6Addr::LOCALHOST);
        assert!(!bans.is_banned(other));
        assert_eq!(canonical_ip(mapped), IpAddr::V4(v4));
        assert_eq!(canonical_ip(other), other);
    }

    #[test]
    fn strikes_expire_after_a_quiet_interval() {
        let t0 = Instant::now();
        let quiet = Duration::from_secs(60);
        let mut bans = HashBans::default();
        bans.record(0, ip(1));
        assert!(bans.failed(0, 4, t0, quiet).is_empty(), "two strikes");
        bans.record(1, ip(1));
        assert!(
            bans.failed(1, 4, t0 + quiet, quiet).is_empty(),
            "the first two expired"
        );
        bans.record(2, ip(1));
        assert_eq!(
            bans.failed(2, 4, t0 + quiet + Duration::from_secs(1), quiet),
            [ip(1)],
            "two fresh strikes plus these two"
        );
        bans.expire(t0 + Duration::from_secs(10_000), quiet);
        assert!(bans.is_banned(ip(1)), "a ban outlives the strikes");
    }
}
