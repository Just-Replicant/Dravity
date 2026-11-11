//! Session configuration.

use std::path::PathBuf;
use std::time::Duration;

use dravity_dht::DhtConfig;
use dravity_mse::Policy;
use dravity_portmap::PortMapConfig;
use dravity_storage::{Durability, PreallocateMode};
use dravity_tracker::ClientConfig;
use dravity_webseed::WebSeedConfig;

use crate::listen::ListenConfig;

/// How torrent data reaches the disk.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StorageBackend {
    /// `pread`/`pwrite` on the session's blocking thread pool.
    #[default]
    Blocking,
    /// io_uring through `tokio-uring` on a dedicated thread (Linux, crate
    /// feature `uring`). Falls back to [`Blocking`](Self::Blocking), with a
    /// warning, when the feature is off or the kernel or a container
    /// refuses io_uring; [`Session::storage_backend`](crate::Session::storage_backend)
    /// reports the one in use.
    IoUring,
}

impl StorageBackend {
    /// Short name (`blocking`, `io_uring`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blocking => "blocking",
            Self::IoUring => "io_uring",
        }
    }
}

impl std::fmt::Display for StorageBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Default [`SessionConfig::max_connections`]: well under the usual
/// 1024 file descriptor limit, with room for files and other sockets.
pub const DEFAULT_MAX_CONNECTIONS: usize = 500;

/// Default [`SessionConfig::read_cache`]: 64 MiB.
pub const DEFAULT_READ_CACHE: usize = 64 * 1024 * 1024;

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
    /// Open peer connections across every torrent, dialed or accepted,
    /// TCP or uTP (each torrent also keeps its own
    /// [`max_peers`](dravity_engine::Config::max_peers)). At the cap the
    /// listener closes new connections and dials wait. `None` removes the
    /// cap.
    pub max_connections: Option<usize>,
    /// Concurrent piece-hash jobs across the whole session (piece checks,
    /// rechecks and smart-ban block digests of every torrent share them).
    pub hash_workers: usize,
    /// Verify downloaded pieces from their blocks as they are written
    /// (incremental SHA-1, per-block SHA-256 leaves) instead of reading
    /// each finished piece back from storage. Checks of existing data, and
    /// pieces whose blocks arrived twice or too far out of order, are
    /// still read back.
    pub hash_on_arrival: bool,
    /// Bytes of recently read torrent data kept in memory, shared by every
    /// torrent whose storage the session creates, so seeding a popular
    /// piece to many peers reads it from disk once
    /// ([`dravity_storage::CachedStorage`]). `None` turns the cache off.
    pub read_cache: Option<usize>,
    /// How files the session creates reserve their space. `Full` fails a
    /// full disk early and fragments less, but makes every file look
    /// present to the startup prescan, so a fresh full-preallocated
    /// torrent is rechecked once. Default: `None`.
    pub preallocate: PreallocateMode,
    /// When written data is forced to disk. Default: [`Durability::Never`]
    /// (sync on resume save, descriptor eviction and stop only).
    pub durability: Durability,
    /// Session-wide download limit in bytes per second, shared by every
    /// torrent on top of each torrent's own limit. Changeable live
    /// ([`crate::Session::apply_settings`]).
    pub download_rate_limit: Option<u64>,
    /// Session-wide upload limit in bytes per second.
    pub upload_rate_limit: Option<u64>,
    /// Torrents downloading at once; later ones wait in the queue.
    /// `None` (the default) is unlimited.
    pub max_active_downloads: Option<usize>,
    /// Torrents seeding at once; later ones wait in the queue.
    pub max_active_seeds: Option<usize>,
    /// Address ranges whose peers are refused and never dialed
    /// ([`crate::IpFilter`]); replace it at runtime with
    /// [`crate::Session::set_ip_filter`].
    pub ip_filter: Option<std::sync::Arc<crate::IpFilter>>,
    /// Dial peers through this proxy (TCP only: uTP is skipped while a
    /// proxy is set). An HTTP proxy also carries HTTP(S) tracker and web
    /// seed requests. UDP traffic (DHT, UDP trackers, LSD) and incoming
    /// connections are not proxied. `None` (the default) connects directly.
    pub proxy: Option<crate::proxy::Proxy>,
    /// DSCP value (0 to [`crate::MAX_DSCP`]) marked on peer TCP connections,
    /// e.g. 8 (CS1, "lower effort") so home routers deprioritise
    /// BitTorrent. `None` (the default) leaves the system default.
    pub peer_dscp: Option<u8>,
    /// Country database for [`crate::PeerStatus::country`]
    /// ([`crate::GeoIp`]). `None` (the default) reports no countries.
    pub geoip: Option<std::sync::Arc<crate::GeoIp>>,
    /// Local Service Discovery (BEP 14): find peers of the same torrents on
    /// the local network by multicast. Needs a listener; private torrents
    /// never use it. `None` (the default) turns it off.
    pub lsd: Option<dravity_lsd::LsdConfig>,
    /// Scrape every tracker this often (BEP 48 / BEP 15) and show the
    /// counts in [`crate::TrackerStatus::scrape`]. `None` (the default)
    /// scrapes only on [`crate::TorrentHandle::scrape`].
    pub scrape_interval: Option<Duration>,
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
    /// HTTP settings for web seeds (BEP 19: a torrent's `url-list`, a
    /// magnet link's `ws`). `None` never contacts web seeds.
    pub web_seed: Option<WebSeedConfig>,
    /// Wait this long before trying a web seed again after it failed;
    /// doubles with every failure in a row, up to 32 times as long.
    pub web_seed_retry: Duration,
    /// How file data is read and written for torrents whose storage the
    /// session creates (not for [`AddTorrentOptions::with_storage`](crate::AddTorrentOptions::with_storage)).
    pub storage_backend: StorageBackend,
    /// Message Stream Encryption (MSE/PE) for peer connections over TCP and
    /// uTP. [`Enabled`](Policy::Enabled) (the default) accepts encrypted
    /// and plain incoming connections and dials encrypted first, redialing
    /// in plaintext if the peer does not speak MSE;
    /// [`Required`](Policy::Required) only makes and accepts RC4-encrypted
    /// connections; [`Disabled`](Policy::Disabled) never encrypts. MSE is
    /// obfuscation against traffic shaping, not authenticated security.
    pub encryption: Policy,
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
            max_connections: Some(DEFAULT_MAX_CONNECTIONS),
            hash_workers: std::thread::available_parallelism().map_or(2, usize::from),
            hash_on_arrival: true,
            read_cache: Some(DEFAULT_READ_CACHE),
            preallocate: PreallocateMode::None,
            durability: Durability::Never,
            download_rate_limit: None,
            upload_rate_limit: None,
            max_active_downloads: None,
            max_active_seeds: None,
            ip_filter: None,
            proxy: None,
            peer_dscp: None,
            geoip: None,
            lsd: None,
            scrape_interval: None,
            stop_announce_timeout: Duration::from_secs(5),
            alert_buffer: 1024,
            dht: Some(DhtConfig::default()),
            dht_lookup_timeout: Duration::from_secs(30),
            resume_dir: None,
            state_dir: None,
            web_seed: Some(WebSeedConfig::default()),
            web_seed_retry: Duration::from_secs(30),
            storage_backend: StorageBackend::Blocking,
            encryption: Policy::Enabled,
        }
    }
}
