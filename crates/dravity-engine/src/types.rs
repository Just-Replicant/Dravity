//! Inputs, outputs and configuration of [`TorrentState`](crate::TorrentState).

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use dravity_core::BlockRef;
use dravity_wire::Message;

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
    /// Hashing existing data to find pieces that are already present.
    Checking,
    /// Fetching pieces from peers.
    Downloading,
    /// Every piece is present and verified.
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
    /// Outstanding block requests per unchoked peer (request pipeline depth).
    pub max_requests_per_peer: usize,
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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_peers: 50,
            max_requests_per_peer: 16,
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
}
