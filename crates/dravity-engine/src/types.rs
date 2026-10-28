//! Inputs, outputs and configuration of [`TorrentState`](crate::TorrentState).

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use bytes::Bytes;
use dravity_core::BlockRef;
use dravity_wire::{Message, Reserved};

/// Opaque handle for a connected peer, allocated by the driver.
///
/// The driver (`dravity-session`) owns the connection; the engine only ever
/// refers to it by this handle. Handles must not be reused while the engine
/// may still know the peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeerKey(pub u64);

/// Tracker announce event (BEP 3 `event` parameter).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnnounceEvent {
    /// Regular re-announce (no `event` parameter).
    None,
    /// First announce after the torrent starts downloading.
    Started,
    /// The download just completed.
    Completed,
    /// The torrent is stopping.
    Stopped,
}

/// Coarse lifecycle state of a torrent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TorrentStatus {
    /// Created but [`Event::Start`] not yet processed.
    Idle,
    /// Fetching the `info` dictionary from peers (magnet link, BEP 9).
    FetchingMetadata,
    /// Hashing existing data to find pieces that are already present.
    Checking,
    /// Fetching pieces from peers.
    Downloading,
    /// Every piece is present and verified. With [`Config::seed`] the
    /// torrent keeps announcing and uploads to peers (seeding).
    Finished,
    /// Stopped by command or after a fatal error.
    Stopped,
}

/// Tunables for a torrent. [`Config::default`] gives sensible values.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Config {
    /// Maximum simultaneously connected (or connecting) peers.
    pub max_peers: usize,
    /// Upper bound of a peer's AIMD request window (outstanding block
    /// requests). A smaller `reqq` from the peer's extension handshake wins.
    pub max_requests_per_peer: usize,
    /// Request window a new peer starts with; it grows while blocks arrive
    /// in time and halves when a request times out.
    pub initial_requests_per_peer: usize,
    /// Request a block from a second peer when the first has not delivered
    /// it after this long and the second has nothing else to do (piece
    /// stealing from slow peers).
    pub steal_after: Duration,
    /// Once every remaining block is requested, request blocks from more
    /// than one peer and cancel the duplicates as copies arrive (endgame).
    pub endgame: bool,
    /// Download rate limit in bytes per second (`None`: unlimited). Enforced
    /// by pacing block requests with a token bucket.
    pub download_rate_limit: Option<u64>,
    /// Exchange peers with other peers (BEP 11). Disable for private torrents.
    pub pex: bool,
    /// Minimum time between two PEX messages to the same peer (BEP 11: one
    /// minute).
    pub pex_interval: Duration,
    /// Ask the driver to look up peers in the DHT ([`Effect::DhtLookup`]).
    /// Disable for private torrents or when the driver runs no DHT.
    pub dht: bool,
    /// Time between DHT lookups for one torrent after a lookup finishes.
    pub dht_interval: Duration,
    /// Time before another DHT lookup when the previous one timed out or the
    /// node was not ready. Shorter than [`Self::dht_interval`].
    pub dht_retry: Duration,
    /// Largest `info` dictionary accepted from peers for a magnet link.
    pub max_metadata_size: u64,
    /// A request without an answer after this long is given to other peers.
    pub request_timeout: Duration,
    /// Drop a peer that sent nothing for this long.
    pub peer_timeout: Duration,
    /// Wait this long before connecting again to a peer that failed or dropped.
    /// Protocol rejects (wrong torrent, our own peer id, a malformed handshake)
    /// are not retried on this timer.
    pub peer_retry: Duration,
    /// Send a keep-alive after this long without sending anything.
    pub keepalive_interval: Duration,
    /// Re-announce interval when the tracker does not give one.
    pub default_announce_interval: Duration,
    /// Lower bound applied to tracker-provided intervals.
    pub min_announce_interval: Duration,
    /// Delay before retrying a failed announce.
    pub announce_retry: Duration,
    /// Blocks handed to storage but not yet written; no new requests are
    /// sent while this many writes are pending (disk backpressure).
    pub max_pending_writes: usize,
    /// Hash existing data on start to resume a partial download.
    pub check_existing: bool,
    /// Maximum concurrent hash jobs while checking.
    pub check_concurrency: u32,
    /// Keep the torrent in the swarm once it is complete: announce, accept
    /// connections and upload (seeding). When `false` a finished torrent
    /// disconnects its peers and only announces `completed`.
    pub seed: bool,
    /// Peers unchoked for their upload to us (tit-for-tat) while
    /// downloading, or for the fastest uploads while seeding. One optimistic
    /// unchoke comes on top when [`Self::optimistic_unchoke`] is set.
    pub upload_slots: usize,
    /// Give one extra slot to a choked peer in turn, so newcomers get data
    /// and better partners are discovered.
    pub optimistic_unchoke: bool,
    /// Time between two choker rounds (BEP 3 suggests ten seconds).
    pub unchoke_interval: Duration,
    /// Time between two changes of the optimistic unchoke.
    pub optimistic_unchoke_interval: Duration,
    /// A peer that holds our requests and sends no block for this long is
    /// snubbing us: it loses its regular slot (anti-snubbing).
    pub snub_timeout: Duration,
    /// Upload rate limit in bytes per second (`None`: unlimited). Paces
    /// block reads with a token bucket.
    pub upload_rate_limit: Option<u64>,
    /// Requests from one peer we queue before refusing more. Advertised as
    /// `reqq` in our extension handshake.
    pub max_upload_queue: usize,
    /// Block reads for uploads in flight at once across all peers (disk
    /// backpressure for seeding).
    pub max_pending_reads: usize,
    /// Advertise and use the BEP 6 fast extension (`have all`/`have none`,
    /// `reject`, `allowed fast`). The driver must set the reserved bit to
    /// match.
    pub fast_extension: bool,
    /// Size of the BEP 6 allowed fast set offered to fast-extension peers
    /// (0: offer none). Needs [`TorrentState::with_info_hash`](crate::TorrentState::with_info_hash).
    pub allowed_fast: usize,
    /// Ban a peer's IP address once it has this many strikes for pieces that
    /// failed the hash check (0: never ban). Sending every block of a failed
    /// piece is two strikes; sharing a failed piece with other peers is one.
    /// The default (4) forgives a single corrupt piece, which may be a
    /// transient disk or memory error on the sender.
    pub hash_fail_ban: u32,
    /// Port we accept connections on, advertised in the extension handshake
    /// (`p`) so peers that we dialed can tell others how to reach us.
    pub listen_port: Option<u16>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_peers: 50,
            max_requests_per_peer: 128,
            initial_requests_per_peer: 4,
            steal_after: Duration::from_secs(10),
            endgame: true,
            download_rate_limit: None,
            pex: true,
            pex_interval: Duration::from_secs(60),
            dht: false,
            dht_interval: Duration::from_secs(5 * 60),
            dht_retry: Duration::from_secs(15),
            max_metadata_size: 8 * 1024 * 1024,
            request_timeout: Duration::from_secs(30),
            peer_timeout: Duration::from_secs(180),
            peer_retry: Duration::from_secs(30),
            keepalive_interval: Duration::from_secs(90),
            default_announce_interval: Duration::from_secs(30 * 60),
            min_announce_interval: Duration::from_secs(60),
            announce_retry: Duration::from_secs(60),
            max_pending_writes: 512,
            check_existing: true,
            check_concurrency: 4,
            seed: true,
            upload_slots: 4,
            optimistic_unchoke: true,
            unchoke_interval: Duration::from_secs(10),
            optimistic_unchoke_interval: Duration::from_secs(30),
            snub_timeout: Duration::from_secs(60),
            upload_rate_limit: None,
            max_upload_queue: 250,
            max_pending_reads: 64,
            fast_extension: true,
            allowed_fast: dravity_wire::DEFAULT_ALLOWED_FAST,
            hash_fail_ban: 4,
            listen_port: None,
        }
    }
}

/// Something that happened, fed to [`TorrentState::update`](crate::TorrentState::update).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// Start the torrent (check existing data, then announce and download).
    Start,
    /// Stop the torrent: disconnect peers and announce `stopped`.
    Stop,
    /// Periodic timer (the driver should tick about once per second).
    Tick,
    /// An outgoing connection requested by [`Effect::Connect`] completed its
    /// handshake (info-hash verified by the driver).
    PeerConnected {
        /// Handle chosen by the driver.
        peer: PeerKey,
        /// Remote address.
        addr: SocketAddr,
        /// Reserved bytes from the peer's handshake (extension flags).
        reserved: Reserved,
    },
    /// A peer connected to us and completed its handshake for this torrent
    /// (incoming connection). The engine may refuse it with
    /// [`Effect::Disconnect`].
    PeerAccepted {
        /// Handle chosen by the driver.
        peer: PeerKey,
        /// Remote address (the source port is usually not the peer's
        /// listening port).
        addr: SocketAddr,
        /// Reserved bytes from the peer's handshake (extension flags).
        reserved: Reserved,
    },
    /// An [`Effect::Connect`] attempt failed (refused, timeout, bad handshake).
    PeerConnectFailed {
        /// Address that failed.
        addr: SocketAddr,
        /// When `true`, try the address again after
        /// [`Config::peer_retry`](crate::Config::peer_retry). Protocol rejects
        /// pass `false` so a bad peer is not dialed in a loop.
        retry: bool,
    },
    /// A connected peer went away (EOF, I/O error, protocol error).
    PeerDisconnected {
        /// The peer.
        peer: PeerKey,
    },
    /// A peer sent a message.
    Message {
        /// The peer.
        peer: PeerKey,
        /// The decoded message.
        message: Message,
    },
    /// A block from [`Effect::WriteBlock`] is on storage.
    BlockWritten {
        /// The block.
        block: BlockRef,
    },
    /// A block from [`Effect::WriteBlock`] could not be written. Fatal.
    WriteFailed {
        /// The block.
        block: BlockRef,
        /// Human-readable cause.
        error: String,
    },
    /// Result of an [`Effect::HashPiece`]. `ok` is `false` when the data does
    /// not match or was never written. A real storage failure is
    /// [`Event::StorageFailed`], not `ok: false`.
    PieceHashed {
        /// Piece index.
        piece: u32,
        /// Whether the SHA-1 matched.
        ok: bool,
    },
    /// A block from [`Effect::ReadBlock`] was read from storage.
    BlockRead {
        /// The peer that asked for it.
        peer: PeerKey,
        /// Block address.
        block: BlockRef,
        /// Block data.
        data: Bytes,
    },
    /// An [`Effect::ReadBlock`] failed. Fatal: a piece we verified cannot be
    /// read back.
    ReadFailed {
        /// The peer that asked for it.
        peer: PeerKey,
        /// Block address.
        block: BlockRef,
        /// Human-readable cause.
        error: String,
    },
    /// Reading or hashing a piece failed for a reason other than missing data.
    /// Fatal: the torrent stops.
    StorageFailed {
        /// Human-readable cause.
        error: String,
    },
    /// An announce succeeded.
    Announced {
        /// Peers returned by the tracker.
        peers: Vec<SocketAddr>,
        /// Tracker-requested re-announce interval.
        interval: Option<Duration>,
    },
    /// Peers learned from elsewhere (user input, PEX, DHT). Unlike
    /// [`Event::Announced`] this does not affect announce scheduling.
    AddPeers {
        /// Peer addresses.
        peers: Vec<SocketAddr>,
    },
    /// Every tracker failed for an announce.
    AnnounceFailed {
        /// Human-readable cause.
        error: String,
    },
    /// An [`Effect::DhtLookup`] finished (possibly with no peers).
    DhtPeers {
        /// Peers found before the lookup ended. A timed-out lookup still
        /// returns whatever had already arrived.
        peers: Vec<SocketAddr>,
        /// The lookup timed out, or it finished with no peers because the
        /// node still has no DHT neighbors. The engine asks again after
        /// [`Config::dht_retry`] instead of waiting [`Config::dht_interval`].
        /// An empty result from a node that does have neighbors is an empty
        /// swarm and uses the longer interval.
        failed: bool,
    },
}

/// Something the driver must do. Effects never block the engine; their
/// results come back as [`Event`]s.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Effect {
    /// Open a connection and perform the handshake.
    Connect {
        /// Peer address.
        addr: SocketAddr,
    },
    /// Queue a message for a peer.
    Send {
        /// Destination.
        peer: PeerKey,
        /// Message.
        message: Message,
    },
    /// Close a peer connection. No [`Event::PeerDisconnected`] is needed.
    Disconnect {
        /// The peer.
        peer: PeerKey,
    },
    /// Write a received block to storage, then report [`Event::BlockWritten`].
    WriteBlock {
        /// Block address.
        block: BlockRef,
        /// Block data.
        data: Bytes,
    },
    /// Read a block a peer asked for, then report [`Event::BlockRead`] or
    /// [`Event::ReadFailed`].
    ReadBlock {
        /// The peer to upload to.
        peer: PeerKey,
        /// Block address.
        block: BlockRef,
    },
    /// Hash a piece from storage, then report [`Event::PieceHashed`].
    HashPiece {
        /// Piece index.
        piece: u32,
    },
    /// Announce to the torrent's trackers, then report [`Event::Announced`]
    /// or [`Event::AnnounceFailed`].
    Announce {
        /// Event parameter.
        event: AnnounceEvent,
        /// Payload bytes uploaded.
        uploaded: u64,
        /// Payload bytes downloaded.
        downloaded: u64,
        /// Bytes still missing.
        left: u64,
    },
    /// Look the torrent up in the DHT, then report [`Event::DhtPeers`].
    DhtLookup,
    /// The `info` dictionary of a magnet link was downloaded and matches the
    /// info-hash. Only produced by [`MetadataState`](crate::MetadataState).
    MetadataReceived {
        /// The bencoded `info` dictionary.
        info: Bytes,
    },
    /// Notify the application.
    Alert(Alert),
}

/// Notifications for the application (forwarded by the session).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Alert {
    /// The torrent changed state.
    StateChanged(TorrentStatus),
    /// Periodic progress snapshot (one per [`Event::Tick`]).
    Stats(Stats),
    /// A piece passed its hash check.
    PieceCompleted {
        /// Piece index.
        piece: u32,
    },
    /// A downloaded piece failed its hash check and will be downloaded again.
    HashFailed {
        /// Piece index.
        piece: u32,
    },
    /// An announce failed; it will be retried.
    TrackerError {
        /// Cause.
        message: String,
    },
    /// Every piece is present and verified.
    Finished,
    /// The `info` dictionary of a magnet link was received and verified.
    MetadataReceived,
    /// A peer was banned for sending data that failed the hash check. Its
    /// connections are closed and its address is not used again.
    PeerBanned {
        /// The banned address.
        ip: IpAddr,
    },
    /// A fatal error stopped the torrent.
    Error {
        /// Cause.
        message: String,
    },
}

/// Progress snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Stats {
    /// Current state.
    pub status: TorrentStatus,
    /// Total pieces.
    pub piece_count: u32,
    /// Verified pieces.
    pub pieces_have: u32,
    /// Total content bytes.
    pub total_length: u64,
    /// Verified content bytes.
    pub bytes_have: u64,
    /// Payload bytes received from peers (including discarded data).
    pub downloaded: u64,
    /// Payload bytes sent to peers.
    pub uploaded: u64,
    /// Connected peers.
    pub peers: usize,
    /// Connection attempts in flight.
    pub connecting: usize,
    /// Peer addresses waiting to be dialed (queued, or scheduled for retry).
    pub known_peers: usize,
    /// Blocks waiting to be written to storage.
    pub pending_writes: usize,
    /// Peers we are uploading to (unchoked by us).
    pub unchoked: usize,
    /// Peers that are interested in our pieces.
    pub interested: usize,
    /// Block reads for uploads in flight.
    pub pending_reads: usize,
    /// IP addresses banned for bad data.
    pub banned: usize,
}
