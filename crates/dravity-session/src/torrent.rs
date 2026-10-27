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

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use bytes::Bytes;
use dravity_core::{BlockRef, InfoHash, Metainfo, PeerId};
use dravity_dht::Dht;
use dravity_engine::{
    Alert, AnnounceEvent, Bitfield, Config, Effect, Event, MetadataState, PeerKey, Stats,
    TorrentState, TorrentStatus,
};
use dravity_storage::{
    FileStorage, ResumeData, Storage, load_resume, save_resume, verify_piece, write_block,
};
use dravity_tracker::{AnnounceRequest, AnnounceResponse, TrackerClient};
use dravity_wire::Message;
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot};
use tokio::task::JoinSet;

use crate::SessionAlert;
use crate::config::SessionConfig;
use crate::peer::{self, PeerCommand, PeerParams};

/// Tracker tiers (BEP 12), shared with announce tasks so a tracker that
/// answered moves to the front of its tier.
pub(crate) type Tiers = Arc<Mutex<Vec<Vec<String>>>>;

/// Everything that can wake the owner task besides commands and the timer.
#[derive(Debug)]
pub(crate) enum Input {
    Connected {
        key: PeerKey,
        addr: SocketAddr,
        reserved: dravity_wire::Reserved,
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

/// Commands from [`TorrentHandle`](crate::TorrentHandle).
#[derive(Debug)]
pub(crate) enum Command {
    Stats(oneshot::Sender<Stats>),
    AddPeers(Vec<SocketAddr>),
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
}

impl TorrentContext {
    /// Engine settings for this torrent: DHT only if the session runs one,
    /// and neither DHT nor PEX for private torrents (BEP 27).
    fn engine_config(&self, private: bool) -> Config {
        let mut config = self.config.engine.clone();
        config.dht = self.dht.is_some() && !private;
        config.pex = config.pex && !private;
        config
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
    background: JoinSet<()>,
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
        commands: mpsc::Receiver<Command>,
    ) -> Self {
        let (phase, content, download_dir) = match source {
            Source::Torrent { metainfo, storage } => {
                let config = ctx.engine_config(metainfo.info().is_private());
                let state = TorrentState::new(*metainfo.info().layout(), config);
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
        }
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
                    Some(Command::Stats(reply)) => {
                        let _ = reply.send(self.phase.stats());
                        continue;
                    }
                    Some(Command::AddPeers(peers)) => Event::AddPeers { peers },
                    Some(Command::Stop(ack)) => {
                        stop_ack = Some(ack);
                        Event::Stop
                    }
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
        let state = TorrentState::with_have(*content.metainfo.info().layout(), have, config);
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
            return;
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
        let content = Content { metainfo, storage };
        let info_meta = content.metainfo.info();
        let state = match self.resumed_state(&content, Some(&info)).await {
            Some(state) => state,
            None => TorrentState::new(
                *info_meta.layout(),
                self.ctx.engine_config(info_meta.is_private()),
            )
            .with_info_bytes(info),
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
        let state = state.with_started_announced(handover.started_announced);
        self.phase = Phase::Download(Box::new(state));
        self.local.push_back(Event::Start);
        for peer in handover.peers {
            self.local.push_back(Event::PeerConnected {
                peer: peer.peer,
                addr: peer.addr,
                reserved: peer.reserved,
            });
            for message in peer.replay {
                self.local.push_back(Event::Message {
                    peer: peer.peer,
                    message,
                });
            }
        }
        if !handover.known.is_empty() {
            self.local.push_back(Event::AddPeers {
                peers: handover.known,
            });
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
            self.local.push_back(Event::DhtPeers { peers: Vec::new() });
            return;
        };
        let info_hash = self.ctx.info_hash;
        let timeout = self.ctx.config.dht_lookup_timeout;
        let tx = self.events_tx.clone();
        self.background.spawn(async move {
            let peers = tokio::time::timeout(timeout, dht.get_peers(info_hash))
                .await
                .unwrap_or_default();
            tracing::debug!(peers = peers.len(), "DHT lookup done");
            let _ = tx.send(Input::Engine(Event::DhtPeers { peers })).await;
        });
    }

    fn connect(&mut self, addr: SocketAddr) {
        let key = PeerKey(self.next_key);
        self.next_key += 1;
        let (tx, rx) = mpsc::channel(self.ctx.config.peer_queue.max(1));
        self.peers.insert(key, tx);
        let params = PeerParams {
            key,
            addr,
            info_hash: self.ctx.info_hash,
            our_id: self.ctx.peer_id,
            connect_timeout: self.ctx.config.connect_timeout,
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
        self.background.spawn(async move {
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

    fn hash(&mut self, piece: u32) {
        let Some(content) = self.content.clone() else {
            tracing::warn!(piece, "hash request before metadata");
            return;
        };
        let slots = Arc::clone(&self.hash_slots);
        let tx = self.events_tx.clone();
        self.background.spawn(async move {
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
            port: self.ctx.config.announce_port,
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

    use super::{AnnounceMerge, error_chain, prepare_tiers, promote};

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
}
