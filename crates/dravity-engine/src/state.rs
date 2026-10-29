//! The per-torrent state machine.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use bytes::Bytes;
use dravity_core::{BLOCK_SIZE, BlockRef, InfoHash, PieceLayout};
use dravity_wire::{
    BlockRequest, ExtendedHandshake, METADATA_PIECE_LEN, Message, MetadataMessage, Reserved,
};

use crate::ban::HashBans;
use crate::bitfield::Bitfield;
use crate::choker::{Candidate, Choker};
use crate::ext::{self, PeerExtensions};
use crate::picker::{Picker, Priority, Received};
use crate::pool::PeerPool;
use crate::rate::TokenBucket;
use crate::types::{Alert, AnnounceEvent, Config, Effect, Event, PeerKey, Stats, TorrentStatus};
use crate::window::Window;

/// Most peers that may hold a request for the same block (endgame and
/// stealing): the original requester plus one more.
const MAX_BLOCK_OWNERS: usize = 2;

/// Suggestions (BEP 6 `suggest`) remembered per peer.
const MAX_SUGGESTED: usize = 16;

/// Messages that may arrive before `bitfield` / `have all` / `have none`.
///
/// BEP 3 says the bitfield is the first message, but libtorrent-family clients
/// send a BEP 10 extended handshake (and sometimes a keep-alive or DHT `port`)
/// first. Treating those as "already spoke" would drop the peer.
fn message_defers_bitfield(message: &Message) -> bool {
    matches!(
        message,
        Message::KeepAlive | Message::Port(_) | Message::Extended { .. } | Message::Unknown { .. }
    )
}

/// What the engine knows about one connected peer.
#[derive(Debug, Clone)]
struct Peer {
    addr: SocketAddr,
    /// The peer connected to us (its source port is not its listen port).
    incoming: bool,
    /// Where other peers can reach this one (for PEX): the address we
    /// dialed, or an incoming peer's IP with the `p` of its extension
    /// handshake.
    reachable: Option<SocketAddr>,
    has: Bitfield,
    /// `bitfield`, `have all`, and `have none` are still legal. Keep-alives,
    /// DHT `port`, and BEP 10 extended messages do not clear this: common
    /// clients send the extended handshake before the bitfield.
    bitfield_ok: bool,
    am_interested: bool,
    peer_choking: bool,
    peer_interested: bool,
    inflight: Vec<(BlockRef, Instant)>,
    /// Blocks this peer was asked for, including ones that timed out, so a
    /// late `piece` is accepted and an unsolicited one is ignored.
    asked: HashSet<(u32, u32)>,
    window: Window,
    /// The peer set the BEP 10 bit in its handshake.
    extended: bool,
    /// Both sides speak the BEP 6 fast extension.
    fast: bool,
    ext: PeerExtensions,
    /// Addresses we told this peer about with PEX.
    pex_known: HashSet<SocketAddr>,
    last_recv: Instant,
    last_sent: Instant,
    /// We refuse to upload to the peer (initially true).
    am_choking: bool,
    /// Pieces the peer lets us request while it chokes us (BEP 6).
    allowed_in: HashSet<u32>,
    /// Pieces we let the peer request while we choke it (BEP 6).
    allowed_out: HashSet<u32>,
    /// Pieces the peer suggested (BEP 6), oldest first, at most
    /// [`MAX_SUGGESTED`]. Preferred when a new piece is started.
    suggested: VecDeque<u32>,
    /// Requests from the peer waiting for a disk read.
    upload_queue: VecDeque<BlockRef>,
    /// Requests whose read is in flight. A cancel, a choke, or a drop
    /// removes the block and refunds its upload tokens; the data is dropped
    /// when it arrives.
    reading: HashSet<BlockRef>,
    /// Payload bytes received from the peer since the last choker round.
    down_round: u64,
    /// Payload bytes sent to the peer since the last choker round.
    up_round: u64,
    /// Since when the peer has held our requests without delivering a
    /// block (snub detection). A request timeout does not reset it.
    snub_since: Option<Instant>,
    /// When the peer last held the optimistic unchoke (or connected).
    last_optimistic: Instant,
}

impl Peer {
    fn new(
        addr: SocketAddr,
        incoming: bool,
        reserved: Reserved,
        piece_count: u32,
        config: &Config,
        now: Instant,
    ) -> Self {
        let cap = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        Self {
            addr,
            incoming,
            reachable: (!incoming).then_some(addr),
            has: Bitfield::new(piece_count),
            bitfield_ok: true,
            am_interested: false,
            peer_choking: true,
            peer_interested: false,
            inflight: Vec::new(),
            asked: HashSet::new(),
            window: Window::new(
                cap(config.initial_requests_per_peer),
                cap(config.max_requests_per_peer),
            ),
            extended: reserved.extension_protocol(),
            fast: config.fast_extension && reserved.fast(),
            ext: PeerExtensions::default(),
            pex_known: HashSet::new(),
            last_recv: now,
            last_sent: now,
            am_choking: true,
            allowed_in: HashSet::new(),
            allowed_out: HashSet::new(),
            suggested: VecDeque::new(),
            upload_queue: VecDeque::new(),
            reading: HashSet::new(),
            down_round: 0,
            up_round: 0,
            snub_since: None,
            last_optimistic: now,
        }
    }

    /// The peer holds requests and delivered nothing for `timeout`.
    fn is_snubbing(&self, now: Instant, timeout: Duration) -> bool {
        self.snub_since
            .is_some_and(|since| now.saturating_duration_since(since) >= timeout)
    }

    /// Whether the peer may receive `piece` from us right now.
    fn may_download(&self, piece: u32) -> bool {
        !self.am_choking || self.allowed_out.contains(&piece)
    }

    /// Outstanding requests allowed now: the AIMD window, capped by the
    /// peer's advertised `reqq`.
    fn request_limit(&self) -> usize {
        let window = self.window.size().min(self.ext.reqq.unwrap_or(u32::MAX));
        usize::try_from(window).unwrap_or(usize::MAX)
    }
}

/// Progress of the initial data check.
#[derive(Debug, Clone, Copy)]
struct Check {
    next: u32,
    outstanding: u32,
}

/// When to ask the driver for DHT peers.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DhtSchedule {
    next: Option<Instant>,
    in_flight: bool,
}

impl DhtSchedule {
    /// Emits [`Effect::DhtLookup`] if one is due.
    pub(crate) fn poll(&mut self, now: Instant, config: &Config, out: &mut Vec<Effect>) {
        if config.dht && !self.in_flight && self.next.is_none_or(|t| now >= t) {
            self.in_flight = true;
            out.push(Effect::DhtLookup);
        }
    }

    /// A lookup finished. `failed` schedules the shorter [`Config::dht_retry`].
    pub(crate) fn done(&mut self, now: Instant, config: &Config, failed: bool) {
        self.in_flight = false;
        let wait = if failed {
            config.dht_retry
        } else {
            config.dht_interval
        };
        self.next = Some(now + wait);
    }
}

/// Sans-IO state of one torrent: `update(now, Event) -> Vec<Effect>`.
///
/// The engine performs no I/O and reads no clock: the driver passes `now`
/// with every event, executes the returned [`Effect`]s, and feeds their
/// completions back as events. That makes every behavior reproducible in
/// plain unit tests.
///
/// ```
/// use std::time::Instant;
/// use dravity_core::PieceLayout;
/// use dravity_engine::{AnnounceEvent, Config, Effect, Event, TorrentState};
///
/// let layout = PieceLayout::new(16 * 1024, 40_000).unwrap();
/// let mut config = Config::default();
/// config.check_existing = false;
/// let mut torrent = TorrentState::new(layout, config);
/// let effects = torrent.update(Instant::now(), Event::Start);
/// assert!(effects.iter().any(|e| matches!(
///     e,
///     Effect::Announce { event: AnnounceEvent::Started, left: 40_000, .. }
/// )));
/// ```
#[derive(Debug)]
pub struct TorrentState {
    config: Config,
    picker: Picker,
    status: TorrentStatus,
    peers: HashMap<PeerKey, Peer>,
    pool: PeerPool,
    /// Peers holding a request for each in-flight block, with request times.
    owners: HashMap<BlockRef, Vec<(PeerKey, Instant)>>,
    check: Option<Check>,
    next_announce: Option<Instant>,
    announce_in_flight: bool,
    started_announced: bool,
    /// `completed` was announced; it is sent once, when every piece is in.
    completed_announced: bool,
    /// The announce the driver is currently performing.
    pending_announce: Option<AnnounceEvent>,
    /// DHT and PEX addresses refused after a magnet turns out private.
    swarm_ban: HashSet<SocketAddr>,
    dht: DhtSchedule,
    next_pex: Option<Instant>,
    rate: Option<TokenBucket>,
    /// The bencoded `info` dictionary, served to peers with BEP 9.
    info_bytes: Option<Bytes>,
    /// Needed for the BEP 6 allowed fast set.
    info_hash: Option<InfoHash>,
    choker: Choker,
    bans: HashBans,
    upload_rate: Option<TokenBucket>,
    /// Block reads for uploads in flight.
    pending_reads: usize,
    /// Last peer served by the upload round-robin.
    upload_cursor: Option<PeerKey>,
    downloaded: u64,
    uploaded: u64,
    bytes_have: u64,
    pending_writes: usize,
}

impl TorrentState {
    /// A torrent with no pieces yet.
    #[must_use]
    pub fn new(layout: PieceLayout, config: Config) -> Self {
        Self::with_have(layout, Bitfield::new(layout.piece_count()), config)
    }

    /// A torrent that already has (verified) `have` pieces, e.g. from resume
    /// data. A bitfield of the wrong length is ignored.
    #[must_use]
    pub fn with_have(layout: PieceLayout, have: Bitfield, config: Config) -> Self {
        let picker = Picker::new(layout, have);
        let bytes_have = picker
            .have()
            .ones()
            .map(|p| u64::from(layout.piece_size(p).unwrap_or(0)))
            .sum();
        Self {
            config,
            picker,
            status: TorrentStatus::Idle,
            peers: HashMap::new(),
            pool: PeerPool::default(),
            owners: HashMap::new(),
            check: None,
            next_announce: None,
            announce_in_flight: false,
            started_announced: false,
            completed_announced: false,
            pending_announce: None,
            swarm_ban: HashSet::new(),
            dht: DhtSchedule::default(),
            next_pex: None,
            rate: None,
            info_bytes: None,
            info_hash: None,
            choker: Choker::default(),
            bans: HashBans::default(),
            upload_rate: None,
            pending_reads: 0,
            upload_cursor: None,
            downloaded: 0,
            uploaded: 0,
            bytes_have,
            pending_writes: 0,
        }
    }

    /// Serves `info` (the torrent's bencoded `info` dictionary) to peers that
    /// ask for it with BEP 9, and advertises its size.
    #[must_use]
    pub fn with_info_bytes(mut self, info: Bytes) -> Self {
        self.info_bytes = Some(info);
        self
    }

    /// The torrent's info-hash, used for the BEP 6 allowed fast set.
    #[must_use]
    pub const fn with_info_hash(mut self, info_hash: InfoHash) -> Self {
        self.info_hash = Some(info_hash);
        self
    }

    /// Records that a `started` announce was already sent for this torrent
    /// (by the [`MetadataState`](crate::MetadataState) that fetched its
    /// metadata), so the first announce is a regular one.
    #[must_use]
    pub const fn with_started_announced(mut self, started: bool) -> Self {
        self.started_announced = started;
        self
    }

    /// A `started` announce is still waiting for the tracker. No second one
    /// is sent; [`Event::AnnounceFailed`] clears the flag so it is retried.
    #[must_use]
    pub fn with_started_pending(mut self, pending: bool) -> Self {
        if pending {
            self.started_announced = true;
            self.announce_in_flight = true;
            self.pending_announce = Some(AnnounceEvent::Started);
        }
        self
    }

    /// Starts with these piece priorities (see [`Event::SetPriorities`]).
    /// A slice of the wrong length is ignored.
    #[must_use]
    pub fn with_priorities(mut self, priorities: &[Priority]) -> Self {
        self.picker.set_priorities(priorities);
        self
    }

    /// The piece picker (priorities, availability, urgent pieces).
    #[must_use]
    pub const fn picker(&self) -> &Picker {
        &self.picker
    }

    /// Addresses whose TCP handshake is already in progress.
    #[must_use]
    pub fn with_connecting(mut self, addrs: impl IntoIterator<Item = SocketAddr>) -> Self {
        self.pool.adopt_connecting(addrs);
        self
    }

    /// Addresses that must not be dialed (DHT/PEX peers of a private torrent).
    #[must_use]
    pub fn ban_peers(mut self, addrs: impl IntoIterator<Item = SocketAddr>) -> Self {
        self.swarm_ban.extend(addrs);
        self
    }

    /// Current lifecycle state.
    #[must_use]
    pub const fn status(&self) -> TorrentStatus {
        self.status
    }

    /// Verified pieces.
    #[must_use]
    pub const fn have(&self) -> &Bitfield {
        self.picker.have()
    }

    /// The torrent configuration.
    #[must_use]
    pub const fn config(&self) -> &Config {
        &self.config
    }

    /// A progress snapshot.
    #[must_use]
    pub fn stats(&self) -> Stats {
        let layout = self.picker.layout();
        Stats {
            status: self.status,
            piece_count: layout.piece_count(),
            pieces_have: self.picker.have().count(),
            total_length: layout.total_length(),
            bytes_have: self.bytes_have,
            wanted_left: self.picker.wanted_left(),
            downloaded: self.downloaded,
            uploaded: self.uploaded,
            peers: self.peers.len(),
            connecting: self.pool.connecting(),
            known_peers: self.pool.known(),
            pending_writes: self.pending_writes,
            unchoked: self.peers.values().filter(|p| !p.am_choking).count(),
            interested: self.peers.values().filter(|p| p.peer_interested).count(),
            pending_reads: self.pending_reads,
            banned: self.bans.count(),
        }
    }

    fn left(&self) -> u64 {
        self.picker
            .layout()
            .total_length()
            .saturating_sub(self.bytes_have)
    }

    /// Processes one event and returns the effects the driver must perform,
    /// in order.
    pub fn update(&mut self, now: Instant, event: Event) -> Vec<Effect> {
        let mut out = Vec::new();
        if self.status == TorrentStatus::Stopped {
            return out;
        }
        if let Some(rate) = self.rate.as_mut() {
            rate.refill(now);
        }
        if let Some(rate) = self.upload_rate.as_mut() {
            rate.refill(now);
        }
        match event {
            Event::Start => self.on_start(now, &mut out),
            Event::Stop => self.stop(&mut out),
            Event::Tick => self.on_tick(now, &mut out),
            Event::PeerConnected {
                peer,
                addr,
                reserved,
            } => self.on_connected(now, peer, addr, reserved, false, &mut out),
            Event::PeerAccepted {
                peer,
                addr,
                reserved,
            } => self.on_connected(now, peer, addr, reserved, true, &mut out),
            Event::PeerConnectFailed { addr, retry } => {
                self.pool.dial_done(addr);
                if retry {
                    self.schedule_retry(now, addr);
                }
                self.fill_connections(&mut out);
            }
            Event::PeerDisconnected { peer } => {
                if let Some(addr) = self.remove_peer(peer) {
                    self.schedule_retry(now, addr);
                }
                self.fill_connections(&mut out);
                self.fill_unchoke_slots(now, &mut out);
                self.pump_uploads(&mut out);
            }
            Event::Message { peer, message } => self.on_message(now, peer, message, &mut out),
            Event::BlockWritten { block } => {
                self.pending_writes = self.pending_writes.saturating_sub(1);
                if let Some(piece) = self.picker.block_written(block) {
                    out.push(Effect::HashPiece { piece });
                }
                if self.pending_writes + 1 == self.config.max_pending_writes {
                    // Backpressure released: resume requesting.
                    self.fill_all_requests(now, &mut out);
                }
            }
            Event::WriteFailed { block, error } => {
                self.pending_writes = self.pending_writes.saturating_sub(1);
                self.picker.block_write_failed(block);
                self.fail(format!("storage write failed: {error}"), &mut out);
            }
            Event::PieceHashed { piece, ok } => self.on_hashed(now, piece, ok, &mut out),
            Event::BlockRead { peer, block, data } => {
                self.on_block_read(now, peer, block, data, &mut out);
            }
            Event::ReadFailed { peer, block, error } => {
                self.on_read_failed(now, peer, block, error, &mut out);
            }
            Event::StorageFailed { error } => self.fail(error, &mut out),
            Event::Announced { peers, interval } => {
                self.on_announced(now, peers, interval, &mut out);
            }
            Event::AddPeers { peers } => {
                self.add_to_pool(peers);
                self.fill_connections(&mut out);
            }
            Event::SetPriorities { priorities } => {
                if self.picker.set_priorities(&priorities) {
                    self.wanted_changed(now, &mut out);
                }
            }
            Event::SetUrgent { pieces } => {
                self.picker.set_urgent(pieces);
                self.wanted_changed(now, &mut out);
            }
            Event::AnnounceFailed { error } => {
                if self.pending_announce == Some(AnnounceEvent::Started) {
                    self.started_announced = false;
                }
                self.pending_announce = None;
                self.announce_in_flight = false;
                self.next_announce = Some(now + self.config.announce_retry);
                out.push(Effect::Alert(Alert::TrackerError { message: error }));
            }
            Event::DhtPeers { peers, failed } => {
                self.dht.done(now, &self.config, failed);
                if self.config.dht {
                    self.add_to_pool(peers);
                    self.fill_connections(&mut out);
                }
            }
        }
        out
    }

    fn set_status(&mut self, status: TorrentStatus, out: &mut Vec<Effect>) {
        if self.status != status {
            self.status = status;
            out.push(Effect::Alert(Alert::StateChanged(status)));
        }
    }

    fn on_start(&mut self, now: Instant, out: &mut Vec<Effect>) {
        if self.status != TorrentStatus::Idle {
            return;
        }
        self.rate = self
            .config
            .download_rate_limit
            .filter(|&r| r > 0)
            // One second of burst, and at least one block.
            .map(|r| TokenBucket::new(r, r.max(u64::from(BLOCK_SIZE)), now));
        self.upload_rate = self
            .config
            .upload_rate_limit
            .filter(|&r| r > 0)
            .map(|r| TokenBucket::new(r, r.max(u64::from(BLOCK_SIZE)), now));
        if self.config.check_existing && self.picker.layout().piece_count() > 0 {
            self.set_status(TorrentStatus::Checking, out);
            self.check = Some(Check {
                next: 0,
                outstanding: 0,
            });
            self.schedule_checks(now, out);
        } else {
            self.begin_download(now, out);
        }
        // Peer discovery runs during the hash check; addresses wait until
        // downloading starts. A torrent that is already complete does not look
        // anyone up.
        if matches!(
            self.status,
            TorrentStatus::Downloading | TorrentStatus::Checking
        ) {
            self.dht.poll(now, &self.config, out);
        }
    }

    fn schedule_checks(&mut self, now: Instant, out: &mut Vec<Effect>) {
        let count = self.picker.layout().piece_count();
        let Some(check) = self.check.as_mut() else {
            return;
        };
        while check.outstanding < self.config.check_concurrency.max(1) && check.next < count {
            if !self.picker.have().get(check.next) {
                out.push(Effect::HashPiece { piece: check.next });
                check.outstanding += 1;
            }
            check.next += 1;
        }
        if check.outstanding == 0 && check.next >= count {
            self.check = None;
            self.begin_download(now, out);
        }
    }

    fn begin_download(&mut self, now: Instant, out: &mut Vec<Effect>) {
        if self.picker.is_done() {
            self.finish(now, out);
            return;
        }
        self.set_status(TorrentStatus::Downloading, out);
        let event = if self.started_announced {
            AnnounceEvent::None
        } else {
            AnnounceEvent::Started
        };
        self.announce(now, event, out);
        self.dht.poll(now, &self.config, out);
        self.next_pex = Some(now + self.config.pex_interval);
        // Peers handed over from a metadata fetch connected while checking.
        for key in self.sorted_keys() {
            self.send_bitfield(now, key, out);
            self.update_interest(now, key, out);
        }
        self.fill_all_requests(now, out);
        self.fill_connections(out);
    }

    /// Every wanted piece is verified. A seeding torrent stays in the
    /// swarm: it announces (`completed` once every piece is in, or
    /// `started` if it never announced, as when it was complete on disk),
    /// drops peers that are seeds too and keeps uploading. Otherwise it
    /// leaves the swarm.
    fn finish(&mut self, now: Instant, out: &mut Vec<Effect>) {
        self.set_status(TorrentStatus::Finished, out);
        out.push(Effect::Alert(Alert::Finished));
        let complete = self.picker.is_complete();
        if !self.config.seed {
            self.pool.clear_known();
            if self.started_announced {
                if complete && !self.completed_announced {
                    self.completed_announced = true;
                    self.push_announce(AnnounceEvent::Completed, out);
                } else if !complete {
                    // Only part of the torrent is wanted: leave the swarm.
                    self.push_announce(AnnounceEvent::Stopped, out);
                    self.started_announced = false;
                }
            }
            self.next_announce = None;
            for key in self.sorted_keys() {
                self.remove_peer(key);
                out.push(Effect::Disconnect { peer: key });
            }
            return;
        }
        if !self.started_announced {
            self.announce(now, AnnounceEvent::Started, out);
        } else if complete && !self.completed_announced {
            self.completed_announced = true;
            self.push_announce(AnnounceEvent::Completed, out);
        }
        if self.next_pex.is_none() {
            self.next_pex = Some(now + self.config.pex_interval);
        }
        self.dht.poll(now, &self.config, out);
        let seeds: Vec<PeerKey> = self
            .sorted_keys()
            .into_iter()
            .filter(|k| self.peers.get(k).is_some_and(|p| p.has.is_full()) && complete)
            .collect();
        for key in seeds {
            self.remove_peer(key);
            out.push(Effect::Disconnect { peer: key });
        }
        for key in self.sorted_keys() {
            self.update_interest(now, key, out);
        }
        // Rankings switch from download to upload rates.
        self.run_choker(now, out);
        self.fill_connections(out);
    }

    /// Priorities or urgent pieces changed: finish when nothing wanted is
    /// missing any more, go back to downloading when something is, and
    /// re-evaluate interest and requests.
    fn wanted_changed(&mut self, now: Instant, out: &mut Vec<Effect>) {
        match self.status {
            TorrentStatus::Downloading if self.picker.is_done() => self.finish(now, out),
            TorrentStatus::Finished if !self.picker.is_done() => self.resume_download(now, out),
            TorrentStatus::Downloading => {
                for key in self.sorted_keys() {
                    self.update_interest(now, key, out);
                }
                self.fill_all_requests(now, out);
            }
            _ => {}
        }
    }

    /// A finished torrent wants more pieces: download again. Peers keep
    /// their connections (when seeding); a torrent that left the swarm
    /// announces `started` again.
    fn resume_download(&mut self, now: Instant, out: &mut Vec<Effect>) {
        self.set_status(TorrentStatus::Downloading, out);
        if !self.started_announced {
            self.announce(now, AnnounceEvent::Started, out);
        } else if self.next_announce.is_none() {
            self.next_announce = Some(now + self.config.default_announce_interval);
        }
        if self.next_pex.is_none() {
            self.next_pex = Some(now + self.config.pex_interval);
        }
        self.dht.poll(now, &self.config, out);
        for key in self.sorted_keys() {
            self.update_interest(now, key, out);
        }
        self.run_choker(now, out);
        self.fill_all_requests(now, out);
        self.fill_connections(out);
    }

    /// Whether the torrent is in the swarm: downloading, or seeding.
    fn is_active(&self) -> bool {
        self.status == TorrentStatus::Downloading
            || (self.status == TorrentStatus::Finished && self.config.seed)
    }

    fn stop(&mut self, out: &mut Vec<Effect>) {
        for peer in self.sorted_keys() {
            self.remove_peer(peer);
            out.push(Effect::Disconnect { peer });
        }
        if self.started_announced {
            self.push_announce(AnnounceEvent::Stopped, out);
            self.started_announced = false;
        }
        self.pool.clear();
        self.set_status(TorrentStatus::Stopped, out);
    }

    fn fail(&mut self, message: String, out: &mut Vec<Effect>) {
        out.push(Effect::Alert(Alert::Error { message }));
        self.stop(out);
    }

    fn announce(&mut self, now: Instant, event: AnnounceEvent, out: &mut Vec<Effect>) {
        if self.announce_in_flight {
            return;
        }
        self.announce_in_flight = true;
        self.pending_announce = Some(event);
        self.next_announce = Some(now + self.config.default_announce_interval);
        self.push_announce(event, out);
        if event == AnnounceEvent::Started {
            self.started_announced = true;
        }
    }

    fn push_announce(&self, event: AnnounceEvent, out: &mut Vec<Effect>) {
        out.push(Effect::Announce {
            event,
            uploaded: self.uploaded,
            downloaded: self.downloaded,
            left: self.left(),
        });
    }

    fn on_announced(
        &mut self,
        now: Instant,
        peers: Vec<SocketAddr>,
        interval: Option<Duration>,
        out: &mut Vec<Effect>,
    ) {
        self.pending_announce = None;
        self.announce_in_flight = false;
        let interval = interval
            .unwrap_or(self.config.default_announce_interval)
            .max(self.config.min_announce_interval);
        self.next_announce = Some(now + interval);
        self.add_to_pool(peers);
        self.fill_connections(out);
    }

    fn is_connected(peers: &HashMap<PeerKey, Peer>, addr: SocketAddr) -> bool {
        peers.values().any(|p| p.addr == addr)
    }

    fn add_to_pool(&mut self, peers: Vec<SocketAddr>) {
        // Peers given while checking are kept for when downloading starts.
        if !(self.is_active() || self.status == TorrentStatus::Checking) {
            return;
        }
        let connected: HashSet<SocketAddr> = self.peers.values().map(|p| p.addr).collect();
        let banned = &self.swarm_ban;
        let bans = &self.bans;
        self.pool.add(
            peers
                .into_iter()
                .filter(|addr| !banned.contains(addr) && !bans.is_banned(addr.ip())),
            |a| connected.contains(&a),
        );
    }

    /// Whether failed and dropped peers are dialed again: while downloading,
    /// checking, or seeding. A finished torrent that is not seeding has left
    /// the swarm.
    fn retries_peers(&self) -> bool {
        self.is_active() || self.status == TorrentStatus::Checking
    }

    /// Remembers `addr` so a later tick can dial it again.
    fn schedule_retry(&mut self, now: Instant, addr: SocketAddr) {
        if self.swarm_ban.contains(&addr) || self.bans.is_banned(addr.ip()) || !self.retries_peers()
        {
            return;
        }
        let peers = &self.peers;
        self.pool
            .schedule_retry(now + self.config.peer_retry, addr, |a| {
                Self::is_connected(peers, a)
            });
    }

    fn fill_connections(&mut self, out: &mut Vec<Effect>) {
        if !self.is_active() {
            return;
        }
        while self.peers.len() + self.pool.connecting() < self.config.max_peers {
            let peers = &self.peers;
            let Some(addr) = self.pool.next_dial(|a| Self::is_connected(peers, a)) else {
                break;
            };
            out.push(Effect::Connect { addr });
        }
    }

    fn on_connected(
        &mut self,
        now: Instant,
        key: PeerKey,
        addr: SocketAddr,
        reserved: Reserved,
        incoming: bool,
        out: &mut Vec<Effect>,
    ) {
        if !incoming {
            self.pool.dial_done(addr);
        }
        // Peers may connect while checking: a magnet's metadata fetch hands
        // its connections over before the data check ends.
        if self.swarm_ban.contains(&addr)
            || self.bans.is_banned(addr.ip())
            || !(self.is_active() || self.status == TorrentStatus::Checking)
            || self.peers.len() >= self.config.max_peers
            || self.peers.contains_key(&key)
            || Self::is_connected(&self.peers, addr)
        {
            out.push(Effect::Disconnect { peer: key });
            return;
        }
        let piece_count = self.picker.layout().piece_count();
        let mut peer = Peer::new(addr, incoming, reserved, piece_count, &self.config, now);
        if peer.extended {
            let size = self
                .info_bytes
                .as_ref()
                .and_then(|b| u64::try_from(b.len()).ok());
            let hs = ext::our_handshake(&self.config, size, addr);
            Self::send(&mut peer, key, hs, now, out);
        }
        self.peers.insert(key, peer);
        if self.status != TorrentStatus::Checking {
            self.send_bitfield(now, key, out);
        }
    }

    /// Cancels requests that no longer fit the window (after `reqq` shrinks
    /// it, or a timeout halves it).
    fn trim_inflight(&mut self, now: Instant, key: PeerKey, out: &mut Vec<Effect>) {
        let Some(peer) = self.peers.get_mut(&key) else {
            return;
        };
        let limit = peer.request_limit();
        if peer.inflight.len() <= limit {
            return;
        }
        let extra: Vec<BlockRef> = peer
            .inflight
            .split_off(limit)
            .into_iter()
            .map(|(block, _)| block)
            .collect();
        for block in extra {
            if let Some(peer) = self.peers.get_mut(&key) {
                Self::send(peer, key, Message::Cancel(block.into()), now, out);
            }
            self.release_owner(key, block);
        }
    }

    /// Our availability: `have all`/`have none` to fast-extension peers
    /// when that says it all, else a bitfield (none at all for a peer
    /// without the fast extension when we have nothing). Then the allowed
    /// fast set.
    fn send_bitfield(&mut self, now: Instant, key: PeerKey, out: &mut Vec<Effect>) {
        let have = self.picker.have();
        let Some(peer) = self.peers.get_mut(&key) else {
            return;
        };
        let message = if peer.fast && have.is_full() {
            Some(Message::HaveAll)
        } else if have.count() == 0 {
            peer.fast.then_some(Message::HaveNone)
        } else {
            Some(Message::Bitfield(have.to_bytes()))
        };
        if let Some(message) = message {
            Self::send(peer, key, message, now, out);
        }
        self.send_allowed_fast(now, key, out);
    }

    /// The peer's BEP 6 allowed fast set. Empty unless both sides speak the
    /// fast extension, an info-hash is known, and the peer has an IPv4
    /// address (BEP 6 defines the set for IPv4 only; a mapped IPv6 address
    /// uses the IPv4 form).
    fn allowed_fast_pieces(&self, key: PeerKey) -> Vec<u32> {
        let Some(info_hash) = self.info_hash else {
            return Vec::new();
        };
        let k = self.config.allowed_fast;
        if k == 0 {
            return Vec::new();
        }
        let Some(peer) = self.peers.get(&key) else {
            return Vec::new();
        };
        if !peer.fast {
            return Vec::new();
        }
        let Some(ip) = ipv4_for_allowed_fast(peer.addr.ip()) else {
            return Vec::new();
        };
        dravity_wire::allowed_fast_set(&info_hash, ip, self.picker.have().len(), k)
    }

    /// Offers the pieces of the allowed fast set we already have.
    fn send_allowed_fast(&mut self, now: Instant, key: PeerKey, out: &mut Vec<Effect>) {
        let pieces = self.allowed_fast_pieces(key);
        if pieces.is_empty() {
            return;
        }
        let have = self.picker.have();
        let Some(peer) = self.peers.get_mut(&key) else {
            return;
        };
        for piece in pieces {
            if have.get(piece) && peer.allowed_out.insert(piece) {
                Self::send(peer, key, Message::AllowedFast { piece }, now, out);
            }
        }
    }

    /// Offers `piece` if it belongs in this peer's allowed fast set and was
    /// not offered at the handshake. A piece completed later is otherwise
    /// invisible to a choked peer.
    fn offer_allowed_fast_piece(
        &mut self,
        now: Instant,
        key: PeerKey,
        piece: u32,
        out: &mut Vec<Effect>,
    ) {
        if !self.picker.have().get(piece) {
            return;
        }
        if !self.allowed_fast_pieces(key).contains(&piece) {
            return;
        }
        let Some(peer) = self.peers.get_mut(&key) else {
            return;
        };
        if peer.allowed_out.insert(piece) {
            Self::send(peer, key, Message::AllowedFast { piece }, now, out);
        }
    }

    /// Forgets a connected peer and returns its address. Upload tokens for
    /// reads that will not be sent are returned.
    fn remove_peer(&mut self, key: PeerKey) -> Option<SocketAddr> {
        let peer = self.peers.remove(&key)?;
        let refund: u64 = peer.reading.iter().map(|b| u64::from(b.length)).sum();
        self.choker.remove(key);
        for (block, _) in &peer.inflight {
            self.release_owner(key, *block);
        }
        self.picker.remove_peer_pieces(&peer.has);
        self.refund_upload(refund);
        Some(peer.addr)
    }

    /// `key` no longer holds a request for `block`; the block goes back to
    /// the picker when no other peer holds one either.
    fn release_owner(&mut self, key: PeerKey, block: BlockRef) {
        let now_free = match self.owners.get_mut(&block) {
            Some(owners) => {
                owners.retain(|(k, _)| *k != key);
                owners.is_empty()
            }
            None => true,
        };
        if now_free {
            self.owners.remove(&block);
            self.picker.release(block);
        }
    }

    fn disconnect(&mut self, key: PeerKey, out: &mut Vec<Effect>) {
        self.remove_peer(key);
        out.push(Effect::Disconnect { peer: key });
        self.fill_connections(out);
        self.pump_uploads(out);
    }

    /// Two seeds have nothing to exchange: drop a peer that has every piece
    /// once we have them too. Returns whether it was dropped.
    fn drop_if_both_seeds(&mut self, key: PeerKey, out: &mut Vec<Effect>) -> bool {
        let both =
            self.picker.is_complete() && self.peers.get(&key).is_some_and(|p| p.has.is_full());
        if both {
            self.disconnect(key, out);
        }
        both
    }

    fn on_message(&mut self, now: Instant, key: PeerKey, message: Message, out: &mut Vec<Effect>) {
        let piece_count = self.picker.layout().piece_count();
        let Some(peer) = self.peers.get_mut(&key) else {
            return;
        };
        peer.last_recv = now;
        // Captured before this message consumes the window.
        let bitfield_ok = peer.bitfield_ok;
        if !message_defers_bitfield(&message) {
            peer.bitfield_ok = false;
        }
        match message {
            Message::Bitfield(bits) => {
                if !bitfield_ok {
                    return self.disconnect(key, out);
                }
                match Bitfield::from_wire(&bits, piece_count) {
                    Ok(has) => self.set_peer_pieces(key, has),
                    Err(_) => return self.disconnect(key, out),
                }
                if self.drop_if_both_seeds(key, out) {
                    return;
                }
            }
            // Accepted without the fast extension too: harmless, and some
            // clients send it anyway.
            Message::HaveAll => {
                if !bitfield_ok {
                    return self.disconnect(key, out);
                }
                self.set_peer_pieces(key, Bitfield::full(piece_count));
                if self.drop_if_both_seeds(key, out) {
                    return;
                }
            }
            Message::HaveNone => {
                if !bitfield_ok {
                    return self.disconnect(key, out);
                }
            }
            Message::Have { piece } => {
                if piece >= piece_count {
                    return self.disconnect(key, out);
                }
                if peer.has.set(piece) {
                    self.picker.add_peer_piece(piece);
                }
                if self.drop_if_both_seeds(key, out) {
                    return;
                }
            }
            Message::Choke => {
                peer.peer_choking = true;
                peer.snub_since = None;
                // With the fast extension a choke keeps our requests: the
                // peer rejects the ones it will not serve (BEP 6).
                if !peer.fast {
                    let blocks: Vec<BlockRef> = peer.inflight.drain(..).map(|(b, _)| b).collect();
                    for block in blocks {
                        self.release_owner(key, block);
                    }
                }
            }
            Message::Unchoke => peer.peer_choking = false,
            Message::Interested => {
                peer.peer_interested = true;
                self.fill_unchoke_slots(now, out);
            }
            Message::NotInterested => {
                peer.peer_interested = false;
                if !peer.am_choking {
                    // Give the slot to someone who wants it.
                    self.choke_peer(now, key, out);
                    self.fill_unchoke_slots(now, out);
                    self.pump_uploads(out);
                }
            }
            Message::Request(request) => self.on_request(now, key, request, out),
            Message::Cancel(request) => self.on_cancel(now, key, request, out),
            Message::Reject(request) if peer.fast => self.on_reject(key, request),
            Message::AllowedFast { piece } if peer.fast => {
                if piece < piece_count {
                    peer.allowed_in.insert(piece);
                }
            }
            Message::Suggest { piece } if peer.fast => {
                if piece < piece_count && !peer.suggested.contains(&piece) {
                    if peer.suggested.len() >= MAX_SUGGESTED {
                        peer.suggested.pop_front();
                    }
                    peer.suggested.push_back(piece);
                }
            }
            Message::Piece {
                piece,
                offset,
                data,
            } => self.on_block(now, key, piece, offset, data, out),
            Message::Extended { id, payload } => {
                let found_peers = self.on_extended(now, key, id, &payload, out);
                if found_peers {
                    self.fill_connections(out);
                }
            }
            // Fast-extension messages from a peer without the extension and
            // DHT `port` are ignored.
            _ => {}
        }
        self.update_interest(now, key, out);
        self.trim_inflight(now, key, out);
        self.fill_requests(now, key, out);
    }

    /// Handles a BEP 10 message. Returns `true` if new peers were queued.
    fn on_extended(
        &mut self,
        now: Instant,
        key: PeerKey,
        id: u8,
        payload: &Bytes,
        out: &mut Vec<Effect>,
    ) -> bool {
        let Some(peer) = self.peers.get_mut(&key) else {
            return false;
        };
        if !peer.extended {
            return false;
        }
        match id {
            ext::HANDSHAKE_ID => {
                if let Ok(hs) = ExtendedHandshake::decode(payload) {
                    peer.ext.update(&hs);
                    if peer.incoming {
                        if let Some(port) = hs.listen_port.filter(|&p| p != 0) {
                            peer.reachable = Some(SocketAddr::new(peer.addr.ip(), port));
                        }
                    }
                }
                false
            }
            ext::OUR_UT_METADATA => {
                let Some(their_id) = peer.ext.ut_metadata else {
                    return false;
                };
                if let Ok(MetadataMessage::Request { piece }) = MetadataMessage::decode(payload) {
                    let reply = metadata_reply(self.info_bytes.as_ref(), piece);
                    if let Ok(payload) = reply.encode() {
                        let message = Message::Extended {
                            id: their_id,
                            payload,
                        };
                        Self::send(peer, key, message, now, out);
                    }
                }
                false
            }
            ext::OUR_UT_PEX if self.config.pex => match ext::pex_added(payload) {
                Some(added) if !added.is_empty() => {
                    self.add_to_pool(added);
                    true
                }
                _ => false,
            },
            _ => false,
        }
    }

    fn set_peer_pieces(&mut self, key: PeerKey, has: Bitfield) {
        if let Some(peer) = self.peers.get_mut(&key) {
            self.picker.remove_peer_pieces(&peer.has);
            self.picker.add_peer_pieces(&has);
            peer.has = has;
        }
    }

    fn on_block(
        &mut self,
        now: Instant,
        key: PeerKey,
        piece: u32,
        offset: u32,
        data: Bytes,
        out: &mut Vec<Effect>,
    ) {
        let Ok(length) = u32::try_from(data.len()) else {
            return;
        };
        let block = BlockRef {
            piece,
            offset,
            length,
        };
        // Only a peer we asked may deliver the block. Anyone else would
        // cancel the honest download and get their bytes hashed.
        let asked = self
            .peers
            .get(&key)
            .is_some_and(|peer| peer.asked.contains(&(piece, offset)));
        if !asked {
            return;
        }
        let mut from_ip = None;
        if let Some(peer) = self.peers.get_mut(&key) {
            let before = peer.inflight.len();
            peer.inflight.retain(|(b, _)| *b != block);
            if peer.inflight.len() < before {
                peer.window.on_block();
            }
            peer.down_round = peer.down_round.saturating_add(u64::from(length));
            peer.snub_since = (!peer.inflight.is_empty()).then_some(now);
            from_ip = Some(peer.addr.ip());
        }
        // Cancel copies of this block requested from other peers.
        for (other, _) in self.owners.remove(&block).unwrap_or_default() {
            if other == key {
                continue;
            }
            if let Some(peer) = self.peers.get_mut(&other) {
                peer.inflight.retain(|(b, _)| *b != block);
                Self::send(peer, other, Message::Cancel(block.into()), now, out);
            }
        }
        self.downloaded = self.downloaded.saturating_add(u64::from(length));
        if self.picker.block_received(block) == Received::Accepted {
            if let Some(ip) = from_ip {
                self.bans.record(piece, ip);
            }
            self.pending_writes += 1;
            out.push(Effect::WriteBlock { block, data });
        }
    }

    fn send(peer: &mut Peer, key: PeerKey, message: Message, now: Instant, out: &mut Vec<Effect>) {
        peer.last_sent = now;
        out.push(Effect::Send { peer: key, message });
    }

    fn update_interest(&mut self, now: Instant, key: PeerKey, out: &mut Vec<Effect>) {
        // While checking, our bitfield is not final; interest (and the
        // bitfield, which must come first) wait for the download to begin.
        if self.status == TorrentStatus::Checking {
            return;
        }
        let Some(peer) = self.peers.get_mut(&key) else {
            return;
        };
        let interesting = self.picker.is_interesting(&peer.has);
        if interesting != peer.am_interested {
            peer.am_interested = interesting;
            let message = if interesting {
                Message::Interested
            } else {
                Message::NotInterested
            };
            Self::send(peer, key, message, now, out);
        }
    }

    fn fill_all_requests(&mut self, now: Instant, out: &mut Vec<Effect>) {
        for key in self.sorted_keys() {
            self.fill_requests(now, key, out);
        }
    }

    fn fill_requests(&mut self, now: Instant, key: PeerKey, out: &mut Vec<Effect>) {
        if self.status != TorrentStatus::Downloading {
            return;
        }
        let piece_count = self.picker.layout().piece_count();
        let Some(peer) = self.peers.get_mut(&key) else {
            return;
        };
        if !peer.am_interested {
            return;
        }
        // A choking fast-extension peer still serves its allowed fast set.
        let choked_mask = if peer.peer_choking {
            if !peer.fast || peer.allowed_in.is_empty() {
                return;
            }
            let mut mask = Bitfield::new(piece_count);
            for &piece in &peer.allowed_in {
                if peer.has.get(piece) {
                    mask.set(piece);
                }
            }
            Some(mask)
        } else {
            None
        };
        let has = choked_mask.as_ref().unwrap_or(&peer.has);
        let limit = peer.request_limit();
        while peer.inflight.len() < limit && self.pending_writes < self.config.max_pending_writes {
            if self
                .rate
                .as_ref()
                .is_some_and(|r| r.available() < u64::from(BLOCK_SIZE))
            {
                break;
            }
            let block = match self
                .picker
                .pick_with_hint(has, peer.suggested.make_contiguous())
            {
                Some(block) => block,
                None => match duplicate_candidate(
                    &self.picker,
                    &self.owners,
                    &self.config,
                    key,
                    has,
                    now,
                ) {
                    Some(block) => block,
                    None => break,
                },
            };
            if let Some(rate) = self.rate.as_mut() {
                rate.try_take(u64::from(block.length));
            }
            self.owners.entry(block).or_default().push((key, now));
            if peer.snub_since.is_none() {
                // The snub timer runs from the first outstanding request.
                peer.snub_since = Some(now);
            }
            peer.inflight.push((block, now));
            peer.asked.insert((block.piece, block.offset));
            // Not `Self::send`: `has` may borrow `peer.has`.
            peer.last_sent = now;
            out.push(Effect::Send {
                peer: key,
                message: Message::Request(block.into()),
            });
        }
    }

    fn sorted_keys(&self) -> Vec<PeerKey> {
        let mut keys: Vec<PeerKey> = self.peers.keys().copied().collect();
        keys.sort_unstable();
        keys
    }

    fn on_hashed(&mut self, now: Instant, piece: u32, ok: bool, out: &mut Vec<Effect>) {
        if self.status == TorrentStatus::Checking {
            if let Some(check) = self.check.as_mut() {
                check.outstanding = check.outstanding.saturating_sub(1);
            }
            if ok && self.picker.mark_have(piece) {
                self.add_have_bytes(piece);
            }
            self.schedule_checks(now, out);
            return;
        }
        if !ok {
            self.picker.piece_hashed(piece, false);
            out.push(Effect::Alert(Alert::HashFailed { piece }));
            for ip in self.bans.failed(
                piece,
                self.config.hash_fail_ban,
                now,
                self.config.hash_fail_quiet,
            ) {
                self.ban(ip, out);
            }
        } else if self.picker.piece_hashed(piece, true) {
            self.bans.passed(piece);
            self.add_have_bytes(piece);
            out.push(Effect::Alert(Alert::PieceCompleted { piece }));
            for key in self.sorted_keys() {
                if let Some(peer) = self.peers.get_mut(&key) {
                    Self::send(peer, key, Message::Have { piece }, now, out);
                }
                self.offer_allowed_fast_piece(now, key, piece, out);
                self.update_interest(now, key, out);
            }
            if self.status == TorrentStatus::Downloading && self.picker.is_done() {
                self.finish(now, out);
                return;
            }
        }
        self.fill_all_requests(now, out);
    }

    fn add_have_bytes(&mut self, piece: u32) {
        let size = self.picker.layout().piece_size(piece).unwrap_or(0);
        self.bytes_have = self.bytes_have.saturating_add(u64::from(size));
    }

    fn on_tick(&mut self, now: Instant, out: &mut Vec<Effect>) {
        self.bans.expire(now, self.config.hash_fail_quiet);
        if self.retries_peers() {
            let peers = &self.peers;
            self.pool.promote(now, |a| Self::is_connected(peers, a));
        }
        let mut dead = Vec::new();
        for key in self.sorted_keys() {
            let expired = {
                let Some(peer) = self.peers.get_mut(&key) else {
                    continue;
                };
                if now.saturating_duration_since(peer.last_recv) >= self.config.peer_timeout {
                    dead.push(key);
                    continue;
                }
                let timeout = self.config.request_timeout;
                let mut expired = Vec::new();
                peer.inflight.retain(|(block, since)| {
                    let late = now.saturating_duration_since(*since) >= timeout;
                    if late {
                        expired.push(*block);
                    }
                    !late
                });
                if !expired.is_empty() {
                    // Live limit is min(window, reqq). Halve that, so a window
                    // that has grown to 128 with reqq 4 becomes 2.
                    let reqq = peer.ext.reqq.unwrap_or(u32::MAX);
                    peer.window.on_timeout(reqq);
                }
                if now.saturating_duration_since(peer.last_sent) >= self.config.keepalive_interval {
                    Self::send(peer, key, Message::KeepAlive, now, out);
                }
                expired
            };
            for block in expired {
                self.release_owner(key, block);
                // The timed-out request is outside the shrunk pipeline. Cancel
                // it so the peer frees the queue slot; a refill may ask again.
                if let Some(peer) = self.peers.get_mut(&key) {
                    Self::send(peer, key, Message::Cancel(block.into()), now, out);
                }
            }
            // Still-in-flight requests past the new limit are cancelled here.
            self.trim_inflight(now, key, out);
        }
        for key in dead {
            self.remove_peer(key);
            out.push(Effect::Disconnect { peer: key });
        }
        if self.is_active() || self.status == TorrentStatus::Checking {
            self.dht.poll(now, &self.config, out);
        }
        if self.status == TorrentStatus::Downloading {
            self.fill_all_requests(now, out);
        }
        if self.is_active() {
            if self.next_pex.is_some_and(|t| now >= t) {
                self.next_pex = Some(now + self.config.pex_interval);
                self.send_pex(now, out);
            }
            if self.choker.due(now) {
                self.run_choker(now, out);
            } else {
                self.fill_unchoke_slots(now, out);
            }
            self.pump_uploads(out);
        }
        if matches!(
            self.status,
            TorrentStatus::Downloading | TorrentStatus::Finished
        ) && self.next_announce.is_some_and(|t| now >= t)
        {
            let event = if self.started_announced {
                AnnounceEvent::None
            } else {
                AnnounceEvent::Started
            };
            self.announce(now, event, out);
        }
        self.fill_connections(out);
        out.push(Effect::Alert(Alert::Stats(self.stats())));
    }

    /// A peer asked us for a block. Queued if we have the piece, the peer
    /// may download it (unchoked, or in its allowed fast set) and its queue
    /// has room; otherwise refused (`reject` with the fast extension,
    /// silently without).
    fn on_request(
        &mut self,
        now: Instant,
        key: PeerKey,
        request: BlockRequest,
        out: &mut Vec<Effect>,
    ) {
        let block = BlockRef {
            piece: request.piece,
            offset: request.offset,
            length: request.length,
        };
        let serving = self.is_active();
        let valid =
            self.picker.layout().is_valid_request(block) && self.picker.have().get(block.piece);
        let max_queue = self.config.max_upload_queue;
        let Some(peer) = self.peers.get_mut(&key) else {
            return;
        };
        if peer.upload_queue.contains(&block) || peer.reading.contains(&block) {
            return;
        }
        let room = peer.upload_queue.len() + peer.reading.len() < max_queue;
        if !(serving && valid && room && peer.may_download(block.piece)) {
            if peer.fast {
                Self::send(peer, key, Message::Reject(request), now, out);
            }
            return;
        }
        peer.upload_queue.push_back(block);
        self.pump_uploads(out);
    }

    /// A peer no longer wants a block. With the fast extension a cancelled
    /// request that was still queued is answered with `reject` (BEP 6).
    fn on_cancel(
        &mut self,
        now: Instant,
        key: PeerKey,
        request: BlockRequest,
        out: &mut Vec<Effect>,
    ) {
        let block = BlockRef {
            piece: request.piece,
            offset: request.offset,
            length: request.length,
        };
        let reading = {
            let Some(peer) = self.peers.get_mut(&key) else {
                return;
            };
            let before = peer.upload_queue.len();
            peer.upload_queue.retain(|b| *b != block);
            let queued = peer.upload_queue.len() < before;
            let reading = peer.reading.remove(&block);
            let found = queued || reading;
            if found && peer.fast {
                Self::send(peer, key, Message::Reject(request), now, out);
            }
            reading
        };
        // Queued requests were not charged. An in-flight read was.
        if reading {
            self.refund_upload(u64::from(block.length));
            self.pump_uploads(out);
        }
    }

    /// A fast-extension peer will not serve a request: hand the block back.
    /// A rejected allowed fast piece is not asked for again while choked.
    fn on_reject(&mut self, key: PeerKey, request: BlockRequest) {
        let block = BlockRef {
            piece: request.piece,
            offset: request.offset,
            length: request.length,
        };
        let Some(peer) = self.peers.get_mut(&key) else {
            return;
        };
        let before = peer.inflight.len();
        peer.inflight.retain(|(b, _)| *b != block);
        if peer.inflight.len() == before {
            return;
        }
        if peer.peer_choking {
            peer.allowed_in.remove(&block.piece);
        }
        self.release_owner(key, block);
    }

    /// Issues block reads for queued requests, round-robin over peers, while
    /// disk reads and the upload rate limit allow.
    fn pump_uploads(&mut self, out: &mut Vec<Effect>) {
        let keys: Vec<PeerKey> = self
            .sorted_keys()
            .into_iter()
            .filter(|k| {
                self.peers
                    .get(k)
                    .is_some_and(|p| !p.upload_queue.is_empty())
            })
            .collect();
        if keys.is_empty() {
            return;
        }
        // Start after the peer served last.
        let start = self
            .upload_cursor
            .and_then(|c| keys.iter().position(|k| *k > c))
            .unwrap_or(0);
        let mut order: VecDeque<PeerKey> = keys
            .iter()
            .cycle()
            .skip(start)
            .take(keys.len())
            .copied()
            .collect();
        while self.pending_reads < self.config.max_pending_reads.max(1) {
            let Some(key) = order.pop_front() else {
                break;
            };
            let Some(peer) = self.peers.get_mut(&key) else {
                continue;
            };
            let Some(&block) = peer.upload_queue.front() else {
                continue;
            };
            if let Some(rate) = self.upload_rate.as_mut() {
                if !rate.try_take(u64::from(block.length)) {
                    // Tokens refill on the next tick. The charge is refunded
                    // if this block is never sent.
                    break;
                }
            }
            peer.upload_queue.pop_front();
            peer.reading.insert(block);
            self.pending_reads += 1;
            self.upload_cursor = Some(key);
            out.push(Effect::ReadBlock { peer: key, block });
            if !peer.upload_queue.is_empty() {
                order.push_back(key);
            }
        }
    }

    fn on_block_read(
        &mut self,
        now: Instant,
        key: PeerKey,
        block: BlockRef,
        data: Bytes,
        out: &mut Vec<Effect>,
    ) {
        self.pending_reads = self.pending_reads.saturating_sub(1);
        let mut refund = false;
        if let Some(peer) = self.peers.get_mut(&key) {
            // Gone from `reading`: cancelled, choked, or the peer was
            // dropped (those paths already refunded).
            if peer.reading.remove(&block) {
                if peer.may_download(block.piece) {
                    let len = u64::try_from(data.len()).unwrap_or(u64::MAX);
                    peer.up_round = peer.up_round.saturating_add(len);
                    self.uploaded = self.uploaded.saturating_add(len);
                    let message = Message::Piece {
                        piece: block.piece,
                        offset: block.offset,
                        data,
                    };
                    Self::send(peer, key, message, now, out);
                } else {
                    refund = true;
                }
            }
        }
        if refund {
            self.refund_upload(u64::from(block.length));
        }
        self.pump_uploads(out);
    }

    /// A block read for an upload failed. Drop the request (reject it when
    /// the peer speaks the fast extension) and keep the torrent running.
    /// The block is no longer in `reading`, so the peer may ask again.
    fn on_read_failed(
        &mut self,
        now: Instant,
        key: PeerKey,
        block: BlockRef,
        error: String,
        out: &mut Vec<Effect>,
    ) {
        self.pending_reads = self.pending_reads.saturating_sub(1);
        out.push(Effect::Alert(Alert::UploadFailed {
            piece: block.piece,
            message: error,
        }));
        let refund = if let Some(peer) = self.peers.get_mut(&key) {
            // Already removed: a cancel or a choke rejected it and refunded.
            let still = peer.reading.remove(&block);
            if still && peer.fast {
                Self::send(peer, key, Message::Reject(block.into()), now, out);
            }
            still
        } else {
            false
        };
        if refund {
            self.refund_upload(u64::from(block.length));
        }
        self.pump_uploads(out);
    }

    fn unchoke_peer(&mut self, now: Instant, key: PeerKey, out: &mut Vec<Effect>) {
        if let Some(peer) = self.peers.get_mut(&key) {
            if peer.am_choking {
                peer.am_choking = false;
                Self::send(peer, key, Message::Unchoke, now, out);
            }
        }
    }

    /// Chokes a peer. Its queued requests are dropped, except allowed fast
    /// pieces; with the fast extension each dropped one is rejected, as BEP
    /// 6 requires (a choke no longer cancels requests implicitly).
    fn choke_peer(&mut self, now: Instant, key: PeerKey, out: &mut Vec<Effect>) {
        let refund = {
            let Some(peer) = self.peers.get_mut(&key) else {
                return;
            };
            if peer.am_choking {
                return;
            }
            peer.am_choking = true;
            Self::send(peer, key, Message::Choke, now, out);
            let allowed = peer.allowed_out.clone();
            let mut dropped: Vec<BlockRef> = Vec::new();
            peer.upload_queue.retain(|b| {
                let keep = allowed.contains(&b.piece);
                if !keep {
                    dropped.push(*b);
                }
                keep
            });
            let mut reading: Vec<BlockRef> = peer
                .reading
                .iter()
                .copied()
                .filter(|b| !allowed.contains(&b.piece))
                .collect();
            reading.sort_unstable_by_key(|b| (b.piece, b.offset));
            let refund: u64 = reading.iter().map(|b| u64::from(b.length)).sum();
            for block in &reading {
                peer.reading.remove(block);
            }
            dropped.extend(reading);
            if peer.fast {
                for block in dropped {
                    Self::send(peer, key, Message::Reject(block.into()), now, out);
                }
            }
            refund
        };
        // Queued requests were never charged. In-flight reads were.
        self.refund_upload(refund);
    }

    /// Chooses who we upload to for the next round (see
    /// [`choker`](crate::choker)) and resets the per-round counters.
    fn run_choker(&mut self, now: Instant, out: &mut Vec<Effect>) {
        if !self.is_active() {
            return;
        }
        let seeding = self.status == TorrentStatus::Finished;
        let snub = self.config.snub_timeout;
        let candidates: Vec<Candidate> = self
            .sorted_keys()
            .into_iter()
            .filter_map(|key| {
                let p = self.peers.get(&key)?;
                Some(Candidate {
                    key,
                    interested: p.peer_interested,
                    rate: if seeding { p.up_round } else { p.down_round },
                    snubbed: !seeding && p.is_snubbing(now, snub),
                    unchoked: !p.am_choking,
                    waiting_since: p.last_optimistic,
                })
            })
            .collect();
        let before = self.choker.optimistic();
        let selected = self.choker.round(now, &self.config, &candidates);
        if let Some(key) = self.choker.optimistic().filter(|k| Some(*k) != before) {
            if let Some(peer) = self.peers.get_mut(&key) {
                peer.last_optimistic = now;
            }
        }
        for candidate in &candidates {
            if selected.contains(&candidate.key) {
                self.unchoke_peer(now, candidate.key, out);
            } else {
                self.choke_peer(now, candidate.key, out);
            }
        }
        for peer in self.peers.values_mut() {
            peer.down_round = 0;
            peer.up_round = 0;
        }
        self.pump_uploads(out);
    }

    /// Between rounds, unchokes interested peers while slots are free, so a
    /// newcomer does not wait for the next round. Never chokes anyone.
    /// Snubbed peers are skipped: a free slot here is a regular slot, and
    /// the optimistic one is chosen by the choker round.
    fn fill_unchoke_slots(&mut self, now: Instant, out: &mut Vec<Effect>) {
        if !self.is_active() {
            return;
        }
        let limit = self.config.upload_slots + usize::from(self.config.optimistic_unchoke);
        let unchoked = self.peers.values().filter(|p| !p.am_choking).count();
        if unchoked >= limit {
            return;
        }
        let seeding = self.status == TorrentStatus::Finished;
        let snub = self.config.snub_timeout;
        let mut waiting: Vec<(u64, PeerKey)> = self
            .peers
            .iter()
            .filter(|(_, p)| {
                p.peer_interested && p.am_choking && (seeding || !p.is_snubbing(now, snub))
            })
            .map(|(k, p)| (if seeding { p.up_round } else { p.down_round }, *k))
            .collect();
        waiting.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        for (_, key) in waiting.into_iter().take(limit - unchoked) {
            self.unchoke_peer(now, key, out);
        }
    }

    /// Returns upload tokens taken for a read whose bytes will not be sent.
    fn refund_upload(&mut self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        if let Some(rate) = self.upload_rate.as_mut() {
            rate.refund(bytes);
        }
    }

    /// Bans `ip` for bad data: closes its connections and forgets it.
    /// `ip` is compared in canonical form, so an IPv4-mapped connection of
    /// the same host is closed too.
    fn ban(&mut self, ip: IpAddr, out: &mut Vec<Effect>) {
        let ip = crate::ban::canonical_ip(ip);
        out.push(Effect::Alert(Alert::PeerBanned { ip }));
        let keys: Vec<PeerKey> = self
            .sorted_keys()
            .into_iter()
            .filter(|k| {
                self.peers
                    .get(k)
                    .is_some_and(|p| crate::ban::canonical_ip(p.addr.ip()) == ip)
            })
            .collect();
        for key in keys {
            self.remove_peer(key);
            out.push(Effect::Disconnect { peer: key });
        }
        self.pool.remove_ip(ip);
        self.fill_connections(out);
        self.pump_uploads(out);
    }

    /// Tells every PEX-capable peer which peers we connected to and dropped
    /// since our last message to it (BEP 11).
    fn send_pex(&mut self, now: Instant, out: &mut Vec<Effect>) {
        if !self.config.pex {
            return;
        }
        // Only addresses others can dial: an incoming peer's source port is
        // useless until its extension handshake names its listen port.
        let connected: HashMap<SocketAddr, u8> = self
            .peers
            .values()
            .filter_map(|p| {
                let mut flags = dravity_wire::PEX_REACHABLE;
                if p.has.is_full() {
                    flags |= dravity_wire::PEX_SEED;
                }
                p.reachable.map(|a| (a, flags))
            })
            .collect();
        for key in self.sorted_keys() {
            let Some(peer) = self.peers.get_mut(&key) else {
                continue;
            };
            let Some(id) = peer.ext.ut_pex else {
                continue;
            };
            let mut added: Vec<(SocketAddr, u8)> = connected
                .iter()
                .map(|(a, f)| (*a, *f))
                .filter(|(a, _)| Some(*a) != peer.reachable && !peer.pex_known.contains(a))
                .collect();
            added.sort_unstable();
            added.truncate(ext::PEX_MAX_SENT);
            let mut dropped: Vec<SocketAddr> = peer
                .pex_known
                .iter()
                .copied()
                .filter(|a| !connected.contains_key(a))
                .collect();
            dropped.sort_unstable();
            dropped.truncate(ext::PEX_MAX_SENT);
            if added.is_empty() && dropped.is_empty() {
                continue;
            }
            peer.pex_known.extend(added.iter().map(|(a, _)| *a));
            for addr in &dropped {
                peer.pex_known.remove(addr);
            }
            Self::send(peer, key, ext::pex_message(id, &added, &dropped), now, out);
        }
    }
}

/// IPv4 address BEP 6 uses for `ip`. Mapped IPv6 addresses use the embedded
/// IPv4 address; other IPv6 addresses have no allowed fast set.
fn ipv4_for_allowed_fast(ip: IpAddr) -> Option<std::net::Ipv4Addr> {
    match ip {
        IpAddr::V4(ip) => Some(ip),
        IpAddr::V6(ip) => ip.to_ipv4_mapped(),
    }
}

/// A block another peer is already fetching that `key` may request too:
/// any in-flight block in endgame, otherwise only one whose request is older
/// than [`Config::steal_after`] (a slow peer). Never more than
/// [`MAX_BLOCK_OWNERS`] requests per block, and never twice from one peer.
fn duplicate_candidate(
    picker: &Picker,
    owners: &HashMap<BlockRef, Vec<(PeerKey, Instant)>>,
    config: &Config,
    key: PeerKey,
    has: &Bitfield,
    now: Instant,
) -> Option<BlockRef> {
    let endgame = config.endgame && !picker.has_unrequested();
    picker.requested_blocks(has).find(|block| {
        let Some(holders) = owners.get(block) else {
            return false;
        };
        if holders.len() >= MAX_BLOCK_OWNERS || holders.iter().any(|(k, _)| *k == key) {
            return false;
        }
        endgame
            || holders
                .iter()
                .all(|(_, since)| now.saturating_duration_since(*since) >= config.steal_after)
    })
}

/// The answer to a `ut_metadata` request for `piece`.
fn metadata_reply(info: Option<&Bytes>, piece: u32) -> MetadataMessage {
    let Some(info) = info else {
        return MetadataMessage::Reject { piece };
    };
    let start = usize::try_from(piece)
        .ok()
        .and_then(|p| p.checked_mul(METADATA_PIECE_LEN as usize));
    match start {
        Some(start) if start < info.len() => {
            let end = (start + METADATA_PIECE_LEN as usize).min(info.len());
            MetadataMessage::Data {
                piece,
                total_size: u64::try_from(info.len()).unwrap_or(u64::MAX),
                data: info.slice(start..end),
            }
        }
        _ => MetadataMessage::Reject { piece },
    }
}
