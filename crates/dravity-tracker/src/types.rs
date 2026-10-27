//! Announce request and response types shared by HTTP and UDP.

use std::net::SocketAddr;
use std::time::Duration;

use dravity_core::{InfoHash, PeerId};

/// The `event` of an announce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum AnnounceEvent {
    /// Periodic announce (no event).
    #[default]
    None,
    /// The download started.
    Started,
    /// The download completed.
    Completed,
    /// The client is stopping the torrent.
    Stopped,
}

impl AnnounceEvent {
    /// The HTTP `event` query value, if any.
    #[must_use]
    pub const fn as_http(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Started => Some("started"),
            Self::Completed => Some("completed"),
            Self::Stopped => Some("stopped"),
        }
    }

    /// The BEP 15 event code.
    #[must_use]
    pub const fn as_udp(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Completed => 1,
            Self::Started => 2,
            Self::Stopped => 3,
        }
    }
}

/// Parameters of an announce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnounceRequest {
    /// Torrent info-hash.
    pub info_hash: InfoHash,
    /// Our peer id.
    pub peer_id: PeerId,
    /// Port we accept peer connections on.
    pub port: u16,
    /// Payload bytes uploaded so far.
    pub uploaded: u64,
    /// Payload bytes downloaded so far.
    pub downloaded: u64,
    /// Bytes still missing.
    pub left: u64,
    /// Announce event.
    pub event: AnnounceEvent,
    /// Number of peers wanted (`None` lets the tracker decide).
    pub num_want: Option<u32>,
    /// Random key identifying this client across IP changes.
    pub key: u32,
}

/// A successful announce.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AnnounceResponse {
    /// Requested re-announce interval.
    pub interval: Option<Duration>,
    /// Minimum re-announce interval.
    pub min_interval: Option<Duration>,
    /// Peers (IPv4 and IPv6).
    pub peers: Vec<SocketAddr>,
    /// Number of seeders, if reported.
    pub seeders: Option<u32>,
    /// Number of leechers, if reported.
    pub leechers: Option<u32>,
    /// Non-fatal tracker warning.
    pub warning: Option<String>,
}
