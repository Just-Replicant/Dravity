//! The per-torrent owner task: the only caller of `TorrentState::update`.
//!
//! ```text
//! loop {
//!     input  = select(commands, peer/disk/hash/tracker events, timer)
//!     effects = state.update(now, input)
//!     dispatch(effects)        // queue work; never wait for it here
//! }
//! ```
//!
//! Dispatch never awaits a bounded channel the owner itself must drain:
//! peer queues use `try_send` (a full queue drops the slow peer) and disk,
//! hash and tracker work runs in spawned tasks that report back as events.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use dravity_core::{BlockRef, Metainfo, PeerId};
use dravity_engine::{
    Alert, AnnounceEvent, Effect, Event, PeerKey, Stats, TorrentState, TorrentStatus,
};
use dravity_storage::{Storage, verify_piece, write_block};
use dravity_tracker::{AnnounceRequest, AnnounceResponse, TrackerClient};
use dravity_wire::Message;
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot};
use tokio::task::JoinSet;

use crate::SessionAlert;
use crate::config::SessionConfig;
use crate::peer::{self, PeerCommand, PeerParams};

/// Everything that can wake the owner task besides commands and the timer.
#[derive(Debug)]
pub(crate) enum Input {
    Connected {
        key: PeerKey,
        addr: SocketAddr,
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
    pub(crate) metainfo: Arc<Metainfo>,
    pub(crate) storage: Arc<dyn Storage>,
    pub(crate) tracker: TrackerClient,
    pub(crate) peer_id: PeerId,
    pub(crate) config: Arc<SessionConfig>,
    pub(crate) alerts: broadcast::Sender<SessionAlert>,
    pub(crate) tracker_key: u32,
}

pub(crate) struct TorrentTask {
    ctx: TorrentContext,
    state: TorrentState,
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
            .field("info_hash", &self.ctx.metainfo.info_hash())
            .field("peers", &self.peers.len())
            .finish_non_exhaustive()
    }
}

impl TorrentTask {
    pub(crate) fn new(ctx: TorrentContext, commands: mpsc::Receiver<Command>) -> Self {
        let layout = *ctx.metainfo.info().layout();
        let state = TorrentState::new(layout, ctx.config.engine.clone());
        let (events_tx, events_rx) = mpsc::channel(ctx.config.event_queue.max(1));
        let hash_slots = Arc::new(Semaphore::new(ctx.config.hash_workers.max(1)));
        Self {
            ctx,
            state,
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

    /// Runs until the torrent stops, then flushes storage and acknowledges
    /// a pending stop command.
    pub(crate) async fn run(mut self) {
        let mut tick = tokio::time::interval(self.ctx.config.tick_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut stop_ack: Option<oneshot::Sender<()>> = None;
        self.handle(Event::Start).await;
        while self.state.status() != TorrentStatus::Stopped {
            if let Some(event) = self.local.pop_front() {
                self.handle(event).await;
                continue;
            }
            let event = tokio::select! {
                command = self.commands.recv() => match command {
                    Some(Command::Stats(reply)) => {
                        let _ = reply.send(self.state.stats());
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
        let storage = Arc::clone(&self.ctx.storage);
        if let Ok(Err(e)) = tokio::task::spawn_blocking(move || storage.flush()).await {
            tracing::warn!(error = %e, "flushing storage failed");
        }
        // Mark the handle as not running before acknowledging the stop.
        self.commands.close();
        if let Some(ack) = stop_ack {
            let _ = ack.send(());
        }
    }

    /// Maps peer-task inputs to engine events, keeping the peer map in sync.
    fn translate(&mut self, input: Input) -> Option<Event> {
        Some(match input {
            Input::Connected { key, addr } => {
                if !self.peers.contains_key(&key) {
                    return None;
                }
                Event::PeerConnected { peer: key, addr }
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
        let effects = self.state.update(Instant::now(), event);
        for effect in effects {
            self.dispatch(effect).await;
        }
    }

    fn alert(&self, alert: Alert) {
        let _ = self.ctx.alerts.send(SessionAlert {
            info_hash: self.ctx.metainfo.info_hash(),
            alert,
        });
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
            Effect::Alert(alert) => self.alert(alert),
            other => tracing::warn!(?other, "unhandled engine effect"),
        }
    }

    fn connect(&mut self, addr: SocketAddr) {
        let key = PeerKey(self.next_key);
        self.next_key += 1;
        let (tx, rx) = mpsc::channel(self.ctx.config.peer_queue.max(1));
        self.peers.insert(key, tx);
        let params = PeerParams {
            key,
            addr,
            info_hash: self.ctx.metainfo.info_hash(),
            our_id: self.ctx.peer_id,
            connect_timeout: self.ctx.config.connect_timeout,
        };
        // Peer tasks are not in `background`: dropping their command sender
        // ends them, and they must not delay shutdown.
        tokio::spawn(peer::run(params, self.events_tx.clone(), rx));
    }

    fn write(&mut self, block: BlockRef, data: bytes::Bytes) {
        let storage = Arc::clone(&self.ctx.storage);
        let meta = Arc::clone(&self.ctx.metainfo);
        let tx = self.events_tx.clone();
        self.background.spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                write_block(storage.as_ref(), meta.info().layout(), block, &data)
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
        let storage = Arc::clone(&self.ctx.storage);
        let meta = Arc::clone(&self.ctx.metainfo);
        let slots = Arc::clone(&self.hash_slots);
        let tx = self.events_tx.clone();
        self.background.spawn(async move {
            let Ok(_permit) = slots.acquire_owned().await else {
                return;
            };
            let result = tokio::task::spawn_blocking(move || {
                verify_piece(storage.as_ref(), meta.info(), piece)
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
            info_hash: self.ctx.metainfo.info_hash(),
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

/// Announces to every tracker of the torrent concurrently and merges the
/// results: success if any tracker answered.
async fn announce_all(ctx: TorrentContext, req: AnnounceRequest) -> Event {
    let urls: Vec<String> = ctx.metainfo.trackers().into_iter().flatten().collect();
    if urls.is_empty() {
        // Explicit peers can still download. Reporting a failure would retry
        // forever and surface a tracker error for a trackerless torrent.
        return Event::Announced {
            peers: Vec::new(),
            interval: None,
        };
    }
    let mut set = JoinSet::new();
    for url in urls {
        let client = ctx.tracker.clone();
        let req = req.clone();
        set.spawn(async move {
            let result = client.announce(&url, &req).await;
            (url, result)
        });
    }
    let mut merge = AnnounceMerge::default();
    let mut errors = Vec::new();
    let mut any_ok = false;
    while let Some(joined) = set.join_next().await {
        let Ok((url, result)) = joined else {
            continue;
        };
        match result {
            Ok(response) => {
                tracing::debug!(%url, peers = response.peers.len(), "announce ok");
                any_ok = true;
                merge.observe(&url, response);
            }
            Err(e) => {
                let e = error_chain(&e);
                tracing::debug!(%url, error = %e, "announce failed");
                errors.push(format!("{url}: {e}"));
            }
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

    use super::{AnnounceMerge, error_chain};

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
