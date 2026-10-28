//! The per-torrent state machine.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use dravity_core::{BLOCK_SIZE, BlockRef, PieceLayout};
use dravity_wire::{ExtendedHandshake, METADATA_PIECE_LEN, Message, MetadataMessage, Reserved};

use crate::bitfield::Bitfield;
use crate::ext::{self, PeerExtensions};
use crate::picker::{Picker, Received};
use crate::pool::PeerPool;
use crate::rate::TokenBucket;
use crate::types::{Alert, AnnounceEvent, Config, Effect, Event, PeerKey, Stats, TorrentStatus};
use crate::window::Window;

/// Most peers that may hold a request for the same block (endgame and
/// stealing): the original requester plus one more.
const MAX_BLOCK_OWNERS: usize = 2;

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
    ext: PeerExtensions,
    /// Addresses we told this peer about with PEX.
    pex_known: HashSet<SocketAddr>,
    last_recv: Instant,
    last_sent: Instant,
}

impl Peer {
    fn new(addr: SocketAddr, piece_count: u32, config: &Config, now: Instant) -> Self {
        let cap = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        Self {
            addr,
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
            extended: false,
            ext: PeerExtensions::default(),
            pex_known: HashSet::new(),
            last_recv: now,
            last_sent: now,
        }
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
    /// The announce the driver is currently performing.
    pending_announce: Option<AnnounceEvent>,
    /// DHT and PEX addresses refused after a magnet turns out private.
    swarm_ban: HashSet<SocketAddr>,
    dht: DhtSchedule,
    next_pex: Option<Instant>,
    rate: Option<TokenBucket>,
    /// The bencoded `info` dictionary, served to peers with BEP 9.
    info_bytes: Option<Bytes>,
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
            pending_announce: None,
            swarm_ban: HashSet::new(),
            dht: DhtSchedule::default(),
            next_pex: None,
            rate: None,
            info_bytes: None,
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
            downloaded: self.downloaded,
            uploaded: self.uploaded,
            peers: self.peers.len(),
            connecting: self.pool.connecting(),
            known_peers: self.pool.known(),
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
        if let Some(rate) = self.rate.as_mut() {
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
            } => self.on_connected(now, peer, addr, reserved, &mut out),
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
            Event::StorageFailed { error } => self.fail(error, &mut out),
            Event::Announced { peers, interval } => {
                self.on_announced(now, peers, interval, &mut out);
            }
            Event::AddPeers { peers } => {
                self.add_to_pool(peers);
                self.fill_connections(&mut out);
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
        if self.picker.is_complete() {
            self.finish(out);
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

    fn finish(&mut self, out: &mut Vec<Effect>) {
        self.set_status(TorrentStatus::Finished, out);
        out.push(Effect::Alert(Alert::Finished));
        self.pool.clear_known();
        if self.started_announced {
            self.push_announce(AnnounceEvent::Completed, out);
        }
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
        if matches!(
            self.status,
            TorrentStatus::Finished | TorrentStatus::Stopped
        ) {
            return;
        }
        let connected: HashSet<SocketAddr> = self.peers.values().map(|p| p.addr).collect();
        let banned = &self.swarm_ban;
        self.pool.add(
            peers.into_iter().filter(|addr| !banned.contains(addr)),
            |a| connected.contains(&a),
        );
    }

    /// Remembers `addr` so a later tick can dial it again.
    fn schedule_retry(&mut self, now: Instant, addr: SocketAddr) {
        if self.swarm_ban.contains(&addr)
            || !matches!(
                self.status,
                TorrentStatus::Downloading | TorrentStatus::Checking
            )
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
        if self.status != TorrentStatus::Downloading {
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
        out: &mut Vec<Effect>,
    ) {
        self.pool.dial_done(addr);
        // Peers may connect while checking: a magnet's metadata fetch hands
        // its connections over before the data check ends.
        if self.swarm_ban.contains(&addr)
            || !matches!(
                self.status,
                TorrentStatus::Downloading | TorrentStatus::Checking
            )
            || self.peers.len() >= self.config.max_peers
            || self.peers.contains_key(&key)
            || Self::is_connected(&self.peers, addr)
        {
            out.push(Effect::Disconnect { peer: key });
            return;
        }
        let mut peer = Peer::new(addr, self.picker.layout().piece_count(), &self.config, now);
        peer.extended = reserved.extension_protocol();
        if peer.extended {
            let size = self
                .info_bytes
                .as_ref()
                .and_then(|b| u64::try_from(b.len()).ok());
            Self::send(
                &mut peer,
                key,
                ext::our_handshake(size, self.config.pex),
                now,
                out,
            );
        }
        self.peers.insert(key, peer);
        if self.status == TorrentStatus::Downloading {
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

    fn send_bitfield(&mut self, now: Instant, key: PeerKey, out: &mut Vec<Effect>) {
        if self.picker.have().count() == 0 {
            return;
        }
        let message = Message::Bitfield(self.picker.have().to_bytes());
        if let Some(peer) = self.peers.get_mut(&key) {
            Self::send(peer, key, message, now, out);
        }
    }

    /// Forgets a connected peer and returns its address.
    fn remove_peer(&mut self, key: PeerKey) -> Option<SocketAddr> {
        let peer = self.peers.remove(&key)?;
        for (block, _) in &peer.inflight {
            self.release_owner(key, *block);
        }
        self.picker.remove_peer_pieces(&peer.has);
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
            }
            Message::HaveAll => {
                if !bitfield_ok {
                    return self.disconnect(key, out);
                }
                self.set_peer_pieces(key, Bitfield::full(piece_count));
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
            }
            Message::Choke => {
                peer.peer_choking = true;
                let blocks: Vec<BlockRef> = peer.inflight.drain(..).map(|(b, _)| b).collect();
                for block in blocks {
                    self.release_owner(key, block);
                }
            }
            Message::Unchoke => peer.peer_choking = false,
            Message::Interested => peer.peer_interested = true,
            Message::NotInterested => peer.peer_interested = false,
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
            // Download-only: we never unchoke, so requests and cancels need
            // no answer. Fast-extension behavior is Phase 3.
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
        if let Some(peer) = self.peers.get_mut(&key) {
            let before = peer.inflight.len();
            peer.inflight.retain(|(b, _)| *b != block);
            if peer.inflight.len() < before {
                peer.window.on_block();
            }
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
        let Some(peer) = self.peers.get_mut(&key) else {
            return;
        };
        if peer.peer_choking || !peer.am_interested {
            return;
        }
        let limit = peer.request_limit();
        while peer.inflight.len() < limit && self.pending_writes < self.config.max_pending_writes {
            if self
                .rate
                .as_ref()
                .is_some_and(|r| r.available() < u64::from(BLOCK_SIZE))
            {
                break;
            }
            let block = match self.picker.pick(&peer.has) {
                Some(block) => block,
                None => match duplicate_candidate(
                    &self.picker,
                    &self.owners,
                    &self.config,
                    key,
                    &peer.has,
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
            peer.inflight.push((block, now));
            peer.asked.insert((block.piece, block.offset));
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
        self.fill_all_requests(now, out);
    }

    fn add_have_bytes(&mut self, piece: u32) {
        let size = self.picker.layout().piece_size(piece).unwrap_or(0);
        self.bytes_have = self.bytes_have.saturating_add(u64::from(size));
    }

    fn on_tick(&mut self, now: Instant, out: &mut Vec<Effect>) {
        if matches!(
            self.status,
            TorrentStatus::Downloading | TorrentStatus::Checking
        ) {
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
        if matches!(
            self.status,
            TorrentStatus::Downloading | TorrentStatus::Checking
        ) {
            self.dht.poll(now, &self.config, out);
        }
        if self.status == TorrentStatus::Downloading {
            self.fill_all_requests(now, out);
            if self.next_pex.is_some_and(|t| now >= t) {
                self.next_pex = Some(now + self.config.pex_interval);
                self.send_pex(now, out);
            }
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

    /// Tells every PEX-capable peer which peers we connected to and dropped
    /// since our last message to it (BEP 11).
    fn send_pex(&mut self, now: Instant, out: &mut Vec<Effect>) {
        if !self.config.pex {
            return;
        }
        let connected: HashSet<SocketAddr> = self.peers.values().map(|p| p.addr).collect();
        for key in self.sorted_keys() {
            let Some(peer) = self.peers.get_mut(&key) else {
                continue;
            };
            let Some(id) = peer.ext.ut_pex else {
                continue;
            };
            let mut added: Vec<SocketAddr> = connected
                .iter()
                .copied()
                .filter(|a| *a != peer.addr && !peer.pex_known.contains(a))
                .collect();
            added.sort_unstable();
            added.truncate(ext::PEX_MAX_SENT);
            let mut dropped: Vec<SocketAddr> = peer
                .pex_known
                .iter()
                .copied()
                .filter(|a| !connected.contains(a))
                .collect();
            dropped.sort_unstable();
            dropped.truncate(ext::PEX_MAX_SENT);
            if added.is_empty() && dropped.is_empty() {
                continue;
            }
            peer.pex_known.extend(added.iter().copied());
            for addr in &dropped {
                peer.pex_known.remove(addr);
            }
            Self::send(peer, key, ext::pex_message(id, &added, &dropped), now, out);
        }
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
