//! The BEP 5 routing table, sans-IO: 160 buckets of at most `k` nodes,
//! bucket `i` holding ids whose XOR distance from ours has `i` leading
//! zero bits. Callers pass the time in.

use std::time::{Duration, Instant};

use crate::krpc::Node;

/// Nodes per bucket (BEP 5's `K`).
pub const K: usize = 8;
/// Unanswered queries after which a node may be replaced.
pub const MAX_FAILURES: u8 = 2;
/// A node not heard from for this long is questionable (BEP 5).
pub const QUESTIONABLE_AFTER: Duration = Duration::from_secs(15 * 60);

/// XOR distance between two ids, comparable as big-endian bytes.
#[must_use]
pub fn distance(a: &[u8; 20], b: &[u8; 20]) -> [u8; 20] {
    let mut out = [0u8; 20];
    for ((o, x), y) in out.iter_mut().zip(a).zip(b) {
        *o = x ^ y;
    }
    out
}

/// The bucket `id` belongs in for a table owned by `own`, or `None` for
/// `own` itself.
#[must_use]
pub fn bucket_index(own: &[u8; 20], id: &[u8; 20]) -> Option<usize> {
    let d = distance(own, id);
    let mut zeros = 0usize;
    for byte in d {
        if byte == 0 {
            zeros += 8;
        } else {
            return Some(zeros + byte.leading_zeros() as usize);
        }
    }
    None
}

#[derive(Debug, Clone)]
struct Entry {
    node: Node,
    last_seen: Instant,
    failures: u8,
}

/// What [`RoutingTable::insert`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Insert {
    /// Added (possibly replacing a node that stopped answering).
    Added,
    /// Already known; marked as seen now.
    Refreshed,
    /// Refused: our own id, port 0, or a known id at another address
    /// (an id does not move; a spoofer must not evict the real node).
    Refused,
    /// The bucket is full of good nodes. If one is questionable it is
    /// returned so the caller can ping it and report the result.
    Full(Option<Node>),
}

/// The routing table.
#[derive(Debug, Clone)]
pub struct RoutingTable {
    own: [u8; 20],
    buckets: Vec<Vec<Entry>>,
}

impl RoutingTable {
    /// An empty table for node id `own`.
    #[must_use]
    pub fn new(own: [u8; 20]) -> Self {
        Self {
            own,
            buckets: vec![Vec::new(); 160],
        }
    }

    /// Our node id.
    #[must_use]
    pub const fn own_id(&self) -> &[u8; 20] {
        &self.own
    }

    /// Nodes in the table.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buckets.iter().map(Vec::len).sum()
    }

    /// `true` with no nodes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buckets.iter().all(Vec::is_empty)
    }

    /// Records that `node` answered us (or queried us) at `now`.
    pub fn insert(&mut self, node: Node, now: Instant) -> Insert {
        let Some(index) = bucket_index(&self.own, &node.id) else {
            return Insert::Refused;
        };
        if node.addr.port() == 0 {
            return Insert::Refused;
        }
        let Some(bucket) = self.buckets.get_mut(index) else {
            return Insert::Refused;
        };
        if let Some(entry) = bucket.iter_mut().find(|e| e.node.id == node.id) {
            if entry.node.addr != node.addr {
                return Insert::Refused;
            }
            entry.last_seen = now;
            entry.failures = 0;
            return Insert::Refreshed;
        }
        let entry = Entry {
            node,
            last_seen: now,
            failures: 0,
        };
        if bucket.len() < K {
            bucket.push(entry);
            return Insert::Added;
        }
        if let Some(bad) = bucket.iter_mut().find(|e| e.failures >= MAX_FAILURES) {
            *bad = entry;
            return Insert::Added;
        }
        let questionable = bucket
            .iter()
            .filter(|e| now.saturating_duration_since(e.last_seen) >= QUESTIONABLE_AFTER)
            .min_by_key(|e| e.last_seen)
            .map(|e| e.node);
        Insert::Full(questionable)
    }

    /// Records an unanswered query to `id`.
    pub fn failed(&mut self, id: &[u8; 20]) {
        if let Some(entry) = bucket_index(&self.own, id)
            .and_then(|i| self.buckets.get_mut(i))
            .and_then(|b| b.iter_mut().find(|e| &e.node.id == id))
        {
            entry.failures = entry.failures.saturating_add(1);
        }
    }

    /// The `count` known nodes closest to `target`, closest first; nodes
    /// that stopped answering come last.
    #[must_use]
    pub fn closest(&self, target: &[u8; 20], count: usize) -> Vec<Node> {
        let mut all: Vec<&Entry> = self.buckets.iter().flatten().collect();
        all.sort_by_key(|e| (e.failures >= MAX_FAILURES, distance(&e.node.id, target)));
        all.into_iter().take(count).map(|e| e.node).collect()
    }

    /// One random-looking target per non-empty bucket not refreshed for
    /// [`QUESTIONABLE_AFTER`]: `own` with the bit of the bucket flipped.
    #[must_use]
    pub fn stale_buckets(&self, now: Instant) -> Vec<[u8; 20]> {
        self.buckets
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                !b.is_empty()
                    && b.iter()
                        .all(|e| now.saturating_duration_since(e.last_seen) >= QUESTIONABLE_AFTER)
            })
            .map(|(i, _)| {
                let mut target = self.own;
                if let Some(byte) = target.get_mut(i / 8) {
                    *byte ^= 0x80 >> (i % 8);
                }
                target
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

    use std::net::SocketAddr;

    use super::*;

    fn node(first: u8, last: u8, port: u16) -> Node {
        let mut id = [0u8; 20];
        id[0] = first;
        id[19] = last;
        Node {
            id,
            addr: SocketAddr::from(([10, 0, first, last], port)),
        }
    }

    #[test]
    fn buckets_by_shared_prefix() {
        let own = [0u8; 20];
        assert_eq!(bucket_index(&own, &own), None);
        assert_eq!(bucket_index(&own, &node(0x80, 0, 1).id), Some(0));
        assert_eq!(bucket_index(&own, &node(0x01, 0, 1).id), Some(7));
        assert_eq!(bucket_index(&own, &node(0, 1, 1).id), Some(159));
    }

    #[test]
    fn full_buckets_keep_good_nodes_and_replace_failed_ones() {
        let now = Instant::now();
        let mut t = RoutingTable::new([0u8; 20]);
        for i in 0..8u8 {
            assert_eq!(t.insert(node(0x80, i, 6881), now), Insert::Added);
        }
        assert_eq!(t.insert(node(0x80, 0, 6881), now), Insert::Refreshed);
        // Same id, another address: refused, the real node stays.
        assert_eq!(t.insert(node(0x80, 0, 9999), now), Insert::Refused);
        assert_eq!(t.insert(node(0x80, 200, 6881), now), Insert::Full(None));
        let later = now + QUESTIONABLE_AFTER;
        t.insert(node(0x80, 1, 6881), later);
        let Insert::Full(Some(q)) = t.insert(node(0x80, 200, 6881), later) else {
            panic!("a questionable node is offered");
        };
        assert_ne!(q.id, node(0x80, 1, 6881).id, "the refreshed node is good");
        t.failed(&node(0x80, 3, 6881).id);
        t.failed(&node(0x80, 3, 6881).id);
        assert_eq!(t.insert(node(0x80, 200, 6881), later), Insert::Added);
        assert_eq!(t.len(), K);
        assert!(
            t.closest(&[0u8; 20], 20)
                .iter()
                .all(|n| n.id != node(0x80, 3, 6881).id)
        );
        assert_eq!(t.insert(node(0, 0, 6881), now), Insert::Refused, "own id");
        assert_eq!(t.insert(node(1, 1, 0), now), Insert::Refused, "port 0");
    }

    #[test]
    fn closest_orders_by_xor_distance() {
        let now = Instant::now();
        let mut t = RoutingTable::new([0xff; 20]);
        for first in [0x10, 0x20, 0x30, 0x40] {
            t.insert(node(first, 1, 1), now);
        }
        let mut target = [0u8; 20];
        target[0] = 0x31;
        let ids: Vec<u8> = t.closest(&target, 2).iter().map(|n| n.id[0]).collect();
        assert_eq!(ids, [0x30, 0x20]);
        assert!(t.stale_buckets(now).is_empty());
        assert_eq!(t.stale_buckets(now + QUESTIONABLE_AFTER).len(), 1);
    }
}
