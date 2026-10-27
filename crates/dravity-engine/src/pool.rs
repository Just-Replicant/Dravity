//! Peer addresses waiting to be dialed: a FIFO queue, a retry list with
//! not-before times, and the set of dials in flight.
//!
//! Used by [`TorrentState`](crate::TorrentState); the owner decides *when* each
//! operation is allowed (e.g. no retries once a torrent is finished).

use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::time::Instant;

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
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

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
        assert_eq!(
            pool.next_dial(|_| false),
            Some(addr(1)),
            "explicit add dials now"
        );
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
