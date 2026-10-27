//! The per-torrent state machine.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use dravity_core::{BlockRef, PieceLayout};
use dravity_wire::Message;

use crate::bitfield::Bitfield;
use crate::picker::{Picker, Received};
use crate::types::{Alert, AnnounceEvent, Config, Effect, Event, PeerKey, Stats, TorrentStatus};

/// Upper bound on queued, not yet tried peer addresses.
const MAX_POOL: usize = 2_000;

/// What the engine knows about one connected peer.
#[derive(Debug, Clone)]
struct Peer {
    addr: SocketAddr,
    has: Bitfield,
    /// Whether any message arrived yet (`bitfield` is only legal first).
    seen_message: bool,
    am_interested: bool,
    peer_choking: bool,
    peer_interested: bool,
    inflight: Vec<(BlockRef, Instant)>,
    last_recv: Instant,
    last_sent: Instant,
}

impl Peer {
    fn new(addr: SocketAddr, piece_count: u32, now: Instant) -> Self {
        Self {
            addr,
            has: Bitfield::new(piece_count),
            seen_message: false,
            am_interested: false,
            peer_choking: true,
            peer_interested: false,
            inflight: Vec::new(),
            last_recv: now,
            last_sent: now,
        }
    }
}

/// Progress of the initial data check.
#[derive(Debug, Clone, Copy)]
struct Check {
    next: u32,
    outstanding: u32,
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
    connecting: HashSet<SocketAddr>,
    pool: VecDeque<SocketAddr>,
    check: Option<Check>,
    next_announce: Option<Instant>,
    announce_in_flight: bool,
    started_announced: bool,
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
            connecting: HashSet::new(),
            pool: VecDeque::new(),
            check: None,
            next_announce: None,
            announce_in_flight: false,
            started_announced: false,
            downloaded: 0,
            uploaded: 0,
            bytes_have,
            pending_writes: 0,
        }
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
            downloaded: self.downloaded,
            uploaded: self.uploaded,
            peers: self.peers.len(),
            connecting: self.connecting.len(),
            known_peers: self.pool.len(),
            pending_writes: self.pending_writes,
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
        match event {
            Event::Start => self.on_start(now, &mut out),
            Event::Stop => self.stop(&mut out),
            Event::Tick => self.on_tick(now, &mut out),
            Event::PeerConnected { peer, addr } => self.on_connected(now, peer, addr, &mut out),
            Event::PeerConnectFailed { addr } => {
                self.connecting.remove(&addr);
                self.fill_connections(&mut out);
            }
            Event::PeerDisconnected { peer } => {
                self.remove_peer(peer);
                self.fill_connections(&mut out);
            }
            Event::Message { peer, message } => self.on_message(now, peer, message, &mut out),
            Event::BlockWritten { block } => {
                self.pending_writes = self.pending_writes.saturating_sub(1);
                if let Some(piece) = self.picker.block_written(block) {
                    out.push(Effect::HashPiece { piece });
                }
                if self.pending_writes + 1 == self.config.max_pending_writes {
                    // Backpressure released: resume requesting.
                    for key in self.sorted_keys() {
                        self.fill_requests(now, key, &mut out);
                    }
                }
            }
            Event::WriteFailed { block, error } => {
                self.pending_writes = self.pending_writes.saturating_sub(1);
                self.picker.block_write_failed(block);
                self.fail(format!("storage write failed: {error}"), &mut out);
            }
            Event::PieceHashed { piece, ok } => self.on_hashed(now, piece, ok, &mut out),
            Event::Announced { peers, interval } => {
                self.on_announced(now, peers, interval, &mut out);
            }
            Event::AddPeers { peers } => {
                self.add_to_pool(peers);
                self.fill_connections(&mut out);
            }
            Event::AnnounceFailed { error } => {
                self.announce_in_flight = false;
                self.next_announce = Some(now + self.config.announce_retry);
                out.push(Effect::Alert(Alert::TrackerError { message: error }));
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
        if self.picker.is_complete() {
            self.finish(out);
            return;
        }
        self.set_status(TorrentStatus::Downloading, out);
        self.announce(now, AnnounceEvent::Started, out);
    }

    fn finish(&mut self, out: &mut Vec<Effect>) {
        self.set_status(TorrentStatus::Finished, out);
        out.push(Effect::Alert(Alert::Finished));
        self.pool.clear();
        if self.started_announced {
            self.push_announce(AnnounceEvent::Completed, out);
        }
    }

    fn stop(&mut self, out: &mut Vec<Effect>) {
        let mut keys: Vec<PeerKey> = self.peers.keys().copied().collect();
        keys.sort_unstable();
        for peer in keys {
            self.remove_peer(peer);
            out.push(Effect::Disconnect { peer });
        }
        if self.started_announced {
            self.push_announce(AnnounceEvent::Stopped, out);
            self.started_announced = false;
        }
        self.connecting.clear();
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
        self.announce_in_flight = false;
        let interval = interval
            .unwrap_or(self.config.default_announce_interval)
            .max(self.config.min_announce_interval);
        self.next_announce = Some(now + interval);
        self.add_to_pool(peers);
        self.fill_connections(out);
    }

    fn add_to_pool(&mut self, peers: Vec<SocketAddr>) {
        if self.status != TorrentStatus::Downloading {
            return;
        }
        let connected: HashSet<SocketAddr> = self.peers.values().map(|p| p.addr).collect();
        let mut queued: HashSet<SocketAddr> = self.pool.iter().copied().collect();
        for addr in peers {
            if self.pool.len() >= MAX_POOL {
                break;
            }
            if addr.port() == 0
                || connected.contains(&addr)
                || self.connecting.contains(&addr)
                || !queued.insert(addr)
            {
                continue;
            }
            self.pool.push_back(addr);
        }
    }

    fn fill_connections(&mut self, out: &mut Vec<Effect>) {
        if self.status != TorrentStatus::Downloading {
            return;
        }
        while self.peers.len() + self.connecting.len() < self.config.max_peers {
            let Some(addr) = self.pool.pop_front() else {
                break;
            };
            if self.peers.values().any(|p| p.addr == addr) || !self.connecting.insert(addr) {
                continue;
            }
            out.push(Effect::Connect { addr });
        }
    }

    fn on_connected(
        &mut self,
        now: Instant,
        key: PeerKey,
        addr: SocketAddr,
        out: &mut Vec<Effect>,
    ) {
        self.connecting.remove(&addr);
        if self.status != TorrentStatus::Downloading
            || self.peers.len() >= self.config.max_peers
            || self.peers.contains_key(&key)
            || self.peers.values().any(|p| p.addr == addr)
        {
            out.push(Effect::Disconnect { peer: key });
            return;
        }
        let mut peer = Peer::new(addr, self.picker.layout().piece_count(), now);
        if self.picker.have().count() > 0 {
            peer.last_sent = now;
            out.push(Effect::Send {
                peer: key,
                message: Message::Bitfield(self.picker.have().to_bytes()),
            });
        }
        self.peers.insert(key, peer);
    }

    fn remove_peer(&mut self, key: PeerKey) {
        if let Some(peer) = self.peers.remove(&key) {
            for (block, _) in peer.inflight {
                self.picker.release(block);
            }
            self.picker.remove_peer_pieces(&peer.has);
        }
    }

    fn disconnect(&mut self, key: PeerKey, out: &mut Vec<Effect>) {
        self.remove_peer(key);
        out.push(Effect::Disconnect { peer: key });
        self.fill_connections(out);
    }

    fn on_message(&mut self, now: Instant, key: PeerKey, message: Message, out: &mut Vec<Effect>) {
        let piece_count = self.picker.layout().piece_count();
        let Some(peer) = self.peers.get_mut(&key) else {
            return;
        };
        peer.last_recv = now;
        let first = !peer.seen_message;
        peer.seen_message = true;
        match message {
            Message::Bitfield(bits) => {
                if !first {
                    return self.disconnect(key, out);
                }
                match Bitfield::from_wire(&bits, piece_count) {
                    Ok(has) => self.set_peer_pieces(key, has),
                    Err(_) => return self.disconnect(key, out),
                }
            }
            Message::HaveAll | Message::HaveNone => {
                if !first {
                    return self.disconnect(key, out);
                }
                if message == Message::HaveAll {
                    self.set_peer_pieces(key, Bitfield::full(piece_count));
                }
            }
            Message::Have { piece } => {
                if piece >= piece_count {
                    return self.disconnect(key, out);
                }
                if peer.has.set(piece) {
                    self.picker.add_peer_piece(piece);
                }
            }
            Message::Choke => {
                peer.peer_choking = true;
                for (block, _) in peer.inflight.drain(..) {
                    self.picker.release(block);
                }
            }
            Message::Unchoke => peer.peer_choking = false,
            Message::Interested => peer.peer_interested = true,
            Message::NotInterested => peer.peer_interested = false,
            Message::Piece {
                piece,
                offset,
                data,
            } => self.on_block(key, piece, offset, data, out),
            // Phase 1 is download-only: we never unchoke, so requests and
            // cancels need no answer. Fast-extension and extension-protocol
            // messages are not negotiated yet.
            _ => {}
        }
        self.update_interest(now, key, out);
        self.fill_requests(now, key, out);
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
        if let Some(peer) = self.peers.get_mut(&key) {
            peer.inflight.retain(|(b, _)| *b != block);
        }
        self.downloaded = self.downloaded.saturating_add(u64::from(length));
        if self.picker.block_received(block) == Received::Accepted {
            self.pending_writes += 1;
            out.push(Effect::WriteBlock { block, data });
        }
    }

    fn send(peer: &mut Peer, key: PeerKey, message: Message, now: Instant, out: &mut Vec<Effect>) {
        peer.last_sent = now;
        out.push(Effect::Send { peer: key, message });
    }

    fn update_interest(&mut self, now: Instant, key: PeerKey, out: &mut Vec<Effect>) {
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

    fn fill_requests(&mut self, now: Instant, key: PeerKey, out: &mut Vec<Effect>) {
        if self.status != TorrentStatus::Downloading {
            return;
        }
        let Some(peer) = self.peers.get_mut(&key) else {
            return;
        };
        if peer.peer_choking || !peer.am_interested {
            return;
        }
        while peer.inflight.len() < self.config.max_requests_per_peer
            && self.pending_writes < self.config.max_pending_writes
        {
            let Some(block) = self.picker.pick(&peer.has) else {
                break;
            };
            peer.inflight.push((block, now));
            Self::send(peer, key, Message::Request(block.into()), now, out);
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
        } else if self.picker.piece_hashed(piece, true) {
            self.add_have_bytes(piece);
            out.push(Effect::Alert(Alert::PieceCompleted { piece }));
            for key in self.sorted_keys() {
                if let Some(peer) = self.peers.get_mut(&key) {
                    Self::send(peer, key, Message::Have { piece }, now, out);
                }
                self.update_interest(now, key, out);
            }
            if self.picker.is_complete() {
                self.finish(out);
                return;
            }
        }
        for key in self.sorted_keys() {
            self.fill_requests(now, key, out);
        }
    }

    fn add_have_bytes(&mut self, piece: u32) {
        let size = self.picker.layout().piece_size(piece).unwrap_or(0);
        self.bytes_have = self.bytes_have.saturating_add(u64::from(size));
    }

    fn on_tick(&mut self, now: Instant, out: &mut Vec<Effect>) {
        let mut dead = Vec::new();
        for key in self.sorted_keys() {
            let Some(peer) = self.peers.get_mut(&key) else {
                continue;
            };
            if now.saturating_duration_since(peer.last_recv) >= self.config.peer_timeout {
                dead.push(key);
                continue;
            }
            let timeout = self.config.request_timeout;
            let picker = &mut self.picker;
            peer.inflight.retain(|(block, since)| {
                let expired = now.saturating_duration_since(*since) >= timeout;
                if expired {
                    picker.release(*block);
                }
                !expired
            });
            if now.saturating_duration_since(peer.last_sent) >= self.config.keepalive_interval {
                Self::send(peer, key, Message::KeepAlive, now, out);
            }
        }
        for key in dead {
            self.remove_peer(key);
            out.push(Effect::Disconnect { peer: key });
        }
        if self.status == TorrentStatus::Downloading {
            for key in self.sorted_keys() {
                self.fill_requests(now, key, out);
            }
        }
        if matches!(
            self.status,
            TorrentStatus::Downloading | TorrentStatus::Finished
        ) && self.next_announce.is_some_and(|t| now >= t)
        {
            self.announce(now, AnnounceEvent::None, out);
        }
        self.fill_connections(out);
        out.push(Effect::Alert(Alert::Stats(self.stats())));
    }
}
