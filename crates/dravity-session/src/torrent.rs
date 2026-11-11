//! The per-torrent owner task: the only caller of the engine's `update`.
//!
//! ```text
//! loop {
//!     input  = select(commands, peer/disk/hash/tracker/DHT events, timer)
//!     effects = phase.update(now, input)
//!     dispatch(effects)        // queue work; never wait for it here
//! }
//! ```
//!
//! Dispatch never awaits a bounded channel the owner itself must drain:
//! peer queues use `try_send` (a full queue drops the slow peer) and disk,
//! hash, tracker and DHT work runs in spawned tasks that report back as
//! events.
//!
//! A magnet link starts in the metadata phase ([`MetadataState`]). Once the
//! `info` dictionary is verified the task builds the storage and a
//! [`TorrentState`], and replays the open connections into it as ordinary
//! events, so no peer is reconnected.
//!
//! Connections accepted by the session's listener arrive as
//! [`Command::Incoming`], already handshaken; they become
//! [`Event::PeerAccepted`] and then run like dialed peers. Blocks peers
//! request are read from storage on the blocking pool
//! ([`Effect::ReadBlock`]) and reported back as [`Event::BlockRead`].

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use bytes::{Bytes, BytesMut};
use dravity_core::admit::{ConnectionLimit, ConnectionSlot};
use dravity_core::merkle::Hash;
use dravity_core::verify::PieceHasher;
use dravity_core::{BlockRef, Info, InfoHash, InfoHashV2, MetaVersion, Metainfo, PeerId};
use dravity_dht::Dht;
use dravity_engine::{
    Alert, AnnounceEvent, Bitfield, Config, DialHints, Effect, Event, MetadataState, PeerKey,
    PeerLink, Priority, Stats, TorrentState, TorrentStatus,
};
use dravity_storage::{
    CachedStorage, Durability, FileIo, FileStorage, ReadCache, ResumeData, Storage, block_hashes,
    load_resume, read_block, read_blocks, save_resume, verify_piece, write_block, write_blocks,
};
use dravity_tracker::{AnnounceRequest, AnnounceResponse, TrackerClient};
use dravity_webseed::WebSeedClient;
use dravity_wire::{Message, Reserved};
use futures_util::StreamExt;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinSet;

use crate::catalog::{Catalog, EntrySource};
use crate::config::SessionConfig;
use crate::files::{self, FileProgress};
use crate::hash_pool::HashPool;
use crate::listen::{self, Registry, UtpSockets};
use crate::peer::{self, PeerCommand, PeerParams};
use crate::persist::Persister;
use crate::status::{
    self, PeerStatus, ScrapeOutcome, TrackerLog, TrackerOutcome, TrackerStatus, Transport,
};
use crate::stream::PeerStream;
use crate::web_seed::{self, WebSeedParams};
use crate::{EncryptionPolicy, SessionAlert};

/// Tracker tiers (BEP 12), shared with announce tasks so a tracker that
/// answered moves to the front of its tier.
pub(crate) type Tiers = Arc<Mutex<Vec<Vec<String>>>>;

/// Everything that can wake the owner task besides commands and the timer.
#[derive(Debug)]
pub(crate) enum Input {
    Connected {
        key: PeerKey,
        addr: SocketAddr,
        reserved: Reserved,
        transport: Transport,
        /// RC4-encrypted (MSE).
        encrypted: bool,
        /// What the dial showed about MSE support (see [`PeerLink::mse`]).
        mse: Option<bool>,
    },
    ConnectFailed {
        key: PeerKey,
        addr: SocketAddr,
        retry: bool,
    },
    Disconnected {
        key: PeerKey,
    },
    /// A web seed resolved its host and serves the torrent (it then sends
    /// its bitfield and an unchoke like any peer).
    WebSeedReady {
        key: PeerKey,
    },
    /// A web seed could not start. `retry` is false when the URL can never
    /// be fetched (forbidden address, bad scheme); a DNS failure retries.
    WebSeedFailed {
        key: PeerKey,
        /// Whether the owner should try the URL again after its back-off.
        retry: bool,
    },
    Message {
        key: PeerKey,
        message: Message,
    },
    Engine(Event),
    /// An announce finished: the engine's event and what each tracker said.
    Announced {
        event: Event,
        outcomes: Vec<(String, TrackerOutcome)>,
    },
    /// A scrape finished; `reply` waits for the tracker list if a handle
    /// asked for it.
    Scraped {
        outcomes: Vec<(String, ScrapeOutcome)>,
        reply: Option<oneshot::Sender<Vec<TrackerStatus>>>,
    },
}

/// A connection the listener accepted and handshook for this torrent.
#[derive(Debug)]
pub(crate) struct Accepted {
    pub(crate) stream: PeerStream,
    pub(crate) addr: SocketAddr,
    /// The remote handshake's reserved bits.
    pub(crate) reserved: Reserved,
    /// Bytes that arrived after the handshake.
    pub(crate) leftover: BytesMut,
    /// The connection's slot under the session-wide cap.
    pub(crate) slot: ConnectionSlot,
}

/// A torrent's content, once its metadata is known.
#[derive(Debug, Clone)]
pub(crate) struct ContentRef {
    pub(crate) metainfo: Arc<Metainfo>,
    pub(crate) storage: Arc<dyn Storage>,
}

/// A progress snapshot with per-file detail.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TorrentProgress {
    /// Torrent-wide numbers.
    pub stats: Stats,
    /// Per-file progress and priority; `None` until the metadata of a
    /// magnet link is known.
    pub files: Option<Vec<FileProgress>>,
}

/// Where the session accepts peers after a rebind.
#[derive(Debug, Clone)]
pub(crate) struct Relisten {
    pub(crate) port: Option<u16>,
    pub(crate) utp: UtpSockets,
    pub(crate) port_map: Option<tokio::sync::watch::Receiver<dravity_portmap::Status>>,
}

/// Commands from [`TorrentHandle`](crate::TorrentHandle), streams and the
/// listener.
#[derive(Debug)]
pub(crate) enum Command {
    Stats(oneshot::Sender<Stats>),
    Progress(oneshot::Sender<TorrentProgress>),
    AddPeers(Vec<SocketAddr>),
    Peers(oneshot::Sender<Vec<PeerStatus>>),
    Trackers(oneshot::Sender<Vec<TrackerStatus>>),
    /// Scrape every tracker now, then reply with the tracker list.
    Scrape(oneshot::Sender<Vec<TrackerStatus>>),
    Incoming(Accepted),
    /// One priority per file (already validated against the metadata, or
    /// kept until it arrives).
    SetFilePriorities(Vec<Priority>),
    Content(oneshot::Sender<Option<ContentRef>>),
    /// Answer once `piece` is verified (dropped if the torrent stops).
    WaitPiece {
        piece: u32,
        reply: oneshot::Sender<()>,
    },
    /// The session moved its listener ([`crate::Session::set_listen_port`]).
    Relisten(Relisten),
    /// A stream's urgent window, replacing its previous one.
    Stream {
        id: u64,
        pieces: Vec<u32>,
    },
    CloseStream(u64),
    Stop(oneshot::Sender<()>),
    /// An address was banned elsewhere in the session.
    BanIp(IpAddr),
    /// The IP filter changed: close connections it now blocks.
    Refilter,
}

/// Shared, immutable inputs of a torrent task.
#[derive(Debug, Clone)]
pub(crate) struct TorrentContext {
    pub(crate) info_hash: InfoHash,
    pub(crate) trackers: Tiers,
    pub(crate) tracker: TrackerClient,
    /// HTTP client for web seeds, unless they are disabled.
    pub(crate) web_seed: Option<WebSeedClient>,
    /// File I/O for storage the session creates.
    pub(crate) file_io: Arc<dyn FileIo>,
    pub(crate) dht: Option<Dht>,
    pub(crate) peer_id: PeerId,
    pub(crate) config: Arc<SessionConfig>,
    pub(crate) alerts: broadcast::Sender<SessionAlert>,
    pub(crate) tracker_key: u32,
    /// Port the session accepts peers on, if it listens.
    pub(crate) listen_port: Option<u16>,
    /// uTP sockets for dialing (empty without a listener or uTP).
    pub(crate) utp: UtpSockets,
    /// The session's torrents, updated when a magnet's metadata arrives.
    pub(crate) catalog: Catalog,
    /// The session-wide ban list.
    pub(crate) bans: crate::bans::Bans,
    /// The session's read cache, for storages the session creates.
    pub(crate) read_cache: Option<Arc<ReadCache>>,
    /// The session-wide hashing pool.
    pub(crate) hash_pool: HashPool,
    /// The session-wide rate limits.
    pub(crate) rates: crate::rate::RateLimits,
    /// The session-wide connection cap: every dial takes a slot first.
    pub(crate) connections: ConnectionLimit,
    /// Session state writer, with a state directory.
    pub(crate) persist: Option<Persister>,
    /// Web seed health from the previous run, applied when the web seeds
    /// load (the catalog is locked while a task is created, so it is
    /// passed in rather than read back).
    pub(crate) web_seed_stats: Vec<crate::persist::WebSeedStat>,
    /// Router port mapping status, when mapping runs.
    pub(crate) port_map: Option<tokio::sync::watch::Receiver<dravity_portmap::Status>>,
    /// Registers the second swarm id of a hybrid magnet once its metadata
    /// is known.
    pub(crate) aliases: Aliases,
}

impl TorrentContext {
    /// Engine settings for this torrent: DHT only if the session runs one,
    /// and neither DHT nor PEX for private torrents (BEP 27).
    fn engine_config(&self, private: bool) -> Config {
        let mut config = self.config.engine.clone();
        config.dht = self.dht.is_some() && !private;
        config.pex = config.pex && !private;
        config.tracker_exchange = config.tracker_exchange && !private;
        config.holepunch = config.holepunch && !private;
        config.listen_port = self.listen_port;
        config
    }

    /// Every tracker URL of the torrent, tier by tier (shared over
    /// `lt_tex`).
    fn tracker_urls(&self) -> Vec<String> {
        self.trackers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .flatten()
            .cloned()
            .collect()
    }

    /// Port reported to trackers and the DHT: the router's public TCP port
    /// when port mapping got one, else the listener's, else the configured
    /// one.
    fn announce_port(&self) -> u16 {
        let mapped = self
            .port_map
            .as_ref()
            .and_then(|rx| rx.borrow().external_port(dravity_portmap::Protocol::Tcp));
        announce_port(mapped, self.listen_port, self.config.announce_port)
    }

    /// Reserved bits of our handshake; `v2` if the torrent has a v2
    /// info-hash.
    pub(crate) fn reserved(&self, v2: bool) -> Reserved {
        peer::our_reserved(self.config.engine.fast_extension, v2)
    }

    /// Where this torrent's resume data lives, if resume is enabled.
    fn resume_path(&self) -> Option<PathBuf> {
        self.config
            .resume_dir
            .as_ref()
            .map(|dir| dir.join(format!("{}.resume", self.info_hash.to_hex())))
    }
}

/// What a torrent task starts from.
#[derive(Debug)]
pub(crate) enum Source {
    /// A `.torrent` file: content is known.
    Torrent {
        metainfo: Arc<Metainfo>,
        storage: Arc<dyn Storage>,
    },
    /// A magnet link: fetch the metadata first, then download.
    Magnet(MagnetStart),
}

/// The listener's routing table and this torrent's command channel (weak:
/// the task must not keep itself alive).
#[derive(Debug, Clone)]
pub(crate) struct Aliases {
    pub(crate) registry: Registry,
    pub(crate) tx: mpsc::WeakSender<Command>,
}

impl Aliases {
    fn register(&self, primary: InfoHash, hashes: &[InfoHash], v2: bool) {
        if let Some(tx) = self.tx.upgrade() {
            listen::register_aliases(&self.registry, primary, hashes, &tx, v2);
        }
    }
}

/// Piece layers by pieces root (BEP 52).
type PieceLayers = Vec<(Hash, Vec<Hash>)>;

/// One web seed of a torrent.
#[derive(Debug)]
struct WebSeed {
    url: String,
    /// The synthetic peer address it is reported under
    /// ([`dravity_engine::web_seed_addr`]), unique per URL.
    addr: SocketAddr,
    /// The peer slot while a task runs for it.
    key: Option<PeerKey>,
    /// Not started again before this.
    retry_at: Instant,
    /// Failures in a row (reset when data arrives).
    failures: u32,
    /// Not started again: banned for data that failed the hash check, or a
    /// URL that can never be fetched.
    stopped: bool,
}

/// What a magnet link's task needs once its metadata arrives.
#[derive(Debug, Clone)]
pub(crate) struct MagnetStart {
    /// Where the content goes.
    pub(crate) download_dir: PathBuf,
    /// The link has a v1 hash (`btih`): the task's info-hash is that one.
    pub(crate) v1: bool,
    /// The link's v2 hash (`btmh`), checked as well.
    pub(crate) info_hash_v2: Option<InfoHashV2>,
    /// Web seeds from the link (`ws`), added to the torrent's.
    pub(crate) web_seeds: Vec<String>,
    /// BEP 53 `so`: the files the link selects (empty: all). Used only
    /// when no file priorities were given.
    pub(crate) select_only: Vec<std::ops::RangeInclusive<usize>>,
}

/// The engine state machine currently driving the torrent.
#[derive(Debug)]
enum Phase {
    Metadata(Box<MetadataState>),
    Download(Box<TorrentState>),
}

impl Phase {
    fn update(&mut self, now: Instant, event: Event) -> Vec<Effect> {
        match self {
            Self::Metadata(state) => state.update(now, event),
            Self::Download(state) => state.update(now, event),
        }
    }

    fn status(&self) -> TorrentStatus {
        match self {
            Self::Metadata(state) => state.status(),
            Self::Download(state) => state.status(),
        }
    }

    fn stats(&self) -> Stats {
        match self {
            Self::Metadata(state) => state.stats(),
            Self::Download(state) => state.stats(),
        }
    }
}

/// The content of a torrent whose metadata is known.
#[derive(Debug, Clone)]
struct Content {
    metainfo: Arc<Metainfo>,
    storage: Arc<dyn Storage>,
}

pub(crate) struct TorrentTask {
    ctx: TorrentContext,
    phase: Phase,
    content: Option<Content>,
    /// A magnet link's download directory and extras, used once metadata
    /// arrives.
    magnet: Option<MagnetStart>,
    /// Verified `info` dictionary waiting to replace the metadata phase.
    metadata: Option<(Bytes, PieceLayers)>,
    peers: HashMap<PeerKey, mpsc::Sender<PeerCommand>>,
    /// Peers connected over uTP (the rest use TCP). Pruned against `peers`.
    utp_peers: HashSet<PeerKey>,
    /// Peers whose connection is RC4-encrypted (MSE).
    encrypted_peers: HashSet<PeerKey>,
    /// The torrent's web seeds (BEP 19) and their peer slots.
    web_seeds: Vec<WebSeed>,
    /// Web seeds the engine knows as peers.
    web_peers: HashSet<PeerKey>,
    /// The last announce outcome per tracker URL.
    tracker_log: TrackerLog,
    /// When the next scheduled scrape is due.
    next_scrape: Option<Instant>,
    next_key: u64,
    events_tx: mpsc::Sender<Input>,
    events_rx: mpsc::Receiver<Input>,
    commands: mpsc::Receiver<Command>,
    /// Events produced synchronously during dispatch, handled next.
    local: VecDeque<Event>,
    /// Block writes and upload reads waiting to go to storage together
    /// (see [`MAX_IO_BATCH`]).
    pending_writes: Vec<(BlockRef, Bytes)>,
    /// Hash-on-arrival state, fed by the block writes
    /// ([`SessionConfig::hash_on_arrival`]).
    hashers: Option<Arc<Mutex<Hashers>>>,
    pending_reads: Vec<(PeerKey, BlockRef)>,
    /// Announce and DHT tasks. Aborted on stop; they must not delay it.
    background: JoinSet<()>,
    /// Block writes and piece hashes. Drained on stop, before resume data
    /// is stamped, so a write cannot land after the stamp.
    disk: JoinSet<()>,
    /// File priorities: one per file once the metadata is known, otherwise
    /// as given (applied, if they fit, when it arrives).
    file_priorities: Vec<Priority>,
    /// Readers waiting for a piece.
    waiters: HashMap<u32, Vec<oneshot::Sender<()>>>,
    /// Urgent windows of open streams.
    streams: BTreeMap<u64, Vec<u32>>,
}

impl std::fmt::Debug for TorrentTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TorrentTask")
            .field("info_hash", &self.ctx.info_hash)
            .field("peers", &self.peers.len())
            .finish_non_exhaustive()
    }
}

impl TorrentTask {
    pub(crate) fn new(
        ctx: TorrentContext,
        source: Source,
        file_priorities: Vec<Priority>,
        commands: mpsc::Receiver<Command>,
    ) -> Self {
        let mut file_priorities = file_priorities;
        let (phase, content, magnet) = match source {
            Source::Torrent { metainfo, storage } => {
                let config = ctx.engine_config(metainfo.info().is_private());
                file_priorities = files::normalize(metainfo.info(), &file_priorities)
                    .unwrap_or_else(|| vec![Priority::Normal; metainfo.info().files().len()]);
                let pieces = files::piece_priorities(metainfo.info(), &file_priorities);
                // Served to magnet-link peers over BEP 9.
                let info = Bytes::copy_from_slice(metainfo.info_bytes());
                let state = TorrentState::new(*metainfo.info().layout(), config)
                    .with_banned_ips(ctx.bans.snapshot())
                    .with_trackers(ctx.tracker_urls())
                    .with_piece_roots(piece_roots(&metainfo))
                    .with_info_hash(ctx.info_hash)
                    .with_info_bytes(info)
                    .with_piece_layers(metainfo.info().piece_length(), owned_layers(&metainfo))
                    .with_padded_last_piece(padded_last_piece(metainfo.info()))
                    .with_missing_piece_layers(
                        metainfo.info().piece_length(),
                        missing_layers(&metainfo),
                    )
                    .with_priorities(&pieces);
                (
                    Phase::Download(Box::new(state)),
                    Some(Content { metainfo, storage }),
                    None,
                )
            }
            Source::Magnet(start) => {
                let config = ctx.engine_config(false);
                let state = match (start.v1, start.info_hash_v2) {
                    (false, Some(v2)) => MetadataState::new_v2(v2, config),
                    (_, Some(v2)) => {
                        MetadataState::new(ctx.info_hash, config).with_info_hash_v2(v2)
                    }
                    (_, None) => MetadataState::new(ctx.info_hash, config),
                };
                (Phase::Metadata(Box::new(state)), None, Some(start))
            }
        };
        let (events_tx, events_rx) = mpsc::channel(ctx.config.event_queue.max(1));
        let hashers = ctx
            .config
            .hash_on_arrival
            .then(|| Arc::new(Mutex::new(Hashers::default())));
        let mut task = Self {
            ctx,
            phase,
            content,
            magnet,
            metadata: None,
            peers: HashMap::new(),
            utp_peers: HashSet::new(),
            encrypted_peers: HashSet::new(),
            web_seeds: Vec::new(),
            web_peers: HashSet::new(),
            tracker_log: TrackerLog::default(),
            next_scrape: None,
            next_key: 0,
            events_tx,
            events_rx,
            commands,
            local: VecDeque::new(),
            pending_writes: Vec::new(),
            hashers,
            pending_reads: Vec::new(),
            background: JoinSet::new(),
            disk: JoinSet::new(),
            file_priorities,
            waiters: HashMap::new(),
            streams: BTreeMap::new(),
        };
        task.load_web_seeds();
        task
    }

    /// Takes the web seeds of the content (once it is known).
    fn load_web_seeds(&mut self) {
        let Some(content) = &self.content else {
            return;
        };
        if self.ctx.web_seed.is_none() {
            return;
        }
        let now = Instant::now();
        let saved = &self.ctx.web_seed_stats;
        let unix = crate::persist::unix_now();
        self.web_seeds = content
            .metainfo
            .web_seeds()
            .iter()
            .zip(0u32..)
            .map(|(url, index)| {
                let stat = saved.iter().find(|s| &s.url == url);
                let wait = stat.map_or(0, |s| s.retry_at.saturating_sub(unix));
                WebSeed {
                    url: url.clone(),
                    addr: dravity_engine::web_seed_addr(index),
                    key: None,
                    retry_at: now + std::time::Duration::from_secs(wait),
                    failures: stat.map_or(0, |s| s.failures),
                    stopped: stat.is_some_and(|s| s.stopped),
                }
            })
            .collect();
    }

    /// Stores web seed health in the catalog and the state directory.
    fn save_web_seed_stats(&self) {
        let now = Instant::now();
        let unix = crate::persist::unix_now();
        let stats: Vec<crate::persist::WebSeedStat> = self
            .web_seeds
            .iter()
            .map(|s| {
                let mut stat = crate::persist::WebSeedStat::new(s.url.clone());
                stat.failures = s.failures;
                stat.stopped = s.stopped;
                stat.retry_at = unix + s.retry_at.saturating_duration_since(now).as_secs();
                stat
            })
            .collect();
        let record = {
            let mut catalog = self
                .ctx
                .catalog
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let Some(entry) = catalog.get_mut(&self.ctx.info_hash) else {
                return;
            };
            entry.web_seeds = stats;
            entry.record(self.ctx.info_hash)
        };
        if let Some(persist) = &self.ctx.persist {
            persist.save_record(&record);
        }
    }

    /// Starts web seeds that are due while the torrent downloads, and
    /// closes them once it does not (finished, checking, paused…): a web
    /// seed only ever gives, and holding its slot helps nobody.
    fn maintain_web_seeds(&mut self) {
        if self.web_seeds.is_empty() {
            return;
        }
        let (Some(client), Some(content)) = (self.ctx.web_seed.clone(), self.content.clone())
        else {
            return;
        };
        let downloading = matches!(self.phase, Phase::Download(_))
            && self.phase.status() == TorrentStatus::Downloading;
        if !downloading {
            let open: Vec<PeerKey> = self
                .web_seeds
                .iter_mut()
                .filter_map(|s| s.key.take())
                .collect();
            for key in open {
                self.peers.remove(&key);
                if self.web_peers.remove(&key) {
                    self.local.push_back(Event::PeerDisconnected { peer: key });
                }
            }
            return;
        }
        let now = Instant::now();
        for i in 0..self.web_seeds.len() {
            let due = self
                .web_seeds
                .get(i)
                .is_some_and(|s| s.key.is_none() && !s.stopped && s.retry_at <= now);
            if !due {
                continue;
            }
            let (key, rx) = self.new_peer();
            let Some(seed) = self.web_seeds.get_mut(i) else {
                continue;
            };
            seed.key = Some(key);
            let params = WebSeedParams {
                key,
                url: seed.url.clone(),
                metainfo: Arc::clone(&content.metainfo),
                client: client.clone(),
            };
            tokio::spawn(web_seed::run(params, self.events_tx.clone(), rx));
        }
    }

    /// A web seed's connection ended without us closing it: try the URL
    /// again after a back-off that doubles with each failure in a row.
    fn web_seed_gone(&mut self, key: PeerKey) {
        self.web_peers.remove(&key);
        let base = self.ctx.config.web_seed_retry;
        let Some(seed) = self.web_seeds.iter_mut().find(|s| s.key == Some(key)) else {
            return;
        };
        seed.key = None;
        // A ban or a URL that can never work must not come back on the
        // back-off timer: the engine would only disconnect it again.
        if seed.stopped {
            self.save_web_seed_stats();
            return;
        }
        seed.failures = seed.failures.saturating_add(1);
        let factor = 1u32 << seed.failures.saturating_sub(1).min(5);
        seed.retry_at = Instant::now() + base.saturating_mul(factor);
        self.save_web_seed_stats();
    }

    /// Stops the web seed reported at `ip` (a synthetic `100::/64` address).
    /// Called when the engine bans it for data that failed the hash check.
    fn stop_web_seed(&mut self, ip: IpAddr) {
        if !dravity_engine::is_web_seed_addr(SocketAddr::new(ip, 0)) {
            return;
        }
        if let Some(seed) = self.web_seeds.iter_mut().find(|s| s.addr.ip() == ip) {
            seed.stopped = true;
            tracing::debug!(url = %seed.url, %ip, "web seed stopped after a hash failure");
            self.save_web_seed_stats();
        }
    }

    /// Piece priorities for `content` from the file priorities.
    fn piece_priorities(&self, content: &Content) -> Vec<Priority> {
        files::piece_priorities(content.metainfo.info(), &self.file_priorities)
    }

    /// Handles a command that does not become an engine event; returns the
    /// event for those that do.
    fn command(&mut self, command: Command) -> Option<Event> {
        match command {
            Command::Relisten(update) => {
                self.ctx.listen_port = update.port;
                self.ctx.utp = update.utp;
                self.ctx.port_map = update.port_map;
                self.announce_dht_port();
                Some(Event::SetListenPort { port: update.port })
            }
            Command::Stats(reply) => {
                let _ = reply.send(self.phase.stats());
                None
            }
            Command::Progress(reply) => {
                let files = match (&self.content, &self.phase) {
                    (Some(content), Phase::Download(state)) => Some(files::progress(
                        content.metainfo.info(),
                        state.have(),
                        &self.file_priorities,
                    )),
                    _ => None,
                };
                let _ = reply.send(TorrentProgress {
                    stats: self.phase.stats(),
                    files,
                });
                None
            }
            Command::AddPeers(peers) => Some(Event::AddPeers { peers }),
            Command::BanIp(ip) => Some(Event::BanIp { ip }),
            Command::Refilter => {
                let peers = match &self.phase {
                    Phase::Metadata(state) => state.peers(),
                    Phase::Download(state) => state.peers(),
                };
                let filter = self.ctx.bans.filter();
                for p in peers.iter().filter(|p| filter.is_blocked(p.addr.ip())) {
                    tracing::debug!(addr = %p.addr, "blocked by the IP filter; closing");
                    self.peers.remove(&p.key);
                    self.web_seed_gone(p.key);
                    self.local
                        .push_back(Event::PeerDisconnected { peer: p.key });
                }
                None
            }
            Command::Peers(reply) => {
                let peers = match &self.phase {
                    Phase::Metadata(state) => state.peers(),
                    Phase::Download(state) => state.peers(),
                };
                self.utp_peers.retain(|key| self.peers.contains_key(key));
                self.encrypted_peers
                    .retain(|key| self.peers.contains_key(key));
                let utp = &self.utp_peers;
                let web = &self.web_peers;
                let encrypted = &self.encrypted_peers;
                let geoip = self.ctx.config.geoip.as_deref();
                let _ = reply.send(status::peers(peers, geoip, |p| {
                    let transport = if web.contains(&p.key) {
                        Transport::Web
                    } else if utp.contains(&p.key) {
                        Transport::Utp
                    } else {
                        Transport::Tcp
                    };
                    (transport, encrypted.contains(&p.key))
                }));
                None
            }
            Command::Trackers(reply) => {
                let _ = reply.send(self.tracker_list());
                None
            }
            Command::Scrape(reply) => {
                self.scrape(Some(reply));
                None
            }
            Command::Incoming(accepted) => Some(self.accept(accepted)),
            Command::SetFilePriorities(priorities) => {
                self.file_priorities = priorities;
                let content = self.content.clone()?;
                Some(Event::SetPriorities {
                    priorities: self.piece_priorities(&content),
                })
            }
            Command::Content(reply) => {
                let _ = reply.send(self.content.as_ref().map(|c| ContentRef {
                    metainfo: Arc::clone(&c.metainfo),
                    storage: Arc::clone(&c.storage),
                }));
                None
            }
            Command::WaitPiece { piece, reply } => {
                if self.has_piece(piece) {
                    let _ = reply.send(());
                } else {
                    self.waiters.entry(piece).or_default().push(reply);
                }
                None
            }
            Command::Stream { id, pieces } => {
                self.streams.insert(id, pieces);
                Some(self.urgent_event())
            }
            Command::CloseStream(id) => {
                self.streams.remove(&id)?;
                Some(self.urgent_event())
            }
            // Handled by the run loop.
            Command::Stop(_) => None,
        }
    }

    fn has_piece(&self, piece: u32) -> bool {
        match &self.phase {
            Phase::Download(state) => state.have().get(piece),
            Phase::Metadata(_) => false,
        }
    }

    /// Every open stream's window, oldest stream first.
    fn urgent_event(&self) -> Event {
        let mut pieces = Vec::new();
        for window in self.streams.values() {
            for &piece in window {
                if !pieces.contains(&piece) {
                    pieces.push(piece);
                }
            }
        }
        Event::SetUrgent { pieces }
    }

    /// Answers readers whose piece is now verified; drops readers of pieces
    /// that cannot exist.
    fn wake_waiters(&mut self) {
        if self.waiters.is_empty() {
            return;
        }
        let Phase::Download(state) = &self.phase else {
            return;
        };
        let have = state.have();
        self.waiters.retain(|&piece, waiting| {
            if piece >= have.len() {
                return false;
            }
            if have.get(piece) {
                for reply in waiting.drain(..) {
                    let _ = reply.send(());
                }
                return false;
            }
            waiting.retain(|reply| !reply.is_closed());
            !waiting.is_empty()
        });
    }

    /// Runs until the torrent stops, then flushes storage, writes resume
    /// data and acknowledges a pending stop command.
    pub(crate) async fn run(mut self) {
        if let Some(content) = self.content.clone() {
            if let Some(state) = self.resumed_state(&content, None).await {
                self.phase = Phase::Download(Box::new(state));
            } else if let Phase::Download(state) = &mut self.phase {
                state.set_absent_pieces(prescan(&content).await);
            }
        }
        let mut tick = tokio::time::interval(self.ctx.config.tick_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The value already in the watch is what `Event::Start` announces.
        // Later updates are a new external port (or the mapping going away).
        if let Some(rx) = &mut self.ctx.port_map {
            rx.borrow_and_update();
        }
        let mut announced_port = self.ctx.announce_port();
        let mut stop_ack: Option<oneshot::Sender<()>> = None;
        self.handle(Event::Start).await;
        while self.phase.status() != TorrentStatus::Stopped {
            if let Some(event) = self.local.pop_front() {
                self.handle(event).await;
                continue;
            }
            // Blocks go to storage together: once no more input is ready, or
            // a batch is full.
            if self.events_rx.is_empty()
                || self.pending_writes.len() >= MAX_IO_BATCH
                || self.pending_reads.len() >= MAX_IO_BATCH
            {
                self.flush_io();
            }
            let event = tokio::select! {
                command = self.commands.recv() => match command {
                    Some(Command::Stop(ack)) => {
                        stop_ack = Some(ack);
                        Event::Stop
                    }
                    Some(command) => match self.command(command) {
                        Some(event) => event,
                        None => continue,
                    },
                    // Every handle and the session are gone.
                    None => Event::Stop,
                },
                Some(input) = self.events_rx.recv() => match self.translate(input) {
                    Some(event) => event,
                    None => continue,
                },
                _ = tick.tick() => {
                    self.maintain_web_seeds();
                    self.scheduled_scrape();
                    Event::Tick
                }
                // Reap finished background work so the set does not grow.
                Some(_) = self.background.join_next(), if !self.background.is_empty() => continue,
                Some(_) = self.disk.join_next(), if !self.disk.is_empty() => continue,
                () = await_port_change(&mut self.ctx.port_map) => {
                    let port = self.ctx.announce_port();
                    if port == announced_port {
                        continue;
                    }
                    announced_port = port;
                    self.announce_dht_port();
                    Event::Reannounce
                }
            };
            self.handle(event).await;
        }
        self.flush_io();
        self.peers.clear();
        self.background.shutdown().await;
        // Unblock disk tasks waiting to report, then let the writes finish.
        self.events_rx.close();
        while self.disk.join_next().await.is_some() {}
        self.finish_storage().await;
        // Mark the handle as not running before acknowledging the stop.
        self.commands.close();
        if let Some(ack) = stop_ack {
            let _ = ack.send(());
        }
    }

    /// Flushes storage and, once that succeeded, saves resume data. Nothing
    /// is written after this point, so the file stamps stay valid until the
    /// files are touched again.
    async fn finish_storage(&self) {
        let Some(content) = self.content.clone() else {
            return;
        };
        let resume = match (&self.phase, self.ctx.resume_path()) {
            (Phase::Download(state), Some(path)) => Some((path, state.have().to_bytes())),
            _ => None,
        };
        let info_hash = self.ctx.info_hash;
        let result = tokio::task::spawn_blocking(move || {
            content.storage.flush()?;
            let Some((path, have)) = resume else {
                return Ok(());
            };
            let Some(files) = content.storage.stamps()? else {
                return Ok(());
            };
            let data = ResumeData {
                info_hash,
                piece_count: content.metainfo.info().piece_count(),
                have: have.to_vec(),
                files,
            };
            if let Err(e) = save_resume(&path, &data) {
                tracing::warn!(error = %e, "saving resume data failed");
            }
            Ok::<_, dravity_storage::StorageError>(())
        })
        .await;
        if let Ok(Err(e)) = result {
            tracing::warn!(error = %e, "flushing storage failed");
        }
    }

    /// A download state seeded from valid resume data, or `None` to check
    /// the data from scratch.
    async fn resumed_state(&self, content: &Content, info: Option<&Bytes>) -> Option<TorrentState> {
        let path = self.ctx.resume_path()?;
        let storage = Arc::clone(&content.storage);
        let piece_count = content.metainfo.info().piece_count();
        let info_hash = self.ctx.info_hash;
        let loaded = tokio::task::spawn_blocking(move || {
            let data = match load_resume(&path) {
                Ok(Some(data)) => data,
                Ok(None) => return None,
                Err(e) => {
                    tracing::warn!(error = %e, "ignoring unreadable resume data");
                    return None;
                }
            };
            let stamps = storage.stamps().ok()??;
            data.is_valid_for(&info_hash, piece_count, &stamps)
                .then_some(data.have)
        })
        .await
        .ok()??;
        let have = Bitfield::from_wire(&loaded, piece_count).ok()?;
        tracing::debug!(pieces = have.count(), "resuming without a full check");
        let mut config = self.ctx.engine_config(content.metainfo.info().is_private());
        config.check_existing = false;
        let state = TorrentState::with_have(*content.metainfo.info().layout(), have, config)
            .with_banned_ips(self.ctx.bans.snapshot())
            .with_trackers(self.ctx.tracker_urls())
            .with_piece_roots(piece_roots(&content.metainfo))
            .with_info_hash(self.ctx.info_hash)
            .with_priorities(&self.piece_priorities(content));
        let info = info
            .cloned()
            .unwrap_or_else(|| Bytes::copy_from_slice(content.metainfo.info_bytes()));
        let piece_length = content.metainfo.info().piece_length();
        Some(
            state
                .with_info_bytes(info)
                .with_piece_layers(piece_length, owned_layers(&content.metainfo))
                .with_padded_last_piece(padded_last_piece(content.metainfo.info()))
                .with_missing_piece_layers(piece_length, missing_layers(&content.metainfo)),
        )
    }

    /// Maps peer-task inputs to engine events, keeping the peer map in sync.
    fn translate(&mut self, input: Input) -> Option<Event> {
        Some(match input {
            Input::Connected {
                key,
                addr,
                reserved,
                transport,
                encrypted,
                mse,
            } => {
                if !self.peers.contains_key(&key) {
                    return None;
                }
                if transport == Transport::Utp {
                    self.utp_peers.insert(key);
                }
                if encrypted {
                    self.encrypted_peers.insert(key);
                }
                Event::PeerConnected {
                    peer: key,
                    addr,
                    reserved,
                    link: PeerLink {
                        utp: transport == Transport::Utp,
                        mse,
                    },
                }
            }
            Input::ConnectFailed { key, addr, retry } => {
                self.peers.remove(&key);
                Event::PeerConnectFailed { addr, retry }
            }
            Input::Disconnected { key } => {
                self.utp_peers.remove(&key);
                self.encrypted_peers.remove(&key);
                self.web_seed_gone(key);
                self.peers.remove(&key)?;
                Event::PeerDisconnected { peer: key }
            }
            Input::WebSeedReady { key } => {
                if !self.peers.contains_key(&key) {
                    return None;
                }
                let addr = self.web_seeds.iter().find(|s| s.key == Some(key))?.addr;
                self.web_peers.insert(key);
                // An "incoming" peer at a synthetic, unroutable address:
                // never dialed, retried or shared over PEX (the engine
                // ignores `100::/64`). The web seed layer does its own
                // back-off. No extension bits: it only answers requests.
                Event::PeerAccepted {
                    peer: key,
                    addr,
                    reserved: Reserved::new(),
                    link: PeerLink::default(),
                }
            }
            Input::WebSeedFailed { key, retry } => {
                self.peers.remove(&key);
                if !retry && let Some(seed) = self.web_seeds.iter_mut().find(|s| s.key == Some(key))
                {
                    seed.stopped = true;
                    tracing::debug!(url = %seed.url, "web seed stopped: its URL cannot be fetched");
                }
                self.web_seed_gone(key);
                return None;
            }
            Input::Message { key, message } => {
                if !self.peers.contains_key(&key) {
                    return None;
                }
                if matches!(message, Message::Piece { .. })
                    && self.web_peers.contains(&key)
                    && let Some(seed) = self.web_seeds.iter_mut().find(|s| s.key == Some(key))
                    && seed.failures > 0
                {
                    seed.failures = 0;
                    self.save_web_seed_stats();
                }
                Event::Message { peer: key, message }
            }
            Input::Engine(event) => event,
            Input::Announced { event, outcomes } => {
                self.tracker_log.record(Instant::now(), outcomes);
                event
            }
            Input::Scraped { outcomes, reply } => {
                self.tracker_log.record_scrapes(Instant::now(), outcomes);
                if let Some(reply) = reply {
                    let _ = reply.send(self.tracker_list());
                }
                return None;
            }
        })
    }

    async fn handle(&mut self, event: Event) {
        let effects = self.phase.update(Instant::now(), event);
        for effect in effects {
            self.dispatch(effect).await;
        }
        if let Some((info, layers)) = self.metadata.take() {
            self.start_download(info, layers).await;
        }
        self.wake_waiters();
    }

    fn alert(&self, alert: Alert) {
        let _ = self.ctx.alerts.send(SessionAlert {
            info_hash: self.ctx.info_hash,
            alert,
        });
    }

    /// Stops the torrent after a fatal error outside the engine.
    fn fail(&mut self, message: String) {
        tracing::warn!(%message, "torrent failed");
        self.alert(Alert::Error { message });
        self.local.push_back(Event::Stop);
    }

    /// Trackers a peer shared (`lt_tex`): added as one new last tier, so
    /// the torrent's own trackers keep priority. The tracker client's SSRF
    /// guard still applies when they are announced to.
    fn add_exchanged_trackers(&mut self, urls: Vec<String>) {
        let mut tiers = self
            .ctx
            .trackers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let fresh: Vec<String> = urls
            .into_iter()
            .filter(|u| !tiers.iter().flatten().any(|t| t == u))
            .collect();
        if !fresh.is_empty() {
            tracing::debug!(?fresh, "trackers learned from peers");
            tiers.push(fresh);
        }
    }

    /// Switches a magnet link from the metadata phase to downloading.
    async fn start_download(&mut self, info: Bytes, layers: PieceLayers) {
        let Phase::Metadata(_) = self.phase else {
            return;
        };
        let Some(start) = self.magnet.take() else {
            return self.fail("magnet is missing a download directory".to_owned());
        };
        let dir = start.download_dir;
        let tiers = self
            .ctx
            .trackers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut metainfo = match Metainfo::from_info_bytes(&info, tiers) {
            Ok(metainfo) => metainfo.with_web_seeds(start.web_seeds),
            Err(e) => return self.fail(format!("invalid metadata: {e}")),
        };
        // Layers fetched with BEP 52 hash requests (already checked against
        // their roots by the engine; `set_piece_layer` checks again).
        for (root, layer) in layers {
            if let Err(e) = metainfo.set_piece_layer(&root, layer) {
                return self.fail(format!("invalid piece layer: {e}"));
            }
        }
        if metainfo.info().version() == MetaVersion::V2
            && !metainfo.info().missing_piece_layers().is_empty()
        {
            // The engine only hands over a v2-only torrent with every layer.
            return self.fail("v2-only torrent: piece layers are missing".to_owned());
        }
        let metainfo = Arc::new(metainfo);
        let storage: Arc<dyn Storage> = match FileStorage::new(&dir, metainfo.info()) {
            Ok(storage) => cached(
                Arc::new(
                    storage
                        .with_io(Arc::clone(&self.ctx.file_io))
                        .with_preallocate(self.ctx.config.preallocate),
                ),
                self.ctx.read_cache.as_ref(),
            ),
            Err(e) => return self.fail(format!("setting up storage: {e}")),
        };
        if self.file_priorities.is_empty() {
            self.file_priorities = files::selected(metainfo.info(), &start.select_only);
        }
        self.file_priorities = files::normalize(metainfo.info(), &self.file_priorities)
            .unwrap_or_else(|| {
                tracing::warn!("file priorities do not match the metadata; using normal");
                vec![Priority::Normal; metainfo.info().files().len()]
            });
        self.record_metadata(&metainfo);
        let content = Content { metainfo, storage };
        let info_meta = content.metainfo.info();
        let private = info_meta.is_private();
        let state = match self.resumed_state(&content, Some(&info)).await {
            Some(state) => state,
            None => TorrentState::new(
                *info_meta.layout(),
                self.ctx.engine_config(info_meta.is_private()),
            )
            .with_banned_ips(self.ctx.bans.snapshot())
            .with_trackers(self.ctx.tracker_urls())
            .with_piece_roots(piece_roots(&content.metainfo))
            .with_info_hash(self.ctx.info_hash)
            .with_info_bytes(info)
            .with_piece_layers(info_meta.piece_length(), owned_layers(&content.metainfo))
            .with_padded_last_piece(padded_last_piece(info_meta))
            .with_missing_piece_layers(info_meta.piece_length(), missing_layers(&content.metainfo))
            .with_priorities(&self.piece_priorities(&content))
            .with_absent_pieces(prescan(&content).await),
        };
        self.content = Some(content);
        self.load_web_seeds();
        // Swap in a throwaway (allocation-free) metadata state to take the
        // real one by value.
        let placeholder = MetadataState::new(self.ctx.info_hash, Config::default());
        let previous = std::mem::replace(&mut self.phase, Phase::Metadata(Box::new(placeholder)));
        let Phase::Metadata(metadata) = previous else {
            return;
        };
        let handover = metadata.into_handover();
        let swarm: HashSet<SocketAddr> = if private {
            handover.swarm.iter().copied().collect()
        } else {
            HashSet::new()
        };
        let state = state
            .with_started_announced(handover.started_announced)
            .with_started_pending(handover.started_pending)
            .with_announce_again(handover.announce_again)
            .with_connecting(handover.connecting.iter().copied())
            .with_dial_hints(
                handover
                    .hints
                    .iter()
                    .copied()
                    .filter(|(addr, _)| !swarm.contains(addr)),
            )
            .ban_peers(swarm.iter().copied());
        self.phase = Phase::Download(Box::new(state));
        self.local.push_back(Event::Start);
        for peer in handover.peers {
            if swarm.contains(&peer.addr) {
                // Dropping the command sender closes a DHT/PEX connection
                // that a private torrent must not keep.
                self.peers.remove(&peer.peer);
                continue;
            }
            self.local.push_back(if peer.incoming {
                Event::PeerAccepted {
                    peer: peer.peer,
                    addr: peer.addr,
                    reserved: peer.reserved,
                    link: peer.link,
                }
            } else {
                Event::PeerConnected {
                    peer: peer.peer,
                    addr: peer.addr,
                    reserved: peer.reserved,
                    link: peer.link,
                }
            });
            for message in peer.replay {
                self.local.push_back(Event::Message {
                    peer: peer.peer,
                    message,
                });
            }
        }
        let known: Vec<SocketAddr> = handover
            .known
            .into_iter()
            .filter(|addr| !swarm.contains(addr))
            .collect();
        if !known.is_empty() {
            self.local.push_back(Event::AddPeers { peers: known });
        }
    }

    /// A hybrid that started on SHA-1 alone got a missing piece layer from
    /// a peer: from now on the file's pieces are checked against the Merkle
    /// tree too. The layer is persisted with the metainfo, and the pieces
    /// already accepted on SHA-1 alone are checked again.
    fn install_piece_layer(&mut self, root: Hash, layer: Vec<Hash>) {
        let Some(content) = &mut self.content else {
            return;
        };
        let mut metainfo = Metainfo::clone(&content.metainfo);
        // The engine checked the layer against its root; this checks again.
        if let Err(e) = metainfo.set_piece_layer(&root, layer) {
            tracing::warn!(error = %e, "ignoring a piece layer");
            return;
        }
        let pieces = metainfo.info().pieces_with_root(&root);
        tracing::info!(
            missing = metainfo.info().missing_piece_layers().len(),
            "piece layer received; re-checking its file"
        );
        let metainfo = Arc::new(metainfo);
        content.metainfo = Arc::clone(&metainfo);
        self.record_metadata(&metainfo);
        for pieces in pieces {
            self.local.push_back(Event::RecheckPieces { pieces });
        }
    }

    /// A magnet's metadata (or a hybrid's late piece layer) arrived: the
    /// catalog (and the state directory) now hold the torrent, so a resume
    /// or restart does not fetch it again.
    fn record_metadata(&self, metainfo: &Arc<Metainfo>) {
        // A hybrid magnet: peers of the other swarm can connect right away.
        self.ctx.aliases.register(
            self.ctx.info_hash,
            &metainfo.swarm_hashes(),
            metainfo.info_hash_v2().is_some(),
        );
        let record = {
            let mut catalog = self
                .ctx
                .catalog
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let Some(entry) = catalog.get_mut(&self.ctx.info_hash) else {
                return;
            };
            entry.source = EntrySource::Torrent(Arc::clone(metainfo));
            entry.priorities.clone_from(&self.file_priorities);
            entry.record(self.ctx.info_hash)
        };
        if let Some(persist) = &self.ctx.persist {
            persist.save_torrent(&self.ctx.info_hash, metainfo);
            persist.save_record(&record);
        }
    }

    async fn dispatch(&mut self, effect: Effect) {
        match effect {
            Effect::Connect { addr, hints } => self.connect(addr, hints),
            Effect::Send { peer, message } => {
                let Some(tx) = self.peers.get(&peer) else {
                    return;
                };
                if tx.try_send(PeerCommand::Send(message)).is_err() {
                    // Full (too slow) or closed: drop the peer.
                    self.peers.remove(&peer);
                    self.web_seed_gone(peer);
                    self.local.push_back(Event::PeerDisconnected { peer });
                }
            }
            Effect::Disconnect { peer } => {
                self.peers.remove(&peer);
                self.web_seed_gone(peer);
            }
            Effect::WriteBlock { block, data } => self.write(block, data),
            Effect::HashPiece { piece } => self.hash(piece),
            Effect::HashBlocks { piece } => self.hash_blocks(piece),
            Effect::ReadBlock { peer, block } => self.read(peer, block),
            Effect::Announce {
                event,
                uploaded,
                downloaded,
                left,
            } => {
                let req = self.announce_request(event, uploaded, downloaded, left);
                // A hybrid torrent is in two swarms: trackers hear about
                // both ids.
                let extra: Vec<InfoHash> = self.swarm_hashes().into_iter().skip(1).collect();
                if event == AnnounceEvent::Stopped {
                    // The task is about to exit: announce inline, bounded.
                    // Nobody can read the outcome once the task exits.
                    let fut = announce_all(self.ctx.clone(), req, extra);
                    let _ = tokio::time::timeout(self.ctx.config.stop_announce_timeout, fut).await;
                } else {
                    let ctx = self.ctx.clone();
                    let tx = self.events_tx.clone();
                    self.background.spawn(async move {
                        let (event, outcomes) = announce_all(ctx, req, extra).await;
                        let _ = tx.send(Input::Announced { event, outcomes }).await;
                    });
                }
            }
            Effect::DhtLookup => self.dht_lookup(),
            Effect::MetadataReceived { info, piece_layers } => {
                self.metadata = Some((info, piece_layers));
            }
            Effect::PieceLayerReceived { root, layer } => self.install_piece_layer(root, layer),
            Effect::TrackersReceived { urls } => self.add_exchanged_trackers(urls),
            Effect::Alert(alert) => {
                if let Alert::PeerBanned { ip } = &alert {
                    self.stop_web_seed(*ip);
                    self.share_ban(*ip);
                }
                if matches!(alert, Alert::Finished)
                    && self.ctx.config.durability == Durability::OnComplete
                {
                    self.sync_all();
                }
                self.alert(alert);
            }
            other => tracing::warn!(?other, "unhandled engine effect"),
        }
    }

    /// Whether the torrent has a v2 info-hash: its metadata says so, or
    /// (before the metadata) its magnet link has a `btmh`. A `btih`-only
    /// magnet does not claim v2 until the metadata shows a hybrid.
    fn has_v2(&self) -> bool {
        match &self.content {
            Some(content) => content.metainfo.info_hash_v2().is_some(),
            None => self
                .magnet
                .as_ref()
                .is_some_and(|m| m.info_hash_v2.is_some()),
        }
    }

    /// The hashes to look up and announce in the DHT: the torrent's id, and
    /// for a hybrid torrent its truncated v2 hash as well.
    fn swarm_hashes(&self) -> Vec<InfoHash> {
        let mut hashes = vec![self.ctx.info_hash];
        if let Some(content) = &self.content {
            for h in content.metainfo.swarm_hashes() {
                if !hashes.contains(&h) {
                    hashes.push(h);
                }
            }
        }
        hashes
    }

    fn dht_lookup(&mut self) {
        let Some(dht) = self.ctx.dht.clone() else {
            // The engine only asks when DHT is enabled; answer anyway so it
            // does not wait for a lookup that never ends.
            self.local.push_back(Event::DhtPeers {
                peers: Vec::new(),
                failed: false,
            });
            return;
        };
        let hashes = self.swarm_hashes();
        let timeout = self.ctx.config.dht_lookup_timeout;
        let port_map = self.ctx.port_map.clone();
        let listen_port = self.ctx.listen_port;
        let configured = self.ctx.config.announce_port;
        let private = self
            .content
            .as_ref()
            .is_some_and(|c| c.metainfo.info().is_private());
        let tx = self.events_tx.clone();
        self.background.spawn(async move {
            // Every swarm of the torrent at once; failed only if all failed.
            let lookups = hashes.iter().map(|h| lookup_peers(&dht, *h, timeout));
            let mut peers: Vec<SocketAddr> = Vec::new();
            let mut failed = true;
            for (found, lookup_failed) in futures_util::future::join_all(lookups).await {
                failed &= lookup_failed;
                for addr in found {
                    if !peers.contains(&addr) {
                        peers.push(addr);
                    }
                }
            }
            tracing::debug!(peers = peers.len(), failed, "DHT lookup done");
            let _ = tx
                .send(Input::Engine(Event::DhtPeers { peers, failed }))
                .await;
            // Tell the DHT where to find us, so peers can connect: only
            // after a lookup that reached nodes, bounded by the same timeout.
            // The port is read now, not when the lookup started: NAT-PMP may
            // have chosen an external port while the lookup was in flight.
            if private || failed {
                return;
            }
            let mapped = port_map
                .as_ref()
                .and_then(|rx| rx.borrow().external_port(dravity_portmap::Protocol::Tcp));
            if let Some(port) = listen_port.map(|_| announce_port(mapped, listen_port, configured))
            {
                for info_hash in hashes {
                    let announced =
                        tokio::time::timeout(timeout, dht.announce(info_hash, Some(port))).await;
                    if let Ok(Err(e)) = announced {
                        tracing::debug!(error = %e, "DHT announce failed");
                    }
                }
            }
        });
    }

    /// Publishes the current TCP port to the DHT without waiting for the
    /// next lookup. Private torrents and sessions without a DHT skip it.
    fn announce_dht_port(&mut self) {
        if self
            .content
            .as_ref()
            .is_some_and(|c| c.metainfo.info().is_private())
        {
            return;
        }
        let Some(dht) = self.ctx.dht.clone() else {
            return;
        };
        let Some(port) = self.ctx.listen_port.map(|_| self.ctx.announce_port()) else {
            return;
        };
        let hashes = self.swarm_hashes();
        let timeout = self.ctx.config.dht_lookup_timeout;
        self.background.spawn(async move {
            for info_hash in hashes {
                let announced =
                    tokio::time::timeout(timeout, dht.announce(info_hash, Some(port))).await;
                if let Ok(Err(e)) = announced {
                    tracing::debug!(error = %e, "DHT announce failed");
                }
            }
        });
    }

    /// A fresh peer key and command queue, registered in the peer map.
    fn new_peer(&mut self) -> (PeerKey, mpsc::Receiver<PeerCommand>) {
        let key = PeerKey(self.next_key);
        self.next_key += 1;
        let (tx, rx) = mpsc::channel(self.ctx.config.peer_queue.max(1));
        self.peers.insert(key, tx);
        (key, rx)
    }

    /// Starts a task for an accepted connection; the returned event tells
    /// the engine about it before any of the peer's messages.
    fn accept(&mut self, accepted: Accepted) -> Event {
        let Accepted {
            stream,
            addr,
            reserved,
            leftover,
            slot,
        } = accepted;
        let (key, rx) = self.new_peer();
        if stream.kind() == Transport::Utp {
            self.utp_peers.insert(key);
        }
        if stream.encrypted() {
            self.encrypted_peers.insert(key);
        }
        let link = PeerLink {
            utp: stream.kind() == Transport::Utp,
            // A plain incoming handshake says nothing about MSE support.
            mse: stream.uses_mse().then_some(true),
        };
        tokio::spawn(peer::run_accepted(
            (key, addr),
            stream,
            leftover,
            self.events_tx.clone(),
            rx,
            slot,
            self.ctx.rates.clone(),
        ));
        Event::PeerAccepted {
            peer: key,
            addr,
            reserved,
            link,
        }
    }

    fn connect(&mut self, addr: SocketAddr, hints: DialHints) {
        if self.ctx.config.encryption == EncryptionPolicy::Required
            && hints.encryption == Some(false)
        {
            // PEX or an earlier dial says the peer does not do MSE: dialing
            // it would only fail the required handshake.
            tracing::trace!(%addr, "skipping a peer without encryption");
            self.local
                .push_back(Event::PeerConnectFailed { addr, retry: false });
            return;
        }
        if self.ctx.bans.filter().is_blocked(addr.ip()) {
            tracing::trace!(%addr, "blocked by the IP filter; not dialing");
            self.local
                .push_back(Event::PeerConnectFailed { addr, retry: false });
            return;
        }
        let Some(slot) = self.ctx.connections.try_admit() else {
            tracing::trace!(%addr, "session connection limit reached; dial deferred");
            self.local.push_back(Event::PeerConnectDeferred { addr });
            return;
        };
        let (key, rx) = self.new_peer();
        let params = PeerParams {
            key,
            addr,
            info_hash: self.ctx.info_hash,
            our_id: self.ctx.peer_id,
            reserved: self.ctx.reserved(self.has_v2()),
            connect_timeout: self.ctx.config.connect_timeout,
            utp: self.ctx.utp.for_addr(addr).cloned(),
            prefer_utp: self.ctx.config.prefer_utp,
            encryption: self.ctx.config.encryption,
            hints,
            rates: self.ctx.rates.clone(),
            proxy: self.ctx.config.proxy.clone().map(Arc::new),
            dscp: self.ctx.config.peer_dscp,
            network: self.ctx.config.network.clone(),
        };
        // Peer tasks are not in `background`: dropping their command sender
        // ends them, and they must not delay shutdown.
        tokio::spawn(peer::run(params, self.events_tx.clone(), rx, slot));
    }

    fn write(&mut self, block: BlockRef, data: Bytes) {
        if self.content.is_none() {
            tracing::warn!(?block, "block write before metadata");
            return;
        }
        self.pending_writes.push((block, data));
    }

    /// Queues a block a peer asked for.
    fn read(&mut self, peer: PeerKey, block: BlockRef) {
        if self.content.is_none() {
            tracing::warn!(?block, "block read before metadata");
            return;
        }
        self.pending_reads.push((peer, block));
    }

    /// Sends the queued block writes and reads to storage, each as one
    /// batch (io_uring submits a batch at once and merges adjacent blocks).
    /// Reads share the disk set with writes so a stop waits for them too.
    fn flush_io(&mut self) {
        let Some(content) = self.content.clone() else {
            self.pending_writes.clear();
            self.pending_reads.clear();
            return;
        };
        if !self.pending_writes.is_empty() {
            let writes = std::mem::take(&mut self.pending_writes);
            let content = content.clone();
            let hashers = self.hashers.clone();
            let tx = self.events_tx.clone();
            self.disk.spawn(async move {
                let events = tokio::task::spawn_blocking(move || {
                    let events = write_batch(&content, writes.clone());
                    if let Some(hashers) = hashers {
                        feed_hashers(&hashers, &content, &writes, &events);
                    }
                    events
                })
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "block writes panicked");
                    Vec::new()
                });
                for event in events {
                    let _ = tx.send(Input::Engine(event)).await;
                }
            });
        }
        if !self.pending_reads.is_empty() {
            let reads = std::mem::take(&mut self.pending_reads);
            let tx = self.events_tx.clone();
            self.disk.spawn(async move {
                let events = tokio::task::spawn_blocking(move || read_batch(&content, reads))
                    .await
                    .unwrap_or_else(|e| {
                        tracing::warn!(error = %e, "block reads panicked");
                        Vec::new()
                    });
                for event in events {
                    let _ = tx.send(Input::Engine(event)).await;
                }
            });
        }
    }

    fn hash(&mut self, piece: u32) {
        let Some(content) = self.content.clone() else {
            tracing::warn!(piece, "hash request before metadata");
            return;
        };
        let arrived = self
            .hashers
            .as_ref()
            .and_then(|h| h.lock().unwrap_or_else(PoisonError::into_inner).take(piece));
        let pool = self.ctx.hash_pool.clone();
        let tx = self.events_tx.clone();
        let sync = self.ctx.config.durability == Durability::PerPiece;
        self.disk.spawn(async move {
            let result = pool
                .run(move || {
                    let info = content.metainfo.info();
                    // Blocks hashed as they were written: no read-back.
                    let ok = match arrived.and_then(|h| h.verify(info)) {
                        Some(ok) => ok,
                        None => verify_piece(content.storage.as_ref(), info, piece)?,
                    };
                    // Durable before the engine counts it.
                    if ok
                        && sync
                        && let Some(range) = info.layout().piece_range(piece)
                    {
                        content
                            .storage
                            .sync_range(range.start, range.end - range.start)?;
                    }
                    Ok::<bool, dravity_storage::StorageError>(ok)
                })
                .await;
            // A mismatch or a short file is "we don't have it". Any other I/O
            // error would otherwise reset the piece and download it forever.
            let event = match result {
                Some(Ok(ok)) => Event::PieceHashed { piece, ok },
                Some(Err(e)) if e.is_missing_data() => Event::PieceHashed { piece, ok: false },
                Some(Err(e)) => {
                    tracing::warn!(piece, error = %e, "piece hash failed");
                    Event::StorageFailed {
                        error: format!("hashing piece {piece}: {e}"),
                    }
                }
                None => Event::StorageFailed {
                    error: format!("hashing piece {piece}: the hash job failed"),
                },
            };
            let _ = tx.send(Input::Engine(event)).await;
        });
    }

    /// Forces everything written so far to disk
    /// ([`Durability::OnComplete`]).
    fn sync_all(&mut self) {
        let Some(content) = self.content.clone() else {
            return;
        };
        let tx = self.events_tx.clone();
        self.disk.spawn(async move {
            let storage = Arc::clone(&content.storage);
            let result = tokio::task::spawn_blocking(move || storage.flush()).await;
            let error = match result {
                Ok(Ok(())) => return,
                Ok(Err(e)) => e.to_string(),
                Err(e) => e.to_string(),
            };
            tracing::warn!(%error, "syncing the finished download failed");
            let _ = tx
                .send(Input::Engine(Event::StorageFailed {
                    error: format!("syncing the finished download: {error}"),
                }))
                .await;
        });
    }

    /// Hashes each block of `piece` for smart banning and reports
    /// [`Event::BlockHashes`] (empty if the piece cannot be read).
    fn hash_blocks(&mut self, piece: u32) {
        let Some(content) = self.content.clone() else {
            self.local.push_back(Event::BlockHashes {
                piece,
                hashes: Vec::new(),
            });
            return;
        };
        let pool = self.ctx.hash_pool.clone();
        let tx = self.events_tx.clone();
        self.disk.spawn(async move {
            let hashes = pool
                .run(move || block_hashes(content.storage.as_ref(), content.metainfo.info(), piece))
                .await
                .and_then(Result::ok)
                .unwrap_or_default();
            let _ = tx
                .send(Input::Engine(Event::BlockHashes { piece, hashes }))
                .await;
        });
    }

    /// Adds `ip` to the session's ban list and tells the other running
    /// torrents.
    fn share_ban(&self, ip: IpAddr) {
        if !self.ctx.bans.insert(ip) {
            return;
        }
        let catalog = self
            .ctx
            .catalog
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for (hash, entry) in catalog.iter() {
            if *hash == self.ctx.info_hash {
                continue;
            }
            if let Some(tx) = &entry.commands {
                // Best effort: a full queue still has the ban list on restart
                // and the listener refuses the address.
                let _ = tx.try_send(Command::BanIp(ip));
            }
        }
    }

    /// Every tracker with its last announce and scrape.
    fn tracker_list(&self) -> Vec<TrackerStatus> {
        let tiers = self
            .ctx
            .trackers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        self.tracker_log.list(&tiers)
    }

    /// Scrapes when [`SessionConfig::scrape_interval`] says one is due.
    fn scheduled_scrape(&mut self) {
        let Some(every) = self.ctx.config.scrape_interval else {
            return;
        };
        let now = Instant::now();
        if self.next_scrape.is_some_and(|at| now < at) {
            return;
        }
        self.next_scrape = Some(now + every);
        self.scrape(None);
    }

    /// Scrapes every tracker in the background; the results come back as
    /// [`Input::Scraped`].
    fn scrape(&mut self, reply: Option<oneshot::Sender<Vec<TrackerStatus>>>) {
        let urls: Vec<String> = self
            .ctx
            .trackers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .flatten()
            .cloned()
            .collect();
        let client = self.ctx.tracker.clone();
        let hash = *self.ctx.info_hash.as_bytes();
        let tx = self.events_tx.clone();
        self.background.spawn(async move {
            let mut set = JoinSet::new();
            for url in urls {
                let client = client.clone();
                set.spawn(async move {
                    let outcome = match client.scrape(&url, &hash).await {
                        Ok(stats) => ScrapeOutcome::Ok(stats),
                        Err(e) => ScrapeOutcome::Failed {
                            error: e.to_string(),
                        },
                    };
                    (url, outcome)
                });
            }
            let mut outcomes = Vec::new();
            while let Some(joined) = set.join_next().await {
                if let Ok(outcome) = joined {
                    outcomes.push(outcome);
                }
            }
            let _ = tx.send(Input::Scraped { outcomes, reply }).await;
        });
    }

    fn announce_request(
        &self,
        event: AnnounceEvent,
        uploaded: u64,
        downloaded: u64,
        left: u64,
    ) -> AnnounceRequest {
        AnnounceRequest {
            info_hash: self.ctx.info_hash,
            peer_id: self.ctx.peer_id,
            port: self.ctx.announce_port(),
            uploaded,
            downloaded,
            left,
            event: match event {
                AnnounceEvent::Started => dravity_tracker::AnnounceEvent::Started,
                AnnounceEvent::Completed => dravity_tracker::AnnounceEvent::Completed,
                AnnounceEvent::Stopped => dravity_tracker::AnnounceEvent::Stopped,
                AnnounceEvent::None => dravity_tracker::AnnounceEvent::None,
            },
            num_want: Some(self.ctx.config.num_want),
            key: self.ctx.tracker_key,
            crypto: crypto_support(self.ctx.config.encryption),
        }
    }
}

/// Collects DHT peers until `timeout`. Peers that arrived before the
/// deadline are kept. `failed` is set on timeout, and when the lookup ended
/// with nobody because the routing table has no neighbors yet.
///
/// [`Dht::bootstrapped`](dravity_dht::Dht::bootstrapped) is not used here: it
/// runs a `find_node` query, so a short timeout around it treats every empty
/// swarm as a bootstrap failure and the engine retries every `dht_retry`.
async fn lookup_peers(
    dht: &Dht,
    info_hash: InfoHash,
    timeout: std::time::Duration,
) -> (Vec<SocketAddr>, bool) {
    let mut seen = HashSet::new();
    let mut peers = Vec::new();
    let mut stream = std::pin::pin!(dht.peer_batches(info_hash));
    let deadline = tokio::time::Instant::now() + timeout;
    let timed_out = loop {
        if peers.len() >= dravity_dht::DEFAULT_MAX_PEERS {
            break false;
        }
        match tokio::time::timeout_at(deadline, stream.as_mut().next()).await {
            Ok(Some(batch)) => {
                for addr in batch {
                    if seen.insert(addr) {
                        peers.push(addr);
                    }
                }
            }
            Ok(None) => break false,
            Err(_) => break true,
        }
    };
    // Only an empty, finished lookup needs the routing-table snapshot.
    // `has_nodes` is a local read; it does not start another DHT query.
    let has_nodes = if !timed_out && peers.is_empty() {
        dht.has_nodes().await
    } else {
        true
    };
    let failed = dht_lookup_failed(timed_out, peers.is_empty(), has_nodes);
    (peers, failed)
}

/// Whether the engine should retry a DHT lookup after [`Config::dht_retry`]
/// instead of waiting the full interval.
///
/// A timeout is retried soon (peers found before the deadline are still
/// returned). So is an empty result when the node has no neighbors yet.
/// An empty result from a node that does have neighbors is an empty swarm.
fn dht_lookup_failed(timed_out: bool, peers_empty: bool, has_nodes: bool) -> bool {
    timed_out || (peers_empty && !has_nodes)
}

/// Announces to the torrent's trackers following BEP 12: tiers are tried
/// concurrently, the trackers within a tier one after another until one
/// answers, and a tracker that answered moves to the front of its tier.
/// Success if any tier got an answer; the results are merged. Also returns
/// what each tracker that was asked said, for [`TrackerLog`].
async fn announce_all(
    ctx: TorrentContext,
    req: AnnounceRequest,
    extra: Vec<InfoHash>,
) -> (Event, Vec<(String, TrackerOutcome)>) {
    let tiers: Vec<Vec<String>> = ctx
        .trackers
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter(|tier| !tier.is_empty())
        .cloned()
        .collect();
    if tiers.is_empty() {
        // Explicit peers can still download. Reporting a failure would retry
        // forever and surface a tracker error for a trackerless torrent.
        let event = Event::Announced {
            peers: Vec::new(),
            interval: None,
        };
        return (event, Vec::new());
    }
    let mut set = JoinSet::new();
    for tier in tiers {
        set.spawn(announce_tier(
            ctx.tracker.clone(),
            Arc::clone(&ctx.trackers),
            tier,
            req.clone(),
            extra.clone(),
        ));
    }
    let mut merge = AnnounceMerge::default();
    let mut errors = Vec::new();
    let mut outcomes = Vec::new();
    let mut any_ok = false;
    while let Some(joined) = set.join_next().await {
        let Ok((answer, tier_errors)) = joined else {
            continue;
        };
        for (url, error) in tier_errors {
            errors.push(format!("{url}: {error}"));
            outcomes.push((url, TrackerOutcome::Failed { error }));
        }
        if let Some((url, response)) = answer {
            any_ok = true;
            outcomes.push((
                url.clone(),
                TrackerOutcome::Ok {
                    interval: response.interval,
                    seeders: response.seeders,
                    leechers: response.leechers,
                    peers: response.peers.len(),
                },
            ));
            merge.observe(&url, response);
        }
    }
    let event = if any_ok {
        for warning in &merge.warnings {
            tracing::warn!(%warning, "tracker warning");
        }
        let interval = merge.interval();
        Event::Announced {
            peers: merge.peers,
            interval,
        }
    } else {
        Event::AnnounceFailed {
            error: errors.join("; "),
        }
    };
    (event, outcomes)
}

/// Tries the trackers of one tier in order; returns the first answer and
/// the errors (by URL) of those that failed before it.
async fn announce_tier(
    client: TrackerClient,
    tiers: Tiers,
    tier: Vec<String>,
    req: AnnounceRequest,
    extra: Vec<InfoHash>,
) -> (Option<(String, AnnounceResponse)>, Vec<(String, String)>) {
    let mut errors = Vec::new();
    for url in tier {
        match client.announce(&url, &req).await {
            Ok(mut response) => {
                tracing::debug!(%url, peers = response.peers.len(), "announce ok");
                promote(&tiers, &url);
                // The other swarm ids go to the tracker that answered. Their
                // peers join the merge; their failures only get logged, the
                // tier's outcome is the primary id's.
                for info_hash in extra {
                    let other = AnnounceRequest {
                        info_hash,
                        ..req.clone()
                    };
                    match client.announce(&url, &other).await {
                        Ok(more) => {
                            tracing::debug!(%url, %info_hash, peers = more.peers.len(), "announce ok");
                            for peer in more.peers {
                                if !response.peers.contains(&peer) {
                                    response.peers.push(peer);
                                }
                            }
                        }
                        Err(e) => {
                            tracing::debug!(%url, %info_hash, error = %error_chain(&e), "announce failed");
                        }
                    }
                }
                return (Some((url, response)), errors);
            }
            Err(e) => {
                let e = error_chain(&e);
                tracing::debug!(%url, error = %e, "announce failed");
                errors.push((url, e));
            }
        }
    }
    (None, errors)
}

/// Moves `url` to the front of its tier (BEP 12).
fn promote(tiers: &Mutex<Vec<Vec<String>>>, url: &str) {
    let mut tiers = tiers.lock().unwrap_or_else(PoisonError::into_inner);
    for tier in tiers.iter_mut() {
        if let Some(pos) = tier.iter().position(|u| u == url) {
            let url = tier.remove(pos);
            tier.insert(0, url);
            return;
        }
    }
}

/// Tiers ready for announcing: empty tiers dropped and each tier shuffled
/// (BEP 12), so load spreads over a tier's trackers.
pub(crate) fn prepare_tiers(tiers: Vec<Vec<String>>) -> Tiers {
    let tiers = tiers
        .into_iter()
        .filter(|tier| !tier.is_empty())
        .map(|mut tier| {
            fastrand::shuffle(&mut tier);
            tier
        })
        .collect();
    Arc::new(Mutex::new(tiers))
}

/// Peers and re-announce timing merged from every tracker that answered.
#[derive(Debug, Default)]
struct AnnounceMerge {
    peers: Vec<SocketAddr>,
    /// Smallest requested interval.
    interval: Option<std::time::Duration>,
    /// Largest `min interval`. BEP 3 forbids announcing more often than this.
    min_interval: Option<std::time::Duration>,
    warnings: Vec<String>,
}

impl AnnounceMerge {
    fn observe(&mut self, url: &str, response: AnnounceResponse) {
        if let Some(warning) = response.warning.as_deref().filter(|w| !w.is_empty()) {
            self.warnings.push(format!("{url}: {warning}"));
        }
        self.peers.extend(response.peers);
        self.interval = min_duration(self.interval, response.interval);
        self.min_interval = max_duration(self.min_interval, response.min_interval);
    }

    /// Interval to report to the engine: not sooner than any tracker's minimum,
    /// otherwise the soonest requested interval.
    fn interval(&self) -> Option<std::time::Duration> {
        match (self.interval, self.min_interval) {
            (Some(interval), Some(min)) => Some(interval.max(min)),
            (interval, min) => interval.or(min),
        }
    }
}

fn min_duration(
    a: Option<std::time::Duration>,
    b: Option<std::time::Duration>,
) -> Option<std::time::Duration> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn max_duration(
    a: Option<std::time::Duration>,
    b: Option<std::time::Duration>,
) -> Option<std::time::Duration> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// Waits until the router's port-mapping status changes. With no mapper,
/// or after the mapper task has ended, waits forever so the torrent loop
/// does not spin.
async fn await_port_change(
    port_map: &mut Option<tokio::sync::watch::Receiver<dravity_portmap::Status>>,
) {
    match port_map {
        Some(rx) => {
            if rx.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
        None => std::future::pending::<()>().await,
    }
}

/// Port to announce: the router's mapped port, the listen port, or the
/// configured fallback, in that order.
fn announce_port(mapped: Option<u16>, listen: Option<u16>, configured: u16) -> u16 {
    mapped.or(listen).unwrap_or(configured)
}

/// Formats an error with its `source()` chain (`outer: inner: root`).
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(inner) = source {
        let part = inner.to_string();
        if !text.contains(&part) {
            text.push_str(": ");
            text.push_str(&part);
        }
        source = inner.source();
    }
    text
}

/// The pieces a stat of every file shows cannot be on disk, so the start
/// check need not hash them (empty when the storage has no stamps).
async fn prescan(content: &Content) -> Vec<u32> {
    let storage = Arc::clone(&content.storage);
    let metainfo = Arc::clone(&content.metainfo);
    let absent = tokio::task::spawn_blocking(move || match storage.stamps() {
        Ok(Some(stamps)) => dravity_storage::absent_pieces(metainfo.info(), &stamps),
        _ => Vec::new(),
    })
    .await
    .unwrap_or_default();
    if !absent.is_empty() {
        tracing::debug!(
            pieces = absent.len(),
            "prescan: skipping pieces that cannot be on disk"
        );
    }
    absent
}

/// Wraps `storage` in the session's read cache, if there is one.
pub(crate) fn cached(
    storage: Arc<dyn Storage>,
    cache: Option<&Arc<ReadCache>>,
) -> Arc<dyn Storage> {
    match cache {
        Some(cache) => Arc::new(CachedStorage::new(storage, Arc::clone(cache))),
        None => storage,
    }
}

/// Bytes of early blocks one torrent's hashers may hold while they wait to
/// be fed in order. A piece that would go over falls back to read-back.
const MAX_HASH_PENDING: usize = 32 * 1024 * 1024;

/// Hash-on-arrival state of one torrent: a [`PieceHasher`] per piece being
/// written.
#[derive(Debug, Default)]
struct Hashers {
    pieces: HashMap<u32, PieceHasher<Bytes>>,
    /// Sum of the hashers' pending bytes.
    pending: usize,
}

impl Hashers {
    /// Takes the hasher of `piece` (its blocks are all written).
    fn take(&mut self, piece: u32) -> Option<PieceHasher<Bytes>> {
        let h = self.pieces.remove(&piece)?;
        self.pending = self.pending.saturating_sub(h.pending_bytes());
        Some(h)
    }
}

/// Feeds the blocks of `writes` that were written (per `events`) to their
/// pieces' hashers.
fn feed_hashers(
    hashers: &Mutex<Hashers>,
    content: &Content,
    writes: &[(BlockRef, Bytes)],
    events: &[Event],
) {
    let info = content.metainfo.info();
    let mut hashers = hashers.lock().unwrap_or_else(PoisonError::into_inner);
    for (block, data) in writes {
        if !events.contains(&Event::BlockWritten { block: *block }) {
            continue;
        }
        let Hashers { pieces, pending } = &mut *hashers;
        let Some(h) = (match pieces.entry(block.piece) {
            std::collections::hash_map::Entry::Occupied(e) => Some(e.into_mut()),
            std::collections::hash_map::Entry::Vacant(e) => {
                PieceHasher::new(info, block.piece).map(|h| e.insert(h))
            }
        }) else {
            continue;
        };
        let before = h.pending_bytes();
        let usable = h.add(block.offset, data.clone());
        *pending = (*pending + h.pending_bytes()).saturating_sub(before);
        if usable && *pending > MAX_HASH_PENDING {
            // Too many early blocks: this piece is read back instead.
            *pending = pending.saturating_sub(h.pending_bytes());
            pieces.remove(&block.piece);
        }
    }
}

/// Most block writes (or upload reads) sent to storage as one batch.
const MAX_IO_BATCH: usize = 64;

/// Writes `writes` as one batch. If the batch fails, each block is tried
/// on its own so only the blocks that really failed are reported.
fn write_batch(content: &Content, writes: Vec<(BlockRef, Bytes)>) -> Vec<Event> {
    let layout = content.metainfo.info().layout();
    let storage = content.storage.as_ref();
    let blocks: Vec<BlockRef> = writes.iter().map(|(block, _)| *block).collect();
    if writes.len() > 1 && write_blocks(storage, layout, writes.clone()).is_ok() {
        return blocks
            .into_iter()
            .map(|block| Event::BlockWritten { block })
            .collect();
    }
    writes
        .into_iter()
        .map(
            |(block, data)| match write_block(storage, layout, block, data) {
                Ok(()) => Event::BlockWritten { block },
                Err(e) => Event::WriteFailed {
                    block,
                    error: e.to_string(),
                },
            },
        )
        .collect()
}

/// Reads `reads` as one batch. If the batch fails, each block is read on
/// its own so only the blocks that really failed are reported.
fn read_batch(content: &Content, reads: Vec<(PeerKey, BlockRef)>) -> Vec<Event> {
    let layout = content.metainfo.info().layout();
    let storage = content.storage.as_ref();
    let blocks: Vec<BlockRef> = reads.iter().map(|(_, block)| *block).collect();
    if reads.len() > 1
        && let Ok(data) = read_blocks(storage, layout, &blocks)
    {
        return reads
            .into_iter()
            .zip(data)
            .map(|((peer, block), data)| Event::BlockRead { peer, block, data })
            .collect();
    }
    reads
        .into_iter()
        .map(|(peer, block)| match read_block(storage, layout, block) {
            Ok(data) => Event::BlockRead {
                peer,
                block,
                data: Bytes::from(data),
            },
            Err(e) => Event::ReadFailed {
                peer,
                block,
                error: e.to_string(),
            },
        })
        .collect()
}

/// The pieces roots (with their piece counts) whose layer `metainfo` still
/// lacks, to fetch in the background.
fn missing_layers(metainfo: &Metainfo) -> Vec<(Hash, u32)> {
    let info = metainfo.info();
    info.missing_piece_layers()
        .into_iter()
        .filter_map(|root| Some((root, info.piece_layer_len(&root)?)))
        .collect()
}

/// Whether peers may ask for the zeros after the last piece's content (see
/// [`TorrentState::with_padded_last_piece`]): v2-only torrents, whose files
/// are all piece-aligned. A hybrid's last piece is sized by its v1 view.
fn padded_last_piece(info: &Info) -> bool {
    info.version() == MetaVersion::V2
}

/// The piece layers of `metainfo`, owned, for serving BEP 52 hash requests.
/// Each v2 file's pieces: `(pieces root, first piece, piece count)`, for
/// smart banning's leaf hash requests.
fn piece_roots(metainfo: &Metainfo) -> Vec<(Hash, u32, u32)> {
    let info = metainfo.info();
    let mut out = Vec::new();
    for (root, _) in info.piece_layers() {
        for range in info.pieces_with_root(&root) {
            out.push((root, range.start, range.end.saturating_sub(range.start)));
        }
    }
    out
}

fn owned_layers(metainfo: &Metainfo) -> PieceLayers {
    metainfo
        .info()
        .piece_layers()
        .into_iter()
        .map(|(root, layer)| (root, layer.to_vec()))
        .collect()
}

/// What the trackers hear about encryption under `policy` (MSE's
/// `supportcrypto` / `requirecrypto`).
fn crypto_support(policy: EncryptionPolicy) -> dravity_tracker::CryptoSupport {
    match policy {
        EncryptionPolicy::Disabled => dravity_tracker::CryptoSupport::None,
        EncryptionPolicy::Required => dravity_tracker::CryptoSupport::Required,
        // `Enabled`, and any later mode that still accepts encryption.
        _ => dravity_tracker::CryptoSupport::Supported,
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::time::Duration;

    use dravity_tracker::AnnounceResponse;

    use super::{
        AnnounceMerge, announce_port, announce_tier, crypto_support, dht_lookup_failed,
        error_chain, prepare_tiers, promote,
    };

    #[test]
    fn a_failed_batch_reports_only_the_blocks_that_failed() {
        use std::sync::Arc;

        use bytes::Bytes;
        use dravity_core::{BlockRef, Metainfo};
        use dravity_engine::{Event, PeerKey};
        use dravity_storage::MemoryStorage;

        use super::{Content, read_batch, write_batch};

        let pieces = vec![0u8; 40];
        let mut info = b"d6:lengthi40000e4:name1:a12:piece lengthi32768e6:pieces40:".to_vec();
        info.extend_from_slice(&pieces);
        info.push(b'e');
        let metainfo = Metainfo::from_info_bytes(&info, Vec::new()).unwrap();
        let content = Content {
            metainfo: Arc::new(metainfo),
            storage: Arc::new(MemoryStorage::new(40_000)),
        };
        let good = BlockRef {
            piece: 0,
            offset: 0,
            length: 16_384,
        };
        let bad = BlockRef {
            piece: 1,
            offset: 16_384,
            length: 16_384,
        };
        let data = Bytes::from(vec![7u8; 16_384]);
        let events = write_batch(&content, vec![(good, data.clone()), (bad, data.clone())]);
        assert_eq!(events[0], Event::BlockWritten { block: good });
        assert!(matches!(events[1], Event::WriteFailed { block, .. } if block == bad));

        let events = read_batch(&content, vec![(PeerKey(1), good), (PeerKey(2), bad)]);
        assert_eq!(
            events[0],
            Event::BlockRead {
                peer: PeerKey(1),
                block: good,
                data,
            }
        );
        assert!(matches!(
            events[1],
            Event::ReadFailed {
                peer: PeerKey(2),
                ..
            }
        ));
        assert_eq!(
            write_batch(&content, vec![(good, Bytes::from(vec![1u8; 16_384]))]),
            vec![Event::BlockWritten { block: good }]
        );
    }

    #[test]
    fn trackers_hear_the_encryption_policy() {
        use crate::EncryptionPolicy;
        use dravity_tracker::CryptoSupport;
        assert_eq!(
            crypto_support(EncryptionPolicy::Disabled),
            CryptoSupport::None
        );
        assert_eq!(
            crypto_support(EncryptionPolicy::Enabled),
            CryptoSupport::Supported
        );
        assert_eq!(
            crypto_support(EncryptionPolicy::Required),
            CryptoSupport::Required
        );
    }

    #[test]
    fn the_mapped_port_is_announced_before_the_listen_port() {
        assert_eq!(announce_port(Some(40_000), Some(6881), 6881), 40_000);
        assert_eq!(announce_port(None, Some(6882), 6881), 6882);
        assert_eq!(announce_port(None, None, 6881), 6881);
    }

    #[test]
    fn dht_failure_is_a_timeout_or_a_node_with_no_neighbors() {
        assert!(dht_lookup_failed(true, true, false));
        // Peers collected before the deadline are kept; the lookup still
        // retries soon because it did not finish.
        assert!(dht_lookup_failed(true, false, true));
        assert!(dht_lookup_failed(false, true, false), "not bootstrapped");
        assert!(!dht_lookup_failed(false, true, true), "empty swarm");
        assert!(!dht_lookup_failed(false, false, false));
    }

    #[test]
    fn tiers_drop_empty_and_promote_answers() {
        let tiers = prepare_tiers(vec![
            vec!["a".into(), "b".into(), "c".into()],
            Vec::new(),
            vec!["d".into()],
        ]);
        promote(&tiers, "c");
        promote(&tiers, "d");
        promote(&tiers, "missing");
        let tiers = tiers.lock().unwrap();
        assert_eq!(tiers.len(), 2, "empty tier dropped");
        assert_eq!(tiers[0][0], "c");
        let mut first: Vec<_> = tiers[0].clone();
        first.sort();
        assert_eq!(first, ["a", "b", "c"]);
        assert_eq!(tiers[1], ["d"]);
    }

    #[test]
    fn error_chain_includes_sources() {
        let inner = std::io::Error::other("connection refused");
        let outer = dravity_tracker::TrackerError::Io(inner);
        assert_eq!(error_chain(&outer), "I/O error: connection refused");
    }

    #[test]
    fn merge_honors_tracker_min_interval() {
        let mut merge = AnnounceMerge::default();
        merge.observe(
            "http://a",
            AnnounceResponse {
                interval: Some(Duration::from_secs(30)),
                min_interval: Some(Duration::from_secs(120)),
                warning: Some("slow".into()),
                ..AnnounceResponse::default()
            },
        );
        merge.observe(
            "http://b",
            AnnounceResponse {
                interval: Some(Duration::from_secs(1800)),
                min_interval: Some(Duration::from_secs(60)),
                peers: vec![SocketAddr::from(([1, 2, 3, 4], 1))],
                ..AnnounceResponse::default()
            },
        );
        // Soonest interval is 30s, strictest minimum is 120s.
        assert_eq!(merge.interval(), Some(Duration::from_secs(120)));
        assert_eq!(merge.peers.len(), 1);
        assert_eq!(merge.warnings, ["http://a: slow"]);

        let mut only_min = AnnounceMerge::default();
        only_min.observe(
            "http://c",
            AnnounceResponse {
                min_interval: Some(Duration::from_secs(90)),
                ..AnnounceResponse::default()
            },
        );
        assert_eq!(only_min.interval(), Some(Duration::from_secs(90)));
    }

    #[tokio::test]
    async fn tier_tries_the_next_tracker_and_promotes_it() {
        use std::sync::{Arc, Mutex};

        use dravity_core::{InfoHash, PeerId};
        use dravity_tracker::{AnnounceEvent, AnnounceRequest, ClientConfig, TrackerClient};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let good = format!("http://{}/announce", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let body = b"d8:completei1e10:incompletei0e8:intervali60e5:peers0:e";
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(body).await;
        });
        let dead_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead = format!("http://{}/announce", dead_listener.local_addr().unwrap());
        drop(dead_listener);
        let dead_url = dead.clone();
        let tiers = Arc::new(Mutex::new(vec![vec![dead.clone(), good.clone()]]));
        let client = TrackerClient::new(ClientConfig::default()).unwrap();
        let req = AnnounceRequest {
            info_hash: InfoHash::new([9; 20]),
            peer_id: PeerId::new(*b"-TS0001-testtesttest"),
            port: 6881,
            uploaded: 0,
            downloaded: 0,
            left: 1,
            event: AnnounceEvent::Started,
            num_want: Some(1),
            key: 7,
            crypto: dravity_tracker::CryptoSupport::None,
        };
        let (answer, errors) = announce_tier(
            client,
            Arc::clone(&tiers),
            vec![dead, good.clone()],
            req,
            Vec::new(),
        )
        .await;
        assert_eq!(answer.expect("fallback").0, good);
        assert_eq!(errors.len(), 1, "the dead tracker was tried");
        assert_eq!(errors[0].0, dead_url);
        assert_eq!(tiers.lock().unwrap()[0][0], good);
    }
}
