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
use crate::types::{Alert, AnnounceEvent, Config, Effect, Event, PeerKey, Stats, TorrentStatus};

/// Metadata requests outstanding per peer.
const PIPELINE: usize = 2;

/// `left` reported to trackers before the torrent size is known. Non-zero so
/// trackers treat us as a leecher and return seeds.
const UNKNOWN_LEFT: u64 = 16 * 1024;

/// Most availability messages remembered per peer for the hand-over.
const MAX_REPLAY: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Piece {
    Missing,
    Requested { peer: PeerKey, since: Instant },
    Have,
}

#[derive(Debug, Clone)]
struct MetaPeer {
    addr: SocketAddr,
    reserved: Reserved,
    ext: PeerExtensions,
    /// The peer rejected a request: it cannot serve metadata.
    rejected: bool,
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
    /// Whether the `started` announce was sent.
    pub started_announced: bool,
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
    dht: DhtSchedule,
    size: Option<usize>,
    pieces: Vec<Piece>,
    buffer: Vec<u8>,
    /// Peers that sent pieces of the current attempt.
    sources: HashSet<PeerKey>,
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
            dht: DhtSchedule::default(),
            size: None,
            pieces: Vec::new(),
            buffer: Vec::new(),
            sources: HashSet::new(),
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

    /// A progress snapshot (sizes are unknown, so they are zero).
    #[must_use]
    pub fn stats(&self) -> Stats {
        Stats {
            status: self.status,
            piece_count: 0,
            pieces_have: 0,
            total_length: 0,
            bytes_have: 0,
            downloaded: self.downloaded,
            uploaded: 0,
            peers: self.peers.len(),
            connecting: self.pool.connecting(),
            known_peers: self.pool.known(),
            pending_writes: 0,
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
                    replay,
                })
            })
            .collect();
        Handover {
            peers,
            known: self.pool.drain_known(),
            started_announced: self.started_announced,
        }
    }

    /// Processes one event and returns the effects the driver must perform.
    pub fn update(&mut self, now: Instant, event: Event) -> Vec<Effect> {
        let mut out = Vec::new();
        if self.complete || self.status == TorrentStatus::Stopped {
            return out;
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
            Event::Announced { peers, interval } => {
                self.announce_in_flight = false;
                let interval = interval
                    .unwrap_or(self.config.default_announce_interval)
                    .max(self.config.min_announce_interval);
                self.next_announce = Some(now + interval);
                self.add_to_pool(peers);
                self.fill_connections(&mut out);
            }
            Event::AnnounceFailed { error } => {
                self.announce_in_flight = false;
                self.next_announce = Some(now + self.config.announce_retry);
                out.push(Effect::Alert(Alert::TrackerError { message: error }));
            }
            Event::AddPeers { peers } => {
                self.add_to_pool(peers);
                self.fill_connections(&mut out);
            }
            Event::DhtPeers { peers } => {
                self.dht.done(now, &self.config);
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
        self.status = TorrentStatus::Stopped;
        out.push(Effect::Alert(Alert::StateChanged(self.status)));
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
            uploaded: 0,
            downloaded: self.downloaded,
            left: UNKNOWN_LEFT,
        });
    }

    fn is_connected(peers: &HashMap<PeerKey, MetaPeer>, addr: SocketAddr) -> bool {
        peers.values().any(|p| p.addr == addr)
    }

    fn add_to_pool(&mut self, peers: Vec<SocketAddr>) {
        let connected: HashSet<SocketAddr> = self.peers.values().map(|p| p.addr).collect();
        self.pool.add(peers, |a| connected.contains(&a));
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
        out: &mut Vec<Effect>,
    ) {
        self.pool.dial_done(addr);
        if self.status != TorrentStatus::FetchingMetadata
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
            reserved,
            ext: PeerExtensions::default(),
            rejected: false,
            handshake: None,
            replay: Vec::new(),
            choked: true,
            last_recv: now,
            last_sent: now,
        };
        if reserved.extension_protocol() {
            out.push(Effect::Send {
                peer: key,
                message: ext::our_handshake(None, self.config.pex),
            });
        }
        self.peers.insert(key, peer);
    }

    fn remove_peer(&mut self, key: PeerKey) -> Option<SocketAddr> {
        let peer = self.peers.remove(&key)?;
        for piece in &mut self.pieces {
            if matches!(piece, Piece::Requested { peer, .. } if *peer == key) {
                *piece = Piece::Missing;
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
                            peer.rejected = true;
                            self.release(key, piece);
                        }
                        Ok(MetadataMessage::Request { piece }) => {
                            // We have nothing to serve yet.
                            if let Some(id) = peer.ext.ut_metadata {
                                if let Ok(payload) = (MetadataMessage::Reject { piece }).encode() {
                                    peer.last_sent = now;
                                    out.push(Effect::Send {
                                        peer: key,
                                        message: Message::Extended { id, payload },
                                    });
                                }
                            }
                        }
                        _ => {}
                    }
                } else if id == ext::OUR_UT_PEX && self.config.pex {
                    if let Some(added) = ext::pex_added(&payload) {
                        self.add_to_pool(added);
                        self.fill_connections(out);
                    }
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
        if size == 0 || size > self.config.max_metadata_size {
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
        self.pieces = vec![Piece::Missing; count];
    }

    fn release(&mut self, key: PeerKey, index: u32) {
        let slot = usize::try_from(index)
            .ok()
            .and_then(|i| self.pieces.get_mut(i));
        if let Some(piece) = slot {
            if matches!(piece, Piece::Requested { peer, .. } if *peer == key) {
                *piece = Piece::Missing;
            }
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
        if *piece == Piece::Have || dst.len() != data.len() {
            return;
        }
        dst.copy_from_slice(data);
        *piece = Piece::Have;
        self.sources.insert(key);
        self.downloaded = self
            .downloaded
            .saturating_add(u64::try_from(data.len()).unwrap_or(0));
        if self.pieces.iter().all(|p| *p == Piece::Have) {
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
        // contributed and start over; they are not dialed again.
        let mut bad: Vec<PeerKey> = self.sources.drain().collect();
        bad.sort_unstable();
        for key in bad {
            self.disconnect(key, out);
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
            if peer.rejected || peer.ext.metadata_size != size {
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
                if *piece != Piece::Missing {
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
        // Timed-out requests go to another peer.
        let timeout = self.config.request_timeout;
        for piece in &mut self.pieces {
            if let Piece::Requested { since, .. } = *piece {
                if now.saturating_duration_since(since) >= timeout {
                    *piece = Piece::Missing;
                }
            }
        }
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
            self.announce(now, AnnounceEvent::None, out);
        }
        self.fill_connections(out);
        out.push(Effect::Alert(Alert::Stats(self.stats())));
    }
}
