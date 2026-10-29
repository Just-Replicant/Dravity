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
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use bytes::{Bytes, BytesMut};
use dravity_core::{BlockRef, InfoHash, Metainfo, PeerId};
use dravity_dht::Dht;
use dravity_engine::{
    Alert, AnnounceEvent, Bitfield, Config, Effect, Event, MetadataState, PeerKey, Priority, Stats,
    TorrentState, TorrentStatus,
};
use dravity_storage::{
    FileStorage, ResumeData, Storage, load_resume, read_block, save_resume, verify_piece,
    write_block,
};
use dravity_tracker::{AnnounceRequest, AnnounceResponse, TrackerClient};
use dravity_wire::{Message, Reserved};
use futures_util::StreamExt;
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot};
use tokio::task::JoinSet;

use crate::SessionAlert;
use crate::catalog::{Catalog, EntrySource};
use crate::config::SessionConfig;
use crate::files::{self, FileProgress};
use crate::listen::UtpSockets;
use crate::peer::{self, PeerCommand, PeerParams};
use crate::persist::Persister;
use crate::stream::PeerStream;

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
    },
    ConnectFailed {
        key: PeerKey,
        addr: SocketAddr,
        retry: bool,
    },
    Disconnected {
        key: PeerKey,
    },
    Message {
        key: PeerKey,
        message: Message,
    },
    Engine(Event),
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

/// Commands from [`TorrentHandle`](crate::TorrentHandle), streams and the
/// listener.
#[derive(Debug)]
pub(crate) enum Command {
    Stats(oneshot::Sender<Stats>),
    Progress(oneshot::Sender<TorrentProgress>),
    AddPeers(Vec<SocketAddr>),
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
    /// A stream's urgent window, replacing its previous one.
    Stream {
        id: u64,
        pieces: Vec<u32>,
    },
    CloseStream(u64),
    Stop(oneshot::Sender<()>),
}

/// Shared, immutable inputs of a torrent task.
#[derive(Debug, Clone)]
pub(crate) struct TorrentContext {
    pub(crate) info_hash: InfoHash,
    pub(crate) trackers: Tiers,
    pub(crate) tracker: TrackerClient,
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
    /// Session state writer, with a state directory.
    pub(crate) persist: Option<Persister>,
    /// Router port mapping status, when mapping runs.
    pub(crate) port_map: Option<tokio::sync::watch::Receiver<dravity_portmap::Status>>,
}

impl TorrentContext {
    /// Engine settings for this torrent: DHT only if the session runs one,
    /// and neither DHT nor PEX for private torrents (BEP 27).
    fn engine_config(&self, private: bool) -> Config {
        let mut config = self.config.engine.clone();
        config.dht = self.dht.is_some() && !private;
        config.pex = config.pex && !private;
        config.listen_port = self.listen_port;
        config
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

    /// Reserved bits of our handshake.
    pub(crate) fn reserved(&self) -> Reserved {
        peer::our_reserved(self.config.engine.fast_extension)
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
    /// A magnet link: fetch the metadata first, then download into `dir`.
    Magnet { download_dir: PathBuf },
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
    /// Download directory of a magnet link, used once metadata arrives.
    download_dir: Option<PathBuf>,
    /// Verified `info` dictionary waiting to replace the metadata phase.
    metadata: Option<Bytes>,
    peers: HashMap<PeerKey, mpsc::Sender<PeerCommand>>,
    next_key: u64,
    events_tx: mpsc::Sender<Input>,
    events_rx: mpsc::Receiver<Input>,
    commands: mpsc::Receiver<Command>,
    /// Events produced synchronously during dispatch, handled next.
    local: VecDeque<Event>,
    hash_slots: Arc<Semaphore>,
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
        let (phase, content, download_dir) = match source {
            Source::Torrent { metainfo, storage } => {
                let config = ctx.engine_config(metainfo.info().is_private());
                file_priorities = files::normalize(metainfo.info(), &file_priorities)
                    .unwrap_or_else(|| vec![Priority::Normal; metainfo.info().files().len()]);
                let pieces = files::piece_priorities(metainfo.info(), &file_priorities);
                let state = TorrentState::new(*metainfo.info().layout(), config)
                    .with_info_hash(ctx.info_hash)
                    .with_priorities(&pieces);
                (
                    Phase::Download(Box::new(state)),
                    Some(Content { metainfo, storage }),
                    None,
                )
            }
            Source::Magnet { download_dir } => {
                let state = MetadataState::new(ctx.info_hash, ctx.engine_config(false));
                (Phase::Metadata(Box::new(state)), None, Some(download_dir))
            }
        };
        let (events_tx, events_rx) = mpsc::channel(ctx.config.event_queue.max(1));
        let hash_slots = Arc::new(Semaphore::new(ctx.config.hash_workers.max(1)));
        Self {
            ctx,
            phase,
            content,
            download_dir,
            metadata: None,
            peers: HashMap::new(),
            next_key: 0,
            events_tx,
            events_rx,
            commands,
            local: VecDeque::new(),
            hash_slots,
            background: JoinSet::new(),
            disk: JoinSet::new(),
            file_priorities,
            waiters: HashMap::new(),
            streams: BTreeMap::new(),
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
            }
        }
        let mut tick = tokio::time::interval(self.ctx.config.tick_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut stop_ack: Option<oneshot::Sender<()>> = None;
        self.handle(Event::Start).await;
        while self.phase.status() != TorrentStatus::Stopped {
            if let Some(event) = self.local.pop_front() {
                self.handle(event).await;
                continue;
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
                _ = tick.tick() => Event::Tick,
                // Reap finished background work so the set does not grow.
                Some(_) = self.background.join_next(), if !self.background.is_empty() => continue,
            };
            self.handle(event).await;
        }
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
            .with_info_hash(self.ctx.info_hash)
            .with_priorities(&self.piece_priorities(content));
        Some(match info {
            Some(info) => state.with_info_bytes(info.clone()),
            None => state,
        })
    }

    /// Maps peer-task inputs to engine events, keeping the peer map in sync.
    fn translate(&mut self, input: Input) -> Option<Event> {
        Some(match input {
            Input::Connected {
                key,
                addr,
                reserved,
            } => {
                if !self.peers.contains_key(&key) {
                    return None;
                }
                Event::PeerConnected {
                    peer: key,
                    addr,
                    reserved,
                }
            }
            Input::ConnectFailed { key, addr, retry } => {
                self.peers.remove(&key);
                Event::PeerConnectFailed { addr, retry }
            }
            Input::Disconnected { key } => {
                self.peers.remove(&key)?;
                Event::PeerDisconnected { peer: key }
            }
            Input::Message { key, message } => {
                if !self.peers.contains_key(&key) {
                    return None;
                }
                Event::Message { peer: key, message }
            }
            Input::Engine(event) => event,
        })
    }

    async fn handle(&mut self, event: Event) {
        let effects = self.phase.update(Instant::now(), event);
        for effect in effects {
            self.dispatch(effect).await;
        }
        if let Some(info) = self.metadata.take() {
            self.start_download(info).await;
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

    /// Switches a magnet link from the metadata phase to downloading.
    async fn start_download(&mut self, info: Bytes) {
        let Phase::Metadata(_) = self.phase else {
            return;
        };
        let Some(dir) = self.download_dir.take() else {
            return self.fail("magnet is missing a download directory".to_owned());
        };
        let tiers = self
            .ctx
            .trackers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let metainfo = match Metainfo::from_info_bytes(&info, tiers) {
            Ok(metainfo) => Arc::new(metainfo),
            Err(e) => return self.fail(format!("invalid metadata: {e}")),
        };
        let storage: Arc<dyn Storage> = match FileStorage::new(&dir, metainfo.info()) {
            Ok(storage) => Arc::new(storage),
            Err(e) => return self.fail(format!("setting up storage: {e}")),
        };
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
            .with_info_hash(self.ctx.info_hash)
            .with_info_bytes(info)
            .with_priorities(&self.piece_priorities(&content)),
        };
        self.content = Some(content);
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
            .with_connecting(handover.connecting.iter().copied())
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
                }
            } else {
                Event::PeerConnected {
                    peer: peer.peer,
                    addr: peer.addr,
                    reserved: peer.reserved,
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

    /// A magnet's metadata arrived: the catalog (and the state directory)
    /// now hold the torrent, so a resume or restart does not fetch it again.
    fn record_metadata(&self, metainfo: &Arc<Metainfo>) {
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
            persist.save_torrent(metainfo);
            persist.save_record(&record);
        }
    }

    async fn dispatch(&mut self, effect: Effect) {
        match effect {
            Effect::Connect { addr } => self.connect(addr),
            Effect::Send { peer, message } => {
                let Some(tx) = self.peers.get(&peer) else {
                    return;
                };
                if tx.try_send(PeerCommand::Send(message)).is_err() {
                    // Full (too slow) or closed: drop the peer.
                    self.peers.remove(&peer);
                    self.local.push_back(Event::PeerDisconnected { peer });
                }
            }
            Effect::Disconnect { peer } => {
                self.peers.remove(&peer);
            }
            Effect::WriteBlock { block, data } => self.write(block, data),
            Effect::HashPiece { piece } => self.hash(piece),
            Effect::ReadBlock { peer, block } => self.read(peer, block),
            Effect::Announce {
                event,
                uploaded,
                downloaded,
                left,
            } => {
                let req = self.announce_request(event, uploaded, downloaded, left);
                if event == AnnounceEvent::Stopped {
                    // The task is about to exit: announce inline, bounded.
                    let fut = announce_all(self.ctx.clone(), req);
                    let _ = tokio::time::timeout(self.ctx.config.stop_announce_timeout, fut).await;
                } else {
                    let ctx = self.ctx.clone();
                    let tx = self.events_tx.clone();
                    self.background.spawn(async move {
                        let event = announce_all(ctx, req).await;
                        let _ = tx.send(Input::Engine(event)).await;
                    });
                }
            }
            Effect::DhtLookup => self.dht_lookup(),
            Effect::MetadataReceived { info } => self.metadata = Some(info),
            Effect::Alert(alert) => self.alert(alert),
            other => tracing::warn!(?other, "unhandled engine effect"),
        }
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
        let info_hash = self.ctx.info_hash;
        let timeout = self.ctx.config.dht_lookup_timeout;
        let listen_port = self.ctx.listen_port.map(|_| self.ctx.announce_port());
        let tx = self.events_tx.clone();
        self.background.spawn(async move {
            let (peers, failed) = lookup_peers(&dht, info_hash, timeout).await;
            tracing::debug!(peers = peers.len(), failed, "DHT lookup done");
            let _ = tx
                .send(Input::Engine(Event::DhtPeers { peers, failed }))
                .await;
            // Tell the DHT where to find us, so peers can connect: only
            // after a lookup that reached nodes, bounded by the same timeout.
            if let (Some(port), false) = (listen_port, failed) {
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
        } = accepted;
        let (key, rx) = self.new_peer();
        tokio::spawn(peer::run_accepted(
            key,
            addr,
            stream,
            leftover,
            self.events_tx.clone(),
            rx,
        ));
        Event::PeerAccepted {
            peer: key,
            addr,
            reserved,
        }
    }

    fn connect(&mut self, addr: SocketAddr) {
        let (key, rx) = self.new_peer();
        let params = PeerParams {
            key,
            addr,
            info_hash: self.ctx.info_hash,
            our_id: self.ctx.peer_id,
            reserved: self.ctx.reserved(),
            connect_timeout: self.ctx.config.connect_timeout,
            utp: self.ctx.utp.for_addr(addr).cloned(),
            prefer_utp: self.ctx.config.prefer_utp,
        };
        // Peer tasks are not in `background`: dropping their command sender
        // ends them, and they must not delay shutdown.
        tokio::spawn(peer::run(params, self.events_tx.clone(), rx));
    }

    fn write(&mut self, block: BlockRef, data: Bytes) {
        let Some(content) = self.content.clone() else {
            tracing::warn!(?block, "block write before metadata");
            return;
        };
        let tx = self.events_tx.clone();
        self.disk.spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                write_block(
                    content.storage.as_ref(),
                    content.metainfo.info().layout(),
                    block,
                    &data,
                )
            })
            .await;
            let event = match result {
                Ok(Ok(())) => Event::BlockWritten { block },
                Ok(Err(e)) => Event::WriteFailed {
                    block,
                    error: e.to_string(),
                },
                Err(e) => Event::WriteFailed {
                    block,
                    error: e.to_string(),
                },
            };
            let _ = tx.send(Input::Engine(event)).await;
        });
    }

    /// Reads a block a peer asked for. Reads share the disk set with writes
    /// so a stop waits for them too.
    fn read(&mut self, peer: PeerKey, block: BlockRef) {
        let Some(content) = self.content.clone() else {
            tracing::warn!(?block, "block read before metadata");
            return;
        };
        let tx = self.events_tx.clone();
        self.disk.spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                read_block(
                    content.storage.as_ref(),
                    content.metainfo.info().layout(),
                    block,
                )
            })
            .await;
            let event = match result {
                Ok(Ok(data)) => Event::BlockRead {
                    peer,
                    block,
                    data: Bytes::from(data),
                },
                Ok(Err(e)) => Event::ReadFailed {
                    peer,
                    block,
                    error: e.to_string(),
                },
                Err(e) => Event::ReadFailed {
                    peer,
                    block,
                    error: e.to_string(),
                },
            };
            let _ = tx.send(Input::Engine(event)).await;
        });
    }

    fn hash(&mut self, piece: u32) {
        let Some(content) = self.content.clone() else {
            tracing::warn!(piece, "hash request before metadata");
            return;
        };
        let slots = Arc::clone(&self.hash_slots);
        let tx = self.events_tx.clone();
        self.disk.spawn(async move {
            let Ok(_permit) = slots.acquire_owned().await else {
                return;
            };
            let result = tokio::task::spawn_blocking(move || {
                verify_piece(content.storage.as_ref(), content.metainfo.info(), piece)
            })
            .await;
            // A mismatch or a short file is "we don't have it". Any other I/O
            // error would otherwise reset the piece and download it forever.
            let event = match result {
                Ok(Ok(ok)) => Event::PieceHashed { piece, ok },
                Ok(Err(e)) if e.is_missing_data() => Event::PieceHashed { piece, ok: false },
                Ok(Err(e)) => {
                    tracing::warn!(piece, error = %e, "piece hash failed");
                    Event::StorageFailed {
                        error: format!("hashing piece {piece}: {e}"),
                    }
                }
                Err(e) => {
                    tracing::warn!(piece, error = %e, "piece hash task failed");
                    Event::StorageFailed {
                        error: format!("hashing piece {piece}: {e}"),
                    }
                }
            };
            let _ = tx.send(Input::Engine(event)).await;
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
/// Success if any tier got an answer; the results are merged.
async fn announce_all(ctx: TorrentContext, req: AnnounceRequest) -> Event {
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
        return Event::Announced {
            peers: Vec::new(),
            interval: None,
        };
    }
    let mut set = JoinSet::new();
    for tier in tiers {
        set.spawn(announce_tier(
            ctx.tracker.clone(),
            Arc::clone(&ctx.trackers),
            tier,
            req.clone(),
        ));
    }
    let mut merge = AnnounceMerge::default();
    let mut errors = Vec::new();
    let mut any_ok = false;
    while let Some(joined) = set.join_next().await {
        let Ok((answer, tier_errors)) = joined else {
            continue;
        };
        errors.extend(tier_errors);
        if let Some((url, response)) = answer {
            any_ok = true;
            merge.observe(&url, response);
        }
    }
    if any_ok {
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
    }
}

/// Tries the trackers of one tier in order; returns the first answer and
/// the errors of those that failed before it.
async fn announce_tier(
    client: TrackerClient,
    tiers: Tiers,
    tier: Vec<String>,
    req: AnnounceRequest,
) -> (Option<(String, AnnounceResponse)>, Vec<String>) {
    let mut errors = Vec::new();
    for url in tier {
        match client.announce(&url, &req).await {
            Ok(response) => {
                tracing::debug!(%url, peers = response.peers.len(), "announce ok");
                promote(&tiers, &url);
                return (Some((url, response)), errors);
            }
            Err(e) => {
                let e = error_chain(&e);
                tracing::debug!(%url, error = %e, "announce failed");
                errors.push(format!("{url}: {e}"));
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

/// Formats an error with its `source()` chain (`outer: inner: root`).
/// Port to announce: the router's mapped port, the listen port, or the
/// configured fallback, in that order.
fn announce_port(mapped: Option<u16>, listen: Option<u16>, configured: u16) -> u16 {
    mapped.or(listen).unwrap_or(configured)
}

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

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::time::Duration;

    use dravity_tracker::AnnounceResponse;

    use super::{
        AnnounceMerge, announce_port, announce_tier, dht_lookup_failed, error_chain, prepare_tiers,
        promote,
    };

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
        };
        let (answer, errors) =
            announce_tier(client, Arc::clone(&tiers), vec![dead, good.clone()], req).await;
        assert_eq!(answer.expect("fallback").0, good);
        assert!(!errors.is_empty(), "the dead tracker was tried");
        assert_eq!(tiers.lock().unwrap()[0][0], good);
    }
}
