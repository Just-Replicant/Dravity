//! Peer addresses waiting to be dialed: a FIFO queue, a retry list with
//! not-before times, and the set of dials in flight.
//!
//! Shared by [`TorrentState`](crate::TorrentState) and
//! [`MetadataState`](crate::MetadataState); the owner decides *when* each
//! operation is allowed. A seeding torrent retries; one that left the swarm
//! does not.

use std::collections::{HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use crate::web_seed::is_web_seed_addr;

/// How long an address waits after a dial was deferred by the session's
/// connection cap ([`Event::PeerConnectDeferred`](crate::Event::PeerConnectDeferred)).
pub const DEFERRED_DIAL_RETRY: Duration = Duration::from_secs(5);

/// Upper bound on queued plus waiting peer addresses.
pub(crate) const MAX_POOL: usize = 2_000;

#[derive(Debug, Clone, Default)]
pub(crate) struct PeerPool {
    queue: VecDeque<SocketAddr>,
    /// `(not_before, addr)` for peers to dial again after a drop or failure.
    retry: VecDeque<(Instant, SocketAddr)>,
    connecting: HashSet<SocketAddr>,
}

impl PeerPool {
    /// Queued plus waiting addresses.
    pub(crate) fn known(&self) -> usize {
        self.queue.len() + self.retry.len()
    }

    pub(crate) fn connecting(&self) -> usize {
        self.connecting.len()
    }

    /// Queues new addresses. Already connected, connecting, or queued
    /// addresses and port 0 are skipped. An explicit add of an address that
    /// waits for a retry dials it now instead.
    pub(crate) fn add(
        &mut self,
        peers: impl IntoIterator<Item = SocketAddr>,
        connected: impl Fn(SocketAddr) -> bool,
    ) {
        let mut queued: HashSet<SocketAddr> = self.queue.iter().copied().collect();
        for addr in peers {
            if self.queue.len() >= MAX_POOL {
                break;
            }
            // Only drop the retry entry once we have room to queue it.
            self.retry.retain(|(_, waiting)| *waiting != addr);
            if addr.port() == 0
                || is_web_seed_addr(addr)
                || connected(addr)
                || self.connecting.contains(&addr)
                || !queued.insert(addr)
            {
                continue;
            }
            self.queue.push_back(addr);
        }
    }

    /// Remembers `addr` to dial again at `when`. Ignored when the address is
    /// already connected, connecting, queued, or waiting.
    pub(crate) fn schedule_retry(
        &mut self,
        when: Instant,
        addr: SocketAddr,
        connected: impl Fn(SocketAddr) -> bool,
    ) {
        if addr.port() == 0
            || is_web_seed_addr(addr)
            || connected(addr)
            || self.connecting.contains(&addr)
            || self.queue.contains(&addr)
            || self.retry.iter().any(|(_, waiting)| *waiting == addr)
            || self.known() >= MAX_POOL
        {
            return;
        }
        self.retry.push_back((when, addr));
    }

    /// Forgets queued and waiting addresses of `ip` (a banned peer),
    /// including the IPv4-mapped form of the same host. Dials in flight
    /// finish and are refused when they connect.
    pub(crate) fn remove_ip(&mut self, ip: IpAddr) {
        let ip = crate::ban::canonical_ip(ip);
        self.queue
            .retain(|a| crate::ban::canonical_ip(a.ip()) != ip);
        self.retry
            .retain(|(_, a)| crate::ban::canonical_ip(a.ip()) != ip);
    }

    /// Moves due retries onto the dial queue.
    pub(crate) fn promote(&mut self, now: Instant, connected: impl Fn(SocketAddr) -> bool) {
        if self.queue.len() >= MAX_POOL {
            return;
        }
        let room = MAX_POOL - self.queue.len();
        let mut due = Vec::new();
        self.retry.retain(|(when, addr)| {
            if *when <= now && due.len() < room {
                due.push(*addr);
                false
            } else {
                true
            }
        });
        if !due.is_empty() {
            self.add(due, connected);
        }
    }

    /// Pops the next address to dial and marks it connecting.
    pub(crate) fn next_dial(
        &mut self,
        connected: impl Fn(SocketAddr) -> bool,
    ) -> Option<SocketAddr> {
        while let Some(addr) = self.queue.pop_front() {
            if !connected(addr) && self.connecting.insert(addr) {
                return Some(addr);
            }
        }
        None
    }

    /// Marks `addr` connecting for a dial outside the queue (a holepunch);
    /// `false` if it is connected or already being dialed.
    pub(crate) fn dial_now(
        &mut self,
        addr: SocketAddr,
        connected: impl Fn(SocketAddr) -> bool,
    ) -> bool {
        if addr.port() == 0 || is_web_seed_addr(addr) || connected(addr) {
            return false;
        }
        self.queue.retain(|a| *a != addr);
        self.retry.retain(|(_, a)| *a != addr);
        self.connecting.insert(addr)
    }

    /// A dial finished (either way).
    pub(crate) fn dial_done(&mut self, addr: SocketAddr) {
        self.connecting.remove(&addr);
    }

    /// Forgets queued and waiting addresses (dials in flight stay tracked).
    pub(crate) fn clear_known(&mut self) {
        self.queue.clear();
        self.retry.clear();
    }

    /// Forgets everything.
    pub(crate) fn clear(&mut self) {
        self.clear_known();
        self.connecting.clear();
    }

    /// Every queued or waiting address, for handing over to another state.
    pub(crate) fn drain_known(&mut self) -> Vec<SocketAddr> {
        let mut out: Vec<SocketAddr> = self.queue.drain(..).collect();
        out.extend(self.retry.drain(..).map(|(_, addr)| addr));
        out
    }

    /// Queued and waiting addresses, plus dials already in flight.
    ///
    /// In-flight dials stay recorded so the next state can adopt them and
    /// does not open a second connection to the same address.
    pub(crate) fn drain_handover(&mut self) -> (Vec<SocketAddr>, Vec<SocketAddr>) {
        let known = self.drain_known();
        let mut connecting: Vec<SocketAddr> = self.connecting.iter().copied().collect();
        connecting.sort_unstable();
        (known, connecting)
    }

    /// Marks addresses whose TCP connect is already running.
    pub(crate) fn adopt_connecting(&mut self, addrs: impl IntoIterator<Item = SocketAddr>) {
        for addr in addrs {
            if addr.port() != 0 {
                self.connecting.insert(addr);
            }
        }
    }
}

/// Which of two connections between the same two endpoints (one dialed by
/// each side, as happens over uTP, where both use their listen port) both
/// sides keep: the one opened by the side with the lower listen port, the
/// rule libtorrent applies. Keeping the first one instead lets each side
/// close a different connection, leaving none.
///
/// Returns whether the newly joined connection replaces the existing one;
/// `None` when the rule cannot decide (our port unknown, or equal ports),
/// in which case the existing connection stays.
pub(crate) fn new_duplicate_wins(
    our_port: Option<u16>,
    their_port: u16,
    new_is_incoming: bool,
) -> Option<bool> {
    let our_port = our_port.filter(|&p| p != 0 && p != their_port)?;
    let keep_ours = our_port < their_port;
    Some(keep_ours != new_is_incoming)
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_lower_listen_port_keeps_its_outgoing_connection() {
        // We listen on 6000, they on 7000: our dial wins on both sides.
        assert_eq!(new_duplicate_wins(Some(6000), 7000, false), Some(true));
        assert_eq!(new_duplicate_wins(Some(6000), 7000, true), Some(false));
        // We listen on 7000, they on 6000: their dial wins.
        assert_eq!(new_duplicate_wins(Some(7000), 6000, true), Some(true));
        assert_eq!(new_duplicate_wins(Some(7000), 6000, false), Some(false));
        // Undecidable: the first connection stays.
        assert_eq!(new_duplicate_wins(None, 6000, true), None);
        assert_eq!(new_duplicate_wins(Some(6000), 6000, true), None);
    }

    fn addr(n: u16) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, 1], n))
    }

    #[test]
    fn dedups_and_retries() {
        let now = Instant::now();
        let mut pool = PeerPool::default();
        pool.add([addr(1), addr(1), addr(0), addr(2)], |a| a == addr(2));
        assert_eq!(pool.known(), 1);
        assert_eq!(pool.next_dial(|_| false), Some(addr(1)));
        assert_eq!(pool.connecting(), 1);
        pool.add([addr(1)], |_| false);
        assert_eq!(pool.known(), 0, "connecting addresses are not queued");
        pool.dial_done(addr(1));
        pool.schedule_retry(now + Duration::from_secs(5), addr(1), |_| false);
        pool.schedule_retry(now, addr(1), |_| false);
        assert_eq!(pool.known(), 1);
        pool.promote(now, |_| false);
        assert_eq!(pool.next_dial(|_| false), None, "not due yet");
        pool.promote(now + Duration::from_secs(5), |_| false);
        assert_eq!(pool.next_dial(|_| false), Some(addr(1)));
        pool.dial_done(addr(1));
        pool.schedule_retry(now + Duration::from_secs(60), addr(1), |_| false);
        pool.add([addr(1)], |_| false);
        assert_eq!(pool.drain_known(), [addr(1)], "explicit add dials now");
        assert_eq!(pool.known(), 0, "drained");
    }

    #[test]
    fn handover_keeps_dials_in_flight() {
        let mut pool = PeerPool::default();
        pool.add([addr(1), addr(2)], |_| false);
        assert_eq!(pool.next_dial(|_| false), Some(addr(1)));
        let (known, connecting) = pool.drain_handover();
        assert_eq!(known, [addr(2)]);
        assert_eq!(connecting, [addr(1)]);
        let mut next = PeerPool::default();
        next.adopt_connecting(connecting);
        assert_eq!(next.connecting(), 1);
        assert_eq!(next.next_dial(|_| false), None, "already connecting");
        next.dial_done(addr(1));
        assert_eq!(next.connecting(), 0);
    }

    #[test]
    fn remove_ip_forgets_the_mapped_form() {
        let mut pool = PeerPool::default();
        let v4 = Ipv4Addr::new(10, 1, 2, 3);
        let plain = SocketAddr::from((v4, 1));
        let mapped = SocketAddr::new(IpAddr::V6(v4.to_ipv6_mapped()), 2);
        pool.add([plain, mapped], |_| false);
        pool.schedule_retry(Instant::now(), SocketAddr::from((v4, 9)), |_| false);
        pool.remove_ip(IpAddr::V6(v4.to_ipv6_mapped()));
        assert_eq!(pool.known(), 0);
    }

    #[test]
    fn bounded() {
        let mut pool = PeerPool::default();
        pool.add((1..=3000).map(addr), |_| false);
        assert_eq!(pool.known(), MAX_POOL);
        pool.schedule_retry(Instant::now(), addr(4000), |_| false);
        assert_eq!(pool.known(), MAX_POOL);
    }
}
