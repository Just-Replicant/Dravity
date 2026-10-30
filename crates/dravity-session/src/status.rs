//! Read-only views of a running torrent's swarm: its peers
//! ([`TorrentHandle::peers`](crate::TorrentHandle::peers)) and its trackers
//! ([`TorrentHandle::trackers`](crate::TorrentHandle::trackers)).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use dravity_engine::PeerInfo;

/// The transport under a peer connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Transport {
    /// TCP.
    Tcp,
    /// uTP (BEP 29).
    Utp,
}

impl Transport {
    /// Short lowercase name (`tcp`, `utp`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Utp => "utp",
        }
    }
}

/// One connected peer: what the engine knows, plus how it is connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PeerStatus {
    /// Address, pieces, byte totals and choke/interest flags.
    pub info: PeerInfo,
    /// TCP or uTP.
    pub transport: Transport,
}

/// The last answer one tracker gave.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TrackerOutcome {
    /// The tracker answered.
    Ok {
        /// Requested re-announce interval, if the tracker sent one.
        interval: Option<Duration>,
        /// Seeders, if reported.
        seeders: Option<u32>,
        /// Leechers, if reported.
        leechers: Option<u32>,
        /// Peers in the answer.
        peers: usize,
    },
    /// The announce failed.
    Failed {
        /// Cause.
        error: String,
    },
}

/// One tracker of a torrent and the outcome of its last announce.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TrackerStatus {
    /// Announce URL.
    pub url: String,
    /// BEP 12 tier, from 0.
    pub tier: usize,
    /// The last outcome; `None` until the tracker was asked (a tier stops at
    /// the first tracker that answers, so the others may never be).
    pub last: Option<TrackerOutcome>,
    /// When `last` was recorded.
    pub updated: Option<Instant>,
}

/// The last outcome per tracker URL, kept by the torrent's owner task.
#[derive(Debug, Default)]
pub(crate) struct TrackerLog {
    last: HashMap<String, (Instant, TrackerOutcome)>,
}

impl TrackerLog {
    /// Records announce outcomes that arrived at `now`.
    pub(crate) fn record(
        &mut self,
        now: Instant,
        outcomes: impl IntoIterator<Item = (String, TrackerOutcome)>,
    ) {
        for (url, outcome) in outcomes {
            self.last.insert(url, (now, outcome));
        }
    }

    /// Every tracker of `tiers`, in announce order, with its last outcome.
    pub(crate) fn list(&self, tiers: &[Vec<String>]) -> Vec<TrackerStatus> {
        tiers
            .iter()
            .enumerate()
            .flat_map(|(tier, urls)| {
                urls.iter().map(move |url| {
                    let last = self.last.get(url);
                    TrackerStatus {
                        url: url.clone(),
                        tier,
                        last: last.map(|(_, outcome)| outcome.clone()),
                        updated: last.map(|&(at, _)| at),
                    }
                })
            })
            .collect()
    }
}

/// Joins the engine's peer list with the transports the owner knows.
pub(crate) fn peers(
    infos: Vec<PeerInfo>,
    transport: impl Fn(&PeerInfo) -> Transport,
) -> Vec<PeerStatus> {
    infos
        .into_iter()
        .map(|info| PeerStatus {
            transport: transport(&info),
            info,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{TrackerLog, TrackerOutcome};

    #[test]
    fn trackers_are_listed_in_tier_order_with_their_last_outcome() {
        let mut log = TrackerLog::default();
        let t0 = Instant::now();
        log.record(
            t0,
            [(
                "http://b".to_owned(),
                TrackerOutcome::Failed {
                    error: "refused".to_owned(),
                },
            )],
        );
        let t1 = t0 + Duration::from_secs(1);
        let ok = TrackerOutcome::Ok {
            interval: Some(Duration::from_secs(1800)),
            seeders: Some(3),
            leechers: Some(4),
            peers: 7,
        };
        log.record(t1, [("http://b".to_owned(), ok.clone())]);
        let tiers = vec![
            vec!["http://b".to_owned(), "http://a".to_owned()],
            vec!["udp://c".to_owned()],
        ];
        let list = log.list(&tiers);
        assert_eq!(list.len(), 3);
        let urls: Vec<_> = list.iter().map(|t| (t.url.as_str(), t.tier)).collect();
        assert_eq!(urls, [("http://b", 0), ("http://a", 0), ("udp://c", 1)]);
        assert_eq!(
            list[0].last,
            Some(ok),
            "the newest outcome replaces the old"
        );
        assert_eq!(list[0].updated, Some(t1));
        assert_eq!(list[1].last, None, "not asked yet");
        assert_eq!(list[1].updated, None);
    }

    #[test]
    fn no_tiers_means_no_trackers() {
        assert!(TrackerLog::default().list(&[]).is_empty());
    }
}
