//! Fetching a magnet link's `info` dictionary from peers (BEP 9).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::time::Instant;

use bytes::Bytes;
use dravity_core::InfoHash;
use dravity_wire::{
    ExtendedHandshake, METADATA_PIECE_LEN, Message, MetadataMessage, Reserved, metadata_piece_count,
};

use crate::ext::{self, PeerExtensions};
use crate::pool::PeerPool;
use crate::state::DhtSchedule;
use crate::types::{
    Alert, AnnounceEvent, Config, Effect, Event, PeerInfo, PeerKey, Stats, TorrentStatus,
};

/// Metadata requests outstanding per peer.
const PIPELINE: usize = 2;

/// `left` reported to trackers before the torrent size is known. Non-zero so
/// trackers treat us as a leecher and return seeds.
const UNKNOWN_LEFT: u64 = 16 * 1024;

/// Most availability messages remembered per peer for the hand-over.
const MAX_REPLAY: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Piece {
    /// Not downloaded. `avoid` is a peer that just failed this piece; other
    /// peers may take it immediately, and that peer only after the instant.
    Missing {
        avoid: Option<(PeerKey, Instant)>,
    },
    Requested {
        peer: PeerKey,
        since: Instant,
    },
    Have,
}

impl Piece {
    const fn is_have(self) -> bool {
        matches!(self, Self::Have)
    }

    const fn is_requested(self) -> bool {
        matches!(self, Self::Requested { .. })
    }

    /// Whether `key` may be asked for this piece at `now`.
    fn can_ask(self, key: PeerKey, now: Instant) -> bool {
        match self {
            Self::Missing { avoid: None } => true,
            Self::Missing {
                avoid: Some((peer, at)),
            } => peer != key || now >= at,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
struct MetaPeer {
    addr: SocketAddr,
    /// The peer connected to us.
    incoming: bool,
    reserved: Reserved,
    ext: PeerExtensions,
    /// The raw extension handshake, replayed after the hand-over.
    handshake: Option<Bytes>,
    /// Availability messages (`bitfield`, `have all/none`, `have`).
    replay: Vec<Message>,
    choked: bool,
    last_recv: Instant,
    last_sent: Instant,
}

/// A connection handed from a [`MetadataState`] to the new
/// [`TorrentState`](crate::TorrentState).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoverPeer {
    /// The driver's handle (the connection stays open).
    pub peer: PeerKey,
    /// Remote address.
    pub addr: SocketAddr,
    /// Reserved bytes of the peer's handshake.
    pub reserved: Reserved,
    /// The peer connected to us: replay it as
    /// [`Event::PeerAccepted`](crate::Event::PeerAccepted) rather than
    /// [`Event::PeerConnected`](crate::Event::PeerConnected).
    pub incoming: bool,
    /// Messages to feed to the torrent after
    /// [`Event::PeerConnected`](crate::Event::PeerConnected), in order: the
    /// peer's extension handshake, its availability, and `unchoke` if it
    /// unchoked us.
    pub replay: Vec<Message>,
}

/// Everything a [`TorrentState`](crate::TorrentState) inherits from a
/// finished metadata fetch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Handover {
    /// Open connections.
    pub peers: Vec<HandoverPeer>,
    /// Addresses not connected yet (for [`Event::AddPeers`]).
    pub known: Vec<SocketAddr>,
    /// Dials already in flight. The new state must not dial them again.
    pub connecting: Vec<SocketAddr>,
    /// Addresses learned from DHT or PEX. A private torrent drops these.
    pub swarm: Vec<SocketAddr>,
    /// Whether a `started` announce was sent and not yet rejected.
    pub started_announced: bool,
    /// A `started` announce was sent and the tracker has not answered yet.
    pub started_pending: bool,
    /// A regular announce should follow the one in flight: the port we
    /// publish changed while it was on the wire.
    pub announce_again: bool,
}

/// Sans-IO metadata fetcher for a magnet link: connects to peers, exchanges
/// BEP 10 handshakes, downloads the `info` dictionary in 16 KiB pieces with
/// `ut_metadata` and checks it against the info-hash.
///
/// It understands the same [`Event`]s as [`TorrentState`](crate::TorrentState)
/// (trackers, DHT, peers, timer) and produces
/// [`Effect::MetadataReceived`] once the metadata is verified. The driver
/// then builds the torrent, creates a `TorrentState`, and replays
/// [`MetadataState::into_handover`] into it so open connections survive.
///
/// ```
/// use std::time::Instant;
/// use dravity_core::InfoHash;
/// use dravity_engine::{Config, Effect, Event, MetadataState, TorrentStatus};
///
/// let mut m = MetadataState::new(InfoHash::new([1; 20]), Config::default());
/// let fx = m.update(Instant::now(), Event::Start);
/// assert_eq!(m.status(), TorrentStatus::FetchingMetadata);
/// assert!(fx.iter().any(|e| matches!(e, Effect::Announce { .. })));
/// ```
#[derive(Debug)]
pub struct MetadataState {
    info_hash: InfoHash,
    config: Config,
    status: TorrentStatus,
    peers: HashMap<PeerKey, MetaPeer>,
    pool: PeerPool,
    next_announce: Option<Instant>,
    announce_in_flight: bool,
    started_announced: bool,
    /// The announce the driver is currently performing.
    pending_announce: Option<AnnounceEvent>,
    /// Send another regular announce when the one in flight finishes.
    announce_again: bool,
    dht: DhtSchedule,
    size: Option<usize>,
    /// Sizes a peer advertised that produced no metadata before another peer
    /// offered a different size.
    abandoned: HashSet<u64>,
    pieces: Vec<Piece>,
    buffer: Vec<u8>,
    /// Peers that sent pieces of the current attempt.
    sources: HashSet<PeerKey>,
    /// Addresses that contributed metadata which failed the info-hash.
    banned: HashSet<SocketAddr>,
    /// Addresses learned from DHT or PEX (not from a tracker or the user).
    swarm: HashSet<SocketAddr>,
    downloaded: u64,
    complete: bool,
}

impl MetadataState {
    /// A fetcher for `info_hash`.
    #[must_use]
    pub fn new(info_hash: InfoHash, config: Config) -> Self {
        Self {
            info_hash,
            config,
            status: TorrentStatus::Idle,
            peers: HashMap::new(),
            pool: PeerPool::default(),
            next_announce: None,
            announce_in_flight: false,
            started_announced: false,
            pending_announce: None,
            announce_again: false,
            dht: DhtSchedule::default(),
            size: None,
            abandoned: HashSet::new(),
            pieces: Vec::new(),
            buffer: Vec::new(),
            sources: HashSet::new(),
            banned: HashSet::new(),
            swarm: HashSet::new(),
            downloaded: 0,
            complete: false,
        }
    }

    /// Current lifecycle state.
    #[must_use]
    pub const fn status(&self) -> TorrentStatus {
        self.status
    }

    /// Whether the metadata was received and verified.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.complete
    }

    /// A snapshot of every connected peer, ordered by address. Piece counts
    /// are zero: the torrent's size is not known yet.
    #[must_use]
    pub fn peers(&self) -> Vec<PeerInfo> {
        let mut peers: Vec<PeerInfo> = self
            .peers
            .iter()
            .map(|(&key, p)| PeerInfo {
                key,
                addr: p.addr,
                incoming: p.incoming,
                pieces_have: 0,
                piece_count: 0,
                downloaded: 0,
                uploaded: 0,
                am_choking: true,
                am_interested: false,
                peer_choking: p.choked,
                peer_interested: false,
            })
            .collect();
        peers.sort_by_key(|p| (p.addr, p.key));
        peers
    }

    /// A progress snapshot (sizes are unknown, so they are zero).
    #[must_use]
    pub fn stats(&self) -> Stats {
        Stats {
            status: self.status,
            piece_count: 0,
            pieces_have: 0,
            total_length: 0,
            bytes_have: 0,
            wanted_left: 0,
            downloaded: self.downloaded,
            uploaded: 0,
            peers: self.peers.len(),
            connecting: self.pool.connecting(),
            known_peers: self.pool.known(),
            pending_writes: 0,
            unchoked: 0,
            interested: 0,
            pending_reads: 0,
            banned: self.banned.len(),
        }
    }

    /// Open connections and known addresses, for the new torrent state.
    #[must_use]
    pub fn into_handover(mut self) -> Handover {
        let mut keys: Vec<PeerKey> = self.peers.keys().copied().collect();
        keys.sort_unstable();
        let peers = keys
            .into_iter()
            .filter_map(|key| {
                let p = self.peers.remove(&key)?;
                let mut replay = Vec::with_capacity(p.replay.len() + 2);
                if let Some(payload) = p.handshake {
                    replay.push(Message::Extended {
                        id: ext::HANDSHAKE_ID,
                        payload,
                    });
                }
                replay.extend(p.replay);
                if !p.choked {
                    replay.push(Message::Unchoke);
                }
                Some(HandoverPeer {
                    peer: key,
                    addr: p.addr,
                    reserved: p.reserved,
                    incoming: p.incoming,
                    replay,
                })
            })
            .collect();
        let (known, connecting) = self.pool.drain_handover();
        let mut swarm: Vec<SocketAddr> = self.swarm.iter().copied().collect();
        swarm.sort_unstable();
        Handover {
            peers,
            known,
            connecting,
            swarm,
            started_announced: self.started_announced,
            started_pending: self.pending_announce == Some(AnnounceEvent::Started),
            announce_again: self.announce_again,
        }
    }

    /// Processes one event and returns the effects the driver must perform.
    pub fn update(&mut self, now: Instant, event: Event) -> Vec<Effect> {
        let mut out = Vec::new();
        if self.status == TorrentStatus::Stopped {
            return out;
        }
        // `Stop` still runs after the metadata is verified, so a failure
        // while the driver builds storage can finish the task.
        if matches!(event, Event::Stop) {
            self.stop(&mut out);
            return out;
        }
        if self.complete {
            return out;
        }
        match event {
            Event::Start => self.on_start(now, &mut out),
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
            }
            Event::Message { peer, message } => self.on_message(now, peer, message, &mut out),
            Event::Announced { peers, interval } => {
                self.pending_announce = None;
                self.announce_in_flight = false;
                let interval = interval
                    .unwrap_or(self.config.default_announce_interval)
                    .max(self.config.min_announce_interval);
                self.next_announce = Some(now + interval);
                self.note_swarm(&peers, false);
                self.add_to_pool(peers);
                self.fill_connections(&mut out);
                self.settle_reannounce(now, &mut out);
            }
            Event::AnnounceFailed { error } => {
                if self.pending_announce == Some(AnnounceEvent::Started) {
                    self.started_announced = false;
                }
                self.pending_announce = None;
                self.announce_in_flight = false;
                self.next_announce = Some(now + self.config.announce_retry);
                out.push(Effect::Alert(Alert::TrackerError { message: error }));
                self.settle_reannounce(now, &mut out);
            }
            Event::Reannounce => self.reannounce(now, &mut out),
            Event::AddPeers { peers } => {
                self.note_swarm(&peers, false);
                self.add_to_pool(peers);
                self.fill_connections(&mut out);
            }
            Event::DhtPeers { peers, failed } => {
                self.dht.done(now, &self.config, failed);
                self.note_swarm(&peers, true);
                self.add_to_pool(peers);
                self.fill_connections(&mut out);
            }
            // Storage and hashing do not exist before the metadata.
            _ => {}
        }
        out
    }

    fn on_start(&mut self, now: Instant, out: &mut Vec<Effect>) {
        if self.status != TorrentStatus::Idle {
            return;
        }
        self.status = TorrentStatus::FetchingMetadata;
        out.push(Effect::Alert(Alert::StateChanged(self.status)));
        self.announce(now, AnnounceEvent::Started, out);
        self.dht.poll(now, &self.config, out);
        self.fill_connections(out);
    }

    fn stop(&mut self, out: &mut Vec<Effect>) {
        let mut keys: Vec<PeerKey> = self.peers.keys().copied().collect();
        keys.sort_unstable();
        for peer in keys {
            self.peers.remove(&peer);
            out.push(Effect::Disconnect { peer });
        }
        if self.started_announced {
            self.push_announce(AnnounceEvent::Stopped, out);
            self.started_announced = false;
        }
        self.pool.clear();
        self.swarm.clear();
        self.status = TorrentStatus::Stopped;
        out.push(Effect::Alert(Alert::StateChanged(self.status)));
    }

    fn announce(&mut self, now: Instant, event: AnnounceEvent, out: &mut Vec<Effect>) {
        if self.announce_in_flight {
            return;
        }
        self.announce_again = false;
        self.announce_in_flight = true;
        self.pending_announce = Some(event);
        self.next_announce = Some(now + self.config.default_announce_interval);
        self.push_announce(event, out);
        if event == AnnounceEvent::Started {
            self.started_announced = true;
        }
    }

    /// The address we publish changed. Announce again while metadata is
    /// still being fetched. An announce already on the wire is followed by
    /// one more.
    fn reannounce(&mut self, now: Instant, out: &mut Vec<Effect>) {
        if !self.started_announced || self.status != TorrentStatus::FetchingMetadata {
            return;
        }
        if self.announce_in_flight {
            self.announce_again = true;
            return;
        }
        self.announce(now, AnnounceEvent::None, out);
    }

    fn settle_reannounce(&mut self, now: Instant, out: &mut Vec<Effect>) {
        if !self.announce_again {
            return;
        }
        self.announce_again = false;
        if self.started_announced && self.status == TorrentStatus::FetchingMetadata {
            self.announce(now, AnnounceEvent::None, out);
        }
    }

    fn push_announce(&self, event: AnnounceEvent, out: &mut Vec<Effect>) {
        out.push(Effect::Announce {
            event,
            uploaded: 0,
            downloaded: self.downloaded,
            left: UNKNOWN_LEFT,
        });
    }

    fn is_connected(peers: &HashMap<PeerKey, MetaPeer>, addr: SocketAddr) -> bool {
        peers.values().any(|p| p.addr == addr)
    }

    fn note_swarm(&mut self, peers: &[SocketAddr], from_swarm: bool) {
        if from_swarm {
            self.swarm.extend(peers.iter().copied());
        } else {
            for addr in peers {
                self.swarm.remove(addr);
            }
        }
    }

    fn add_to_pool(&mut self, peers: Vec<SocketAddr>) {
        let connected: HashSet<SocketAddr> = self.peers.values().map(|p| p.addr).collect();
        let banned = &self.banned;
        self.pool
            .add(peers.into_iter().filter(|a| !banned.contains(a)), |a| {
                connected.contains(&a)
            });
    }

    fn schedule_retry(&mut self, now: Instant, addr: SocketAddr) {
        if self.status != TorrentStatus::FetchingMetadata {
            return;
        }
        let peers = &self.peers;
        self.pool
            .schedule_retry(now + self.config.peer_retry, addr, |a| {
                Self::is_connected(peers, a)
            });
    }

    fn fill_connections(&mut self, out: &mut Vec<Effect>) {
        if self.status != TorrentStatus::FetchingMetadata {
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
        if self.banned.contains(&addr)
            || self.status != TorrentStatus::FetchingMetadata
            || self.peers.len() >= self.config.max_peers
            || self.peers.contains_key(&key)
            || Self::is_connected(&self.peers, addr)
        {
            out.push(Effect::Disconnect { peer: key });
            return;
        }
        // Peers without the extension protocol cannot send metadata, but
        // they are kept for the download that follows.
        let peer = MetaPeer {
            addr,
            incoming,
            reserved,
            ext: PeerExtensions::default(),
            handshake: None,
            replay: Vec::new(),
            choked: true,
            last_recv: now,
            last_sent: now,
        };
        if reserved.extension_protocol() {
            out.push(Effect::Send {
                peer: key,
                message: ext::our_handshake(&self.config, None, addr),
            });
        }
        self.peers.insert(key, peer);
    }

    fn remove_peer(&mut self, key: PeerKey) -> Option<SocketAddr> {
        let peer = self.peers.remove(&key)?;
        for piece in &mut self.pieces {
            if matches!(piece, Piece::Requested { peer, .. } if *peer == key) {
                *piece = Piece::Missing { avoid: None };
            }
        }
        Some(peer.addr)
    }

    fn disconnect(&mut self, key: PeerKey, out: &mut Vec<Effect>) {
        self.remove_peer(key);
        out.push(Effect::Disconnect { peer: key });
    }

    fn on_message(&mut self, now: Instant, key: PeerKey, message: Message, out: &mut Vec<Effect>) {
        let Some(peer) = self.peers.get_mut(&key) else {
            return;
        };
        peer.last_recv = now;
        match message {
            Message::Extended { id, payload } if peer.reserved.extension_protocol() => {
                if id == ext::HANDSHAKE_ID {
                    let Ok(hs) = ExtendedHandshake::decode(&payload) else {
                        return self.disconnect(key, out);
                    };
                    peer.ext.update(&hs);
                    peer.handshake = Some(payload);
                    self.learn_size(key);
                } else if id == ext::OUR_UT_METADATA {
                    match MetadataMessage::decode(&payload) {
                        Ok(MetadataMessage::Data {
                            piece,
                            total_size,
                            data,
                        }) => self.on_data(key, piece, total_size, &data, out),
                        Ok(MetadataMessage::Reject { piece }) => {
                            // One reject means this piece, not the peer: a
                            // client still fetching metadata rejects what it
                            // does not have yet.
                            let retry_at = now + self.config.request_timeout;
                            self.avoid(key, piece, retry_at);
                        }
                        Ok(MetadataMessage::Request { piece }) => {
                            // We have nothing to serve yet.
                            if let Some(id) = peer.ext.ut_metadata
                                && let Ok(payload) = (MetadataMessage::Reject { piece }).encode()
                            {
                                peer.last_sent = now;
                                out.push(Effect::Send {
                                    peer: key,
                                    message: Message::Extended { id, payload },
                                });
                            }
                        }
                        _ => {}
                    }
                } else if id == ext::OUR_UT_PEX
                    && self.config.pex
                    && let Some(added) = ext::pex_added(&payload)
                {
                    self.note_swarm(&added, true);
                    self.add_to_pool(added);
                    self.fill_connections(out);
                }
            }
            Message::Choke => peer.choked = true,
            Message::Unchoke => peer.choked = false,
            // Keep only the first bitfield; a second one is a protocol error
            // the torrent state catches on replay.
            m @ (Message::Bitfield(_) | Message::HaveAll | Message::HaveNone)
                if peer
                    .replay
                    .iter()
                    .all(|r| matches!(r, Message::Have { .. })) =>
            {
                peer.replay.insert(0, m);
            }
            m @ Message::Have { .. } if peer.replay.len() < MAX_REPLAY => peer.replay.push(m),
            // Anything else is irrelevant before the download.
            _ => {}
        }
        if !self.complete {
            self.fill_requests(now, out);
        }
    }

    /// Adopts the metadata size the first capable peer advertises.
    fn learn_size(&mut self, key: PeerKey) {
        if self.size.is_some() {
            return;
        }
        let Some(size) = self.peers.get(&key).and_then(|p| p.ext.metadata_size) else {
            return;
        };
        if size == 0 || size > self.config.max_metadata_size || self.abandoned.contains(&size) {
            return;
        }
        let (Ok(len), Ok(count)) = (
            usize::try_from(size),
            usize::try_from(metadata_piece_count(size)),
        ) else {
            return;
        };
        self.size = Some(len);
        self.buffer = vec![0; len];
        self.pieces = vec![Piece::Missing { avoid: None }; count];
    }

    /// Returns a request to the pool and keeps `key` off it until `retry_at`
    /// when another peer can take it sooner.
    fn avoid(&mut self, key: PeerKey, index: u32, retry_at: Instant) {
        let slot = usize::try_from(index)
            .ok()
            .and_then(|i| self.pieces.get_mut(i));
        if let Some(piece) = slot
            && matches!(piece, Piece::Requested { peer, .. } if *peer == key)
        {
            *piece = Piece::Missing {
                avoid: Some((key, retry_at)),
            };
        }
    }

    fn on_data(
        &mut self,
        key: PeerKey,
        index: u32,
        total_size: u64,
        data: &[u8],
        out: &mut Vec<Effect>,
    ) {
        let Some(size) = self.size else {
            return;
        };
        if u64::try_from(size).ok() != Some(total_size) {
            return;
        }
        let Ok(i) = usize::try_from(index) else {
            return;
        };
        let start = i.saturating_mul(METADATA_PIECE_LEN as usize);
        let end = (start + METADATA_PIECE_LEN as usize).min(size);
        let (Some(piece), Some(dst)) = (self.pieces.get_mut(i), self.buffer.get_mut(start..end))
        else {
            return;
        };
        if piece.is_have() || dst.len() != data.len() {
            return;
        }
        dst.copy_from_slice(data);
        *piece = Piece::Have;
        self.sources.insert(key);
        self.downloaded = self
            .downloaded
            .saturating_add(u64::try_from(data.len()).unwrap_or(0));
        if self.pieces.iter().all(|p| p.is_have()) {
            self.verify(out);
        }
    }

    fn verify(&mut self, out: &mut Vec<Effect>) {
        if InfoHash::from_info_bytes(&self.buffer) == self.info_hash {
            self.complete = true;
            out.push(Effect::MetadataReceived {
                info: Bytes::from(std::mem::take(&mut self.buffer)),
            });
            out.push(Effect::Alert(Alert::MetadataReceived));
            return;
        }
        // Someone sent garbage (or a different size). Drop every peer that
        // contributed and do not dial those addresses again.
        let mut bad: Vec<PeerKey> = self.sources.drain().collect();
        bad.sort_unstable();
        for key in bad {
            if let Some(addr) = self.remove_peer(key) {
                self.banned.insert(addr);
                self.swarm.remove(&addr);
            }
            out.push(Effect::Disconnect { peer: key });
        }
        self.size = None;
        self.pieces.clear();
        self.buffer.clear();
        let mut keys: Vec<PeerKey> = self.peers.keys().copied().collect();
        keys.sort_unstable();
        for key in keys {
            self.learn_size(key);
        }
    }

    fn fill_requests(&mut self, now: Instant, out: &mut Vec<Effect>) {
        if self.size.is_none() || self.status != TorrentStatus::FetchingMetadata {
            return;
        }
        let size = self.size.and_then(|s| u64::try_from(s).ok());
        let mut keys: Vec<PeerKey> = self.peers.keys().copied().collect();
        keys.sort_unstable();
        for key in keys {
            let Some(peer) = self.peers.get_mut(&key) else {
                continue;
            };
            let Some(id) = peer.ext.ut_metadata else {
                continue;
            };
            if peer.ext.metadata_size != size {
                continue;
            }
            let mut outstanding = self
                .pieces
                .iter()
                .filter(|p| matches!(p, Piece::Requested { peer, .. } if *peer == key))
                .count();
            for (index, piece) in self.pieces.iter_mut().enumerate() {
                if outstanding >= PIPELINE {
                    break;
                }
                if !piece.can_ask(key, now) {
                    continue;
                }
                let Ok(index) = u32::try_from(index) else {
                    break;
                };
                let Ok(payload) = (MetadataMessage::Request { piece: index }).encode() else {
                    break;
                };
                *piece = Piece::Requested {
                    peer: key,
                    since: now,
                };
                outstanding += 1;
                peer.last_sent = now;
                out.push(Effect::Send {
                    peer: key,
                    message: Message::Extended { id, payload },
                });
            }
        }
    }

    fn on_tick(&mut self, now: Instant, out: &mut Vec<Effect>) {
        {
            let peers = &self.peers;
            self.pool.promote(now, |a| Self::is_connected(peers, a));
        }
        // Timed-out requests go to another peer. The peer that stalled is
        // skipped until `request_timeout` has passed again, unless it is the
        // only one that can serve the piece.
        let timeout = self.config.request_timeout;
        for piece in &mut self.pieces {
            if let Piece::Requested { peer, since } = *piece
                && now.saturating_duration_since(since) >= timeout
            {
                *piece = Piece::Missing {
                    avoid: Some((peer, now + timeout)),
                };
            }
        }
        self.abandon_stalled_size();
        let mut keys: Vec<PeerKey> = self.peers.keys().copied().collect();
        keys.sort_unstable();
        let mut dead = Vec::new();
        for key in keys {
            let Some(peer) = self.peers.get_mut(&key) else {
                continue;
            };
            if now.saturating_duration_since(peer.last_recv) >= self.config.peer_timeout {
                dead.push(key);
            } else if now.saturating_duration_since(peer.last_sent)
                >= self.config.keepalive_interval
            {
                peer.last_sent = now;
                out.push(Effect::Send {
                    peer: key,
                    message: Message::KeepAlive,
                });
            }
        }
        for key in dead {
            self.disconnect(key, out);
        }
        self.fill_requests(now, out);
        self.dht.poll(now, &self.config, out);
        if self.next_announce.is_some_and(|t| now >= t) {
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

    /// Drops a metadata size that has produced no bytes once another peer
    /// advertises a different in-range size. The first advertisement is
    /// otherwise kept, so one stalled peer would block the fetch forever.
    fn abandon_stalled_size(&mut self) {
        let Some(size) = self.size else {
            return;
        };
        if self
            .pieces
            .iter()
            .any(|piece| piece.is_have() || piece.is_requested())
        {
            return;
        }
        let stalled = self
            .pieces
            .iter()
            .any(|piece| matches!(piece, Piece::Missing { avoid: Some(_), .. }));
        if !stalled {
            return;
        }
        let Ok(size) = u64::try_from(size) else {
            return;
        };
        let alternative = self.peers.values().any(|peer| {
            peer.ext.metadata_size.is_some_and(|other| {
                other != size
                    && other > 0
                    && other <= self.config.max_metadata_size
                    && !self.abandoned.contains(&other)
            })
        });
        if !alternative {
            return;
        }
        self.abandoned.insert(size);
        self.size = None;
        self.pieces.clear();
        self.buffer.clear();
        self.sources.clear();
        let mut keys: Vec<PeerKey> = self.peers.keys().copied().collect();
        keys.sort_unstable();
        for key in keys {
            self.learn_size(key);
        }
    }
}
