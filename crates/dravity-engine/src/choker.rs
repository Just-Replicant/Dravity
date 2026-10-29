//! Choking: which interested peers we upload to.
//!
//! Every [`Config::unchoke_interval`] a round picks the
//! [`Config::upload_slots`] best interested peers:
//!
//! * **downloading** — the peers that sent us the most data in the last
//!   round (tit-for-tat: reciprocate to those who give);
//! * **seeding** — the peers we uploaded the most to, so the slots go to
//!   whoever can take data fastest.
//!
//! Peers that snub us (hold our requests and deliver nothing) do not get a
//! regular slot while we download. They stay eligible for the optimistic
//! slot, so a stall is not permanent. Ties keep the peers that are already
//! unchoked, so slots do not flap.
//!
//! One more slot is the **optimistic unchoke**: every
//! [`Config::optimistic_unchoke_interval`] it moves to the interested,
//! choked peer that has waited longest for one (a peer that never had one
//! waits from when it connected). That is how newcomers with nothing to
//! offer get their first pieces and how better partners are found. The
//! rotation is deterministic, like libtorrent's, so the engine needs no
//! randomness.

use std::collections::HashSet;
use std::time::Instant;

use crate::types::{Config, PeerKey};

/// What the choker needs to know about one peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub(crate) key: PeerKey,
    /// The peer wants our data.
    pub(crate) interested: bool,
    /// Bytes exchanged in the ranking direction since the last round.
    pub(crate) rate: u64,
    /// The peer is snubbing us; no regular slot.
    pub(crate) snubbed: bool,
    /// We currently upload to the peer.
    pub(crate) unchoked: bool,
    /// When the peer last had the optimistic slot (or connected).
    pub(crate) waiting_since: Instant,
}

/// Round timing and the current optimistic unchoke.
#[derive(Debug, Clone, Default)]
pub(crate) struct Choker {
    next_round: Option<Instant>,
    next_optimistic: Option<Instant>,
    optimistic: Option<PeerKey>,
}

impl Choker {
    /// Whether a round is due.
    pub(crate) fn due(&self, now: Instant) -> bool {
        self.next_round.is_none_or(|t| now >= t)
    }

    /// The peer holding the optimistic slot, if any.
    pub(crate) const fn optimistic(&self) -> Option<PeerKey> {
        self.optimistic
    }

    /// Runs a round and returns the peers to unchoke; everyone else is
    /// choked. The optimistic slot moves when its interval elapsed or its
    /// holder is gone, not interested any more, or now has a regular slot.
    pub(crate) fn round(
        &mut self,
        now: Instant,
        config: &Config,
        candidates: &[Candidate],
    ) -> HashSet<PeerKey> {
        self.next_round = Some(now + config.unchoke_interval);
        let mut ranked: Vec<&Candidate> = candidates
            .iter()
            .filter(|c| c.interested && !c.snubbed)
            .collect();
        ranked.sort_by(|a, b| {
            b.rate
                .cmp(&a.rate)
                .then(b.unchoked.cmp(&a.unchoked))
                .then(a.key.cmp(&b.key))
        });
        let mut selected: HashSet<PeerKey> = ranked
            .iter()
            .take(config.upload_slots)
            .map(|c| c.key)
            .collect();
        if !config.optimistic_unchoke {
            self.optimistic = None;
            return selected;
        }
        // Snubbed peers are skipped for regular slots above, and kept here:
        // the optimistic slot is how a stall gets another chance.
        let eligible: Vec<&Candidate> = candidates
            .iter()
            .filter(|c| c.interested && !selected.contains(&c.key))
            .collect();
        let holder_ok = self
            .optimistic
            .is_some_and(|key| eligible.iter().any(|c| c.key == key));
        let rotate = self.next_optimistic.is_none_or(|t| now >= t);
        if !holder_ok || rotate {
            // Hand the slot to the longest-waiting other peer; a holder that
            // is the only one eligible keeps it.
            let next = eligible
                .iter()
                .filter(|c| Some(c.key) != self.optimistic)
                .min_by(|a, b| {
                    a.waiting_since
                        .cmp(&b.waiting_since)
                        .then(a.key.cmp(&b.key))
                })
                .map(|c| c.key)
                .or(if holder_ok { self.optimistic } else { None });
            self.optimistic = next;
            self.next_optimistic = Some(now + config.optimistic_unchoke_interval);
        }
        if let Some(key) = self.optimistic {
            selected.insert(key);
        }
        selected
    }

    /// Forgets `key` if it held the optimistic slot.
    pub(crate) fn remove(&mut self, key: PeerKey) {
        if self.optimistic == Some(key) {
            self.optimistic = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn config(slots: usize) -> Config {
        Config {
            upload_slots: slots,
            ..Config::default()
        }
    }

    fn cand(key: u64, rate: u64, start: Instant) -> Candidate {
        Candidate {
            key: PeerKey(key),
            interested: true,
            rate,
            snubbed: false,
            unchoked: false,
            waiting_since: start + Duration::from_secs(key),
        }
    }

    fn keys(set: &HashSet<PeerKey>) -> Vec<u64> {
        let mut v: Vec<u64> = set.iter().map(|k| k.0).collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn fastest_peers_win_regular_slots() {
        let now = Instant::now();
        let mut choker = Choker::default();
        let mut c = config(2);
        c.optimistic_unchoke = false;
        let peers = [cand(1, 10, now), cand(2, 50, now), cand(3, 30, now)];
        assert_eq!(keys(&choker.round(now, &c, &peers)), [2, 3]);
        assert!(!choker.due(now + Duration::from_secs(9)));
        assert!(choker.due(now + Duration::from_secs(10)));
    }

    #[test]
    fn uninterested_peers_get_nothing_and_snubbed_peers_stay_optimistic() {
        let now = Instant::now();
        let mut choker = Choker::default();
        let mut peers = [cand(1, 100, now), cand(2, 90, now), cand(3, 1, now)];
        peers[0].interested = false;
        peers[1].snubbed = true;
        let set = choker.round(now, &config(1), &peers);
        // Peer 3 has the regular slot. The snubbed peer still gets the
        // optimistic one; it has waited longer than anyone left over.
        assert_eq!(keys(&set), [2, 3]);
        assert_eq!(choker.optimistic(), Some(PeerKey(2)));
    }

    #[test]
    fn ties_keep_current_uploads() {
        let now = Instant::now();
        let mut choker = Choker::default();
        let mut c = config(1);
        c.optimistic_unchoke = false;
        let mut peers = [cand(1, 0, now), cand(2, 0, now)];
        peers[1].unchoked = true;
        assert_eq!(keys(&choker.round(now, &c, &peers)), [2]);
    }

    #[test]
    fn optimistic_slot_rotates_to_the_longest_waiting_peer() {
        let start = Instant::now();
        let mut choker = Choker::default();
        let c = config(1);
        let peers = [cand(1, 100, start), cand(2, 0, start), cand(3, 0, start)];
        let set = choker.round(start, &c, &peers);
        // Peer 1 earns the regular slot; peer 2 has waited longest.
        assert_eq!(keys(&set), [1, 2]);
        assert_eq!(choker.optimistic(), Some(PeerKey(2)));
        // Kept for the optimistic interval…
        let t = start + Duration::from_secs(10);
        assert_eq!(keys(&choker.round(t, &c, &peers)), [1, 2]);
        // …then handed on.
        let t = start + Duration::from_secs(30);
        let mut later = peers;
        later[1].waiting_since = t;
        assert_eq!(keys(&choker.round(t, &c, &later)), [1, 3]);
        // A holder that leaves frees the slot at the next round.
        choker.remove(PeerKey(3));
        let t = t + Duration::from_secs(10);
        assert_eq!(keys(&choker.round(t, &c, &later[..2])), [1, 2]);
    }

    #[test]
    fn a_lone_peer_keeps_the_optimistic_slot() {
        let now = Instant::now();
        let mut choker = Choker::default();
        let c = config(0);
        let peers = [cand(1, 0, now)];
        assert_eq!(keys(&choker.round(now, &c, &peers)), [1]);
        let later = now + Duration::from_secs(60);
        assert_eq!(keys(&choker.round(later, &c, &peers)), [1]);
    }
}
