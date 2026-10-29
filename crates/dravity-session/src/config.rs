//! Session configuration.

use std::path::PathBuf;
use std::time::Duration;

use dravity_dht::DhtConfig;
use dravity_portmap::PortMapConfig;
use dravity_tracker::ClientConfig;

use crate::listen::ListenConfig;

/// Session-wide settings. [`SessionConfig::default`] is a reasonable start.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SessionConfig {
    /// Peer id prefix in Azureus style (`-DV0001-`); the rest is random.
    pub peer_id_prefix: Vec<u8>,
    /// Accept incoming peer connections (TCP, and uTP unless
    /// [`ListenConfig::utp`] is `None`); `None` only dials out over TCP.
    pub listen: Option<ListenConfig>,
    /// Dial peers over uTP first and fall back to TCP. By default TCP is
    /// tried first and uTP only when TCP fails (peers behind NATs that pass
    /// only UDP). Needs uTP sockets, so a listener with uTP.
    pub prefer_utp: bool,
    /// Ask the router (NAT-PMP, then UPnP IGD) to forward the listening
    /// port, TCP and (with uTP) UDP, so peers behind no NAT of their own
    /// can reach us. Mappings are removed on [`crate::Session::shutdown`].
    /// `None`, or no listener, disables it.
    pub port_mapping: Option<PortMapConfig>,
    /// Port reported to trackers when the session does not listen (for
    /// example behind a proxy that forwards to us). With a listener its
    /// bound port is reported instead.
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
    /// Run a DHT node (BEP 5) for peer discovery; `None` disables the DHT.
    /// Private torrents never use it.
    pub dht: Option<DhtConfig>,
    /// Time allowed for one DHT lookup.
    pub dht_lookup_timeout: Duration,
    /// Directory for fastresume files (`<info-hash>.resume`). When set, a
    /// torrent saves its verified pieces on stop and, on the next start,
    /// skips the full check if no file changed since. `None` disables it.
    pub resume_dir: Option<PathBuf>,
    /// Directory for session state ([`crate::persist`]): the torrents of
    /// the session, their directories, file priorities and paused flags, so
    /// [`crate::Session::restore`] can bring them back after a restart.
    /// `None` keeps nothing.
    pub state_dir: Option<PathBuf>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            peer_id_prefix: b"-DV0001-".to_vec(),
            listen: Some(ListenConfig::default()),
            prefer_utp: false,
            port_mapping: Some(PortMapConfig::default()),
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
            dht: Some(DhtConfig::default()),
            dht_lookup_timeout: Duration::from_secs(30),
            resume_dir: None,
            state_dir: None,
        }
    }
}
