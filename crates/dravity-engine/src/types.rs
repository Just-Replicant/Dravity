//! Inputs, outputs and configuration of [`TorrentState`](crate::TorrentState).

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use bytes::Bytes;
use dravity_core::BlockRef;
use dravity_core::merkle::Hash;
use dravity_wire::{Message, Reserved};

use crate::picker::Priority;

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
    /// Every wanted piece is present and verified (every piece, unless
    /// some are [`Priority::Skip`]). With [`Config::seed`] the torrent keeps
    /// announcing and uploads what it has to peers (seeding). Wanting more
    /// pieces ([`Event::SetPriorities`], [`Event::SetUrgent`]) returns it to
    /// [`TorrentStatus::Downloading`].
    Finished,
    /// Stopped by command or after a fatal error.
    Stopped,
}

/// How a seeding torrent ranks peers for its regular upload slots.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SeedChoking {
    /// The peers we uploaded the most to last round: slots go to whoever
    /// takes data fastest (the default).
    #[default]
    FastestUpload,
    /// Peers take turns: a peer keeps its slot until it received
    /// [`Config::round_robin_quota`] bytes, then the slot goes to the peer
    /// that has been choked longest. Spreads a seed evenly.
    RoundRobin,
    /// Prefer peers that are just starting or nearly done over peers in
    /// the middle of the download: new peers get their first pieces fast
    /// and finishing peers complete (and seed) sooner, while peers that
    /// only take data mid-download get less.
    AntiLeech,
}

impl SeedChoking {
    /// The mode's name, as [`std::str::FromStr`] reads it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FastestUpload => "fastest-upload",
            Self::RoundRobin => "round-robin",
            Self::AntiLeech => "anti-leech",
        }
    }
}

impl std::str::FromStr for SeedChoking {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "fastest-upload" | "fastest" => Ok(Self::FastestUpload),
            "round-robin" => Ok(Self::RoundRobin),
            "anti-leech" => Ok(Self::AntiLeech),
            _ => Err(format!(
                "unknown seed choking `{s}` (fastest-upload, round-robin, anti-leech)"
            )),
        }
    }
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
    /// in time and halves when a request times out. A peer whose measured
    /// bandwidth-delay product (delivery rate × base round trip) is deeper
    /// gets that depth at once, still within `max_requests_per_peer`.
    pub initial_requests_per_peer: usize,
    /// Block requests outstanding across all peers of the torrent. Many
    /// peers then share a bounded amount of requested-but-undelivered data
    /// instead of each filling its own window.
    pub request_budget: usize,
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
    /// Exchange tracker URLs with peers (`lt_tex`). Disable for private
    /// torrents.
    pub tracker_exchange: bool,
    /// BEP 16 super-seeding while the torrent is complete: peers see no
    /// bitfield, only one piece at a time chosen from the least shared,
    /// and get the next once they announce it. Speeds up initial seeding.
    pub super_seed: bool,
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
    /// How long a hybrid magnet waits for its v2 piece layers without any
    /// layer block arriving before the download starts on SHA-1 alone for
    /// the files still missing one (a v2-only magnet always waits: it has
    /// no other way to check its pieces).
    pub piece_layer_timeout: Duration,
    /// A v2-only magnet whose piece layers have not progressed for this
    /// long raises [`Alert::PieceLayersStalled`] (and again after each
    /// further period of silence). It keeps waiting and asking.
    pub piece_layer_stall_alert: Duration,
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
    /// How regular slots are ranked while seeding (see [`SeedChoking`]).
    pub seed_choking: SeedChoking,
    /// Bytes a peer receives per turn under [`SeedChoking::RoundRobin`]
    /// before its slot passes on.
    pub round_robin_quota: u64,
    /// Tune [`Self::upload_slots`] every round from the measured upload
    /// rates: one more slot for each peer that takes more than an
    /// increasing threshold (1 KiB/s, 2 KiB/s, …), so slots grow while
    /// extra peers still add upload and stop when they would only split
    /// it. `upload_slots` is the starting value.
    pub upload_slots_auto: bool,
    /// Time between two choker rounds (BEP 3 suggests ten seconds).
    pub unchoke_interval: Duration,
    /// Time between two changes of the optimistic unchoke.
    pub optimistic_unchoke_interval: Duration,
    /// A peer that holds our requests and sends no block for this long is
    /// snubbing us: it loses its regular slot (anti-snubbing) and can still
    /// receive the optimistic unchoke.
    pub snub_timeout: Duration,
    /// Upload rate limit in bytes per second (`None`: unlimited). Paces
    /// block reads with a token bucket. A read that is choked, cancelled,
    /// or dropped before the bytes go out returns its tokens.
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
    /// transient disk or memory error on the sender. Strikes older than
    /// [`Self::hash_fail_quiet`] are forgotten; a ban lasts for the torrent.
    /// An IPv4-mapped IPv6 address is the same key as its IPv4 address.
    pub hash_fail_ban: u32,
    /// Forget an address's hash-failure strikes after this long with no new
    /// failure, so a shared NAT is not banned for mistakes spread over the
    /// whole download. Bans themselves are not lifted.
    pub hash_fail_quiet: Duration,
    /// Smart banning (default `true`): when a piece with several
    /// contributors fails, find out which block was bad instead of striking
    /// everyone. The driver hashes the failed piece's blocks
    /// ([`Effect::HashBlocks`]) and again once the piece passes; the sender
    /// of a block whose digest differs from the verified one is banned at
    /// once. For v2 and hybrid torrents the 16 KiB leaf hashes are also
    /// asked from BEP 52 peers, so the verdict can come before the piece is
    /// downloaded again. Sole contributors are still handled by strikes.
    pub smart_ban: bool,
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
            request_budget: 1024,
            steal_after: Duration::from_secs(10),
            endgame: true,
            download_rate_limit: None,
            pex: true,
            tracker_exchange: true,
            super_seed: false,
            pex_interval: Duration::from_secs(60),
            dht: false,
            dht_interval: Duration::from_secs(5 * 60),
            dht_retry: Duration::from_secs(15),
            max_metadata_size: 8 * 1024 * 1024,
            piece_layer_timeout: Duration::from_secs(20),
            piece_layer_stall_alert: Duration::from_secs(120),
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
            seed_choking: SeedChoking::FastestUpload,
            round_robin_quota: 4 * 1024 * 1024,
            upload_slots_auto: false,
            unchoke_interval: Duration::from_secs(10),
            optimistic_unchoke_interval: Duration::from_secs(30),
            snub_timeout: Duration::from_secs(60),
            upload_rate_limit: None,
            max_upload_queue: 250,
            max_pending_reads: 64,
            fast_extension: true,
            allowed_fast: dravity_wire::DEFAULT_ALLOWED_FAST,
            hash_fail_ban: 4,
            hash_fail_quiet: Duration::from_secs(60 * 60),
            smart_ban: true,
            listen_port: None,
        }
    }
}

/// What the driver observed about a connection: shared with other peers
/// as BEP 11 PEX flags and remembered for dialing the address again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct PeerLink {
    /// The connection runs over uTP (BEP 29).
    pub utp: bool,
    /// Message Stream Encryption: `Some(true)` when the connection completed
    /// an MSE handshake (RC4 or its plaintext method), `Some(false)` when we
    /// offered MSE and the peer refused it, `None` when MSE was not tried.
    pub mse: Option<bool>,
}

/// What is known about a peer before it is dialed, from BEP 11 PEX flags or
/// an earlier connection; see [`Effect::Connect`]. `None` means unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct DialHints {
    /// Supports Message Stream Encryption: PEX flag `0x01`, or an MSE
    /// handshake seen (`Some(true)`) or refused (`Some(false)`).
    pub encryption: Option<bool>,
    /// Supports uTP: PEX flag `0x04`, or an earlier uTP connection.
    pub utp: Option<bool>,
}

impl DialHints {
    /// Hints from a peer's BEP 11 `added.f` flags (`None`: the sender gave
    /// none).
    #[must_use]
    pub fn from_pex_flags(flags: Option<u8>) -> Self {
        Self {
            encryption: flags.map(|f| f & dravity_wire::PEX_ENCRYPTION != 0),
            utp: flags.map(|f| f & dravity_wire::PEX_UTP != 0),
        }
    }

    /// Hints from a connection we made to the peer. A uTP connection proves
    /// uTP support; a TCP one says nothing about it.
    #[must_use]
    pub fn from_link(link: PeerLink) -> Self {
        Self {
            encryption: link.mse,
            utp: link.utp.then_some(true),
        }
    }

    /// `self`, with the fields it does not know taken from `other`.
    #[must_use]
    pub fn or(self, other: Self) -> Self {
        Self {
            encryption: self.encryption.or(other.encryption),
            utp: self.utp.or(other.utp),
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
        /// Transport and encryption of the connection.
        link: PeerLink,
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
        /// Transport and encryption of the connection.
        link: PeerLink,
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
    /// An [`Effect::Connect`] was not started because the session-wide
    /// connection cap was reached. The address is not blamed: it is
    /// dialed again after [`DEFERRED_DIAL_RETRY`](crate::DEFERRED_DIAL_RETRY).
    PeerConnectDeferred {
        /// Address that was not dialed.
        addr: SocketAddr,
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
    /// Result of an [`Effect::HashBlocks`]: one digest per block, in order.
    /// Empty when the piece could not be read.
    BlockHashes {
        /// Piece index.
        piece: u32,
        /// SHA-256 leaf hash of each block.
        hashes: Vec<Hash>,
    },
    /// Ban an address banned elsewhere in the session (another torrent's
    /// hash-failure or smart ban): its connections are closed and it is
    /// never dialed or accepted again. No [`Alert::PeerBanned`] is raised.
    BanIp {
        /// The address (an IPv4-mapped IPv6 address is its IPv4 address).
        ip: IpAddr,
    },
    /// The driver installed a piece layer from
    /// [`Effect::PieceLayerReceived`]: hash the verified pieces in `pieces`
    /// again (each with [`Effect::HashPiece`], now against the Merkle tree
    /// too). A piece that fails is downloaded again.
    RecheckPieces {
        /// The file's pieces (those not verified yet are skipped).
        pieces: std::ops::Range<u32>,
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
    /// An [`Effect::ReadBlock`] failed. The request is dropped and, when the
    /// peer speaks the fast extension, rejected. The torrent keeps running.
    /// A failure of storage itself is [`Event::StorageFailed`].
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
    /// Sets the download priority of every piece (one entry per piece;
    /// any other length is ignored). [`Priority::Skip`] pieces are not
    /// downloaded; the torrent is [`TorrentStatus::Finished`] once every
    /// other piece is verified. The session derives these from file
    /// priorities.
    SetPriorities {
        /// Priority per piece.
        priorities: Vec<Priority>,
    },
    /// Replaces the urgent pieces: they are requested before anything else,
    /// in this order, even when skipped (streaming reads). An empty list
    /// clears it.
    SetUrgent {
        /// Pieces, most urgent first.
        pieces: Vec<u32>,
    },
    /// The port published to trackers and the DHT changed. When the torrent
    /// is in the swarm, announce again so peers are told the current port.
    /// An announce already in flight is followed by one more.
    Reannounce,
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
        /// What is known about the peer: which transport and encryption
        /// to try first, or whether to dial it at all.
        hints: DialHints,
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
    /// Hash every 16 KiB block of a piece as it is on storage with
    /// [`merkle::hash_leaf`](dravity_core::merkle::hash_leaf) (the last
    /// block may be shorter), then report [`Event::BlockHashes`]. Used by
    /// [`Config::smart_ban`].
    HashBlocks {
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
        /// Piece layers fetched from peers with BEP 52 hash requests, by
        /// pieces root, each verified against its root. Empty unless the
        /// magnet link is v2-only and has files larger than a piece.
        piece_layers: Vec<(Hash, Vec<Hash>)>,
    },
    /// A hybrid torrent that started on SHA-1 alone fetched a missing piece
    /// layer from peers, verified against its pieces root. The driver adds
    /// it to the metainfo (and persists it), then replies with
    /// [`Event::RecheckPieces`] for the file's pieces. The engine already
    /// serves the layer to peers' hash requests.
    PieceLayerReceived {
        /// The file's pieces root.
        root: Hash,
        /// One hash per piece of the file.
        layer: Vec<Hash>,
    },
    /// A peer shared tracker URLs we did not know (`lt_tex`). Each passed
    /// [`dravity_wire::is_exchangeable_tracker`]; the driver may add them
    /// to the torrent's announce list.
    TrackersReceived {
        /// New tracker URLs.
        urls: Vec<String>,
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
    /// A block could not be read back for upload. The request is dropped
    /// and the torrent keeps running.
    UploadFailed {
        /// Piece that could not be read.
        piece: u32,
        /// Cause.
        message: String,
    },
    /// Every wanted piece is present and verified.
    Finished,
    /// The `info` dictionary of a magnet link was received and verified.
    MetadataReceived,
    /// A v2-only magnet's metadata is verified, but no peer has sent any of
    /// its missing piece layers for [`Config::piece_layer_stall_alert`]
    /// (BEP 52 hash requests). The download cannot start without them; the
    /// torrent keeps asking every peer that connects.
    PieceLayersStalled {
        /// Files whose piece layer is still incomplete.
        missing_layers: u32,
        /// Time since the last layer data arrived (or since the metadata
        /// was verified).
        silent: Duration,
    },
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
    /// Bytes of wanted (not skipped, or urgent) pieces still missing. Equal
    /// to `total_length - bytes_have` when nothing is skipped.
    pub wanted_left: u64,
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

/// A copy of what the engine keeps about one connected peer
/// ([`TorrentState::peers`](crate::TorrentState::peers),
/// [`MetadataState::peers`](crate::MetadataState::peers)).
///
/// Byte counters are totals since the peer connected, so a client derives
/// rates from two snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PeerInfo {
    /// The driver's handle.
    pub key: PeerKey,
    /// Remote address.
    pub addr: SocketAddr,
    /// The peer connected to us.
    pub incoming: bool,
    /// Pieces the peer has (0 while a magnet's metadata is unknown).
    pub pieces_have: u32,
    /// Pieces of the torrent (0 while a magnet's metadata is unknown).
    pub piece_count: u32,
    /// Payload bytes received from the peer.
    pub downloaded: u64,
    /// Payload bytes sent to the peer.
    pub uploaded: u64,
    /// We refuse to upload to the peer.
    pub am_choking: bool,
    /// We want pieces the peer has.
    pub am_interested: bool,
    /// The peer refuses to upload to us.
    pub peer_choking: bool,
    /// The peer wants pieces we have.
    pub peer_interested: bool,
}
