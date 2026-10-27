//! Session configuration.

use std::time::Duration;

use dravity_tracker::ClientConfig;

/// Session-wide settings. [`SessionConfig::default`] is a reasonable start.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SessionConfig {
    /// Peer id prefix in Azureus style (`-DV0001-`); the rest is random.
    pub peer_id_prefix: Vec<u8>,
    /// Port reported to trackers. Phase 1 does not accept incoming
    /// connections, so this is informational.
    pub announce_port: u16,
    /// Per-torrent engine tunables.
    pub engine: dravity_engine::Config,
    /// Tracker client settings.
    pub tracker: ClientConfig,
    /// Peers requested per announce.
    pub num_want: u32,
    /// Timeout for TCP connect plus handshake.
    pub connect_timeout: Duration,
    /// Engine timer period.
    pub tick_interval: Duration,
    /// Messages queued towards one peer before it is dropped as too slow.
    pub peer_queue: usize,
    /// Capacity of each torrent's event queue (peer, disk, hash, tracker).
    pub event_queue: usize,
    /// Concurrent piece-hash jobs per torrent.
    pub hash_workers: usize,
    /// Time allowed for the `stopped` announce when a torrent stops.
    pub stop_announce_timeout: Duration,
    /// Broadcast buffer for alerts; slow subscribers miss older alerts.
    pub alert_buffer: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            peer_id_prefix: b"-DV0001-".to_vec(),
            announce_port: 6881,
            engine: dravity_engine::Config::default(),
            tracker: ClientConfig::default(),
            num_want: 80,
            connect_timeout: Duration::from_secs(10),
            tick_interval: Duration::from_secs(1),
            peer_queue: 256,
            event_queue: 1024,
            hash_workers: std::thread::available_parallelism().map_or(2, usize::from),
            stop_announce_timeout: Duration::from_secs(5),
            alert_buffer: 1024,
        }
    }
}
