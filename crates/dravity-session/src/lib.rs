//! The Dravity session: the only production I/O driver.
//!
//! A [`Session`] owns one Tokio task per torrent. That task is the sole
//! caller of [`dravity_engine::TorrentState::update`]: it polls peers, disk,
//! hashing, trackers and a timer, feeds the results to the engine as events,
//! and dispatches the engine's effects. Applications never write that loop;
//! they add torrents, send commands through a [`TorrentHandle`], and read
//! [`SessionAlert`]s.
//!
//! ```no_run
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! use dravity_core::Metainfo;
//! use dravity_engine::Alert;
//! use dravity_session::{AddTorrentOptions, Session, SessionConfig};
//!
//! let session = Session::new(SessionConfig::default())?;
//! let mut alerts = session.subscribe();
//! let metainfo = Metainfo::from_bytes(&std::fs::read("example.torrent")?)?;
//! let torrent = session.add_torrent(metainfo, AddTorrentOptions::new("downloads"))?;
//! while let Ok(alert) = alerts.recv().await {
//!     if alert.info_hash == torrent.info_hash() && alert.alert == Alert::Finished {
//!         break;
//!     }
//! }
//! torrent.stop().await;
//! # Ok(()) }
//! ```
//!
//! Torrents come from `.torrent` files ([`Session::add_torrent`]) or magnet
//! links ([`Session::add_magnet`], metadata fetched from peers with BEP 9).
//! Peers are found through trackers (BEP 12 tiers), the DHT (BEP 5), peer
//! exchange (BEP 11) and explicit addresses. With
//! [`SessionConfig::resume_dir`] set, restarts skip the full hash check.
//!
//! The session downloads only: it does not yet listen for incoming
//! connections or upload (Phase 3).

mod codec;
mod config;
mod error;
mod peer;
mod torrent;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use dravity_core::{InfoHash, Magnet, Metainfo, PeerId};
use dravity_dht::Dht;
use dravity_engine::{Alert, Stats};
use dravity_storage::{FileStorage, Storage};
use dravity_tracker::TrackerClient;
use tokio::sync::{broadcast, mpsc, oneshot};

pub use codec::{CodecError, PeerCodec};
pub use config::SessionConfig;
pub use error::SessionError;

use crate::torrent::{Command, Source, TorrentContext, TorrentTask, prepare_tiers};

/// An engine [`Alert`] tagged with the torrent it concerns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionAlert {
    /// The torrent.
    pub info_hash: InfoHash,
    /// What happened.
    pub alert: Alert,
}

/// Where and how to add a torrent.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AddTorrentOptions {
    /// Download directory. Multi-file torrents get a sub-directory named
    /// after the torrent.
    pub download_dir: PathBuf,
    /// Peers to try in addition to those from trackers.
    pub peers: Vec<SocketAddr>,
    /// Custom storage backend; overrides `download_dir`. Not supported for
    /// magnet links.
    pub storage: Option<Arc<dyn Storage>>,
}

impl AddTorrentOptions {
    /// Download into `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            download_dir: dir.into(),
            peers: Vec::new(),
            storage: None,
        }
    }

    /// Adds peers to try in addition to tracker peers.
    #[must_use]
    pub fn with_peers(mut self, peers: impl IntoIterator<Item = SocketAddr>) -> Self {
        self.peers.extend(peers);
        self
    }

    /// Uses a custom storage backend.
    #[must_use]
    pub fn with_storage(mut self, storage: Arc<dyn Storage>) -> Self {
        self.storage = Some(storage);
        self
    }
}

#[derive(Debug)]
struct Inner {
    config: Arc<SessionConfig>,
    peer_id: PeerId,
    tracker: TrackerClient,
    dht: Option<Dht>,
    alerts: broadcast::Sender<SessionAlert>,
    torrents: Mutex<HashMap<InfoHash, mpsc::Sender<Command>>>,
}

/// A BitTorrent session. Cheap to clone; all clones share the torrents.
#[derive(Debug, Clone)]
pub struct Session {
    inner: Arc<Inner>,
}

impl Session {
    /// Creates a session with a random peer id.
    ///
    /// Starts the DHT node when [`SessionConfig::dht`] is set.
    ///
    /// # Errors
    /// [`SessionError::Tracker`] if the tracker HTTP/TLS client cannot be
    /// built; [`SessionError::Dht`] if the DHT socket cannot be bound.
    pub fn new(config: SessionConfig) -> Result<Self, SessionError> {
        let suffix: Vec<u8> = std::iter::repeat_with(fastrand::alphanumeric)
            .take(PeerId::LEN)
            .map(|c| u8::try_from(c).unwrap_or(b'x'))
            .collect();
        let peer_id = PeerId::with_prefix(&config.peer_id_prefix, &suffix);
        let tracker = TrackerClient::new(config.tracker.clone())?;
        let dht = config.dht.as_ref().map(Dht::new).transpose()?;
        let (alerts, _) = broadcast::channel(config.alert_buffer.max(1));
        Ok(Self {
            inner: Arc::new(Inner {
                config: Arc::new(config),
                peer_id,
                tracker,
                dht,
                alerts,
                torrents: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// Our peer id.
    #[must_use]
    pub fn peer_id(&self) -> PeerId {
        self.inner.peer_id
    }

    /// Subscribes to alerts from every torrent. Subscribe before adding a
    /// torrent to see its first alerts.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<SessionAlert> {
        self.inner.alerts.subscribe()
    }

    /// The session's DHT node, if it runs one.
    #[must_use]
    pub fn dht(&self) -> Option<&Dht> {
        self.inner.dht.as_ref()
    }

    /// Adds a torrent and starts it: existing data is checked (or resume
    /// data trusted), then trackers are announced to and pieces downloaded.
    ///
    /// Must be called from within a Tokio runtime.
    ///
    /// # Errors
    /// [`SessionError::Duplicate`] if the torrent is already running;
    /// [`SessionError::Storage`] if the storage cannot be set up.
    pub fn add_torrent(
        &self,
        metainfo: Metainfo,
        options: AddTorrentOptions,
    ) -> Result<TorrentHandle, SessionError> {
        let info_hash = metainfo.info_hash();
        let trackers = metainfo.trackers();
        self.spawn(info_hash, trackers, options.peers, || {
            let storage: Arc<dyn Storage> = match options.storage {
                Some(storage) => storage,
                None => Arc::new(FileStorage::new(&options.download_dir, metainfo.info())?),
            };
            Ok(Source::Torrent {
                metainfo: Arc::new(metainfo),
                storage,
            })
        })
    }

    /// Adds a magnet link and starts it: the `info` dictionary is fetched
    /// from peers (BEP 9) found through the magnet's trackers and peers and
    /// the DHT, verified against the info-hash, and then the torrent
    /// downloads like one added from a file.
    ///
    /// Must be called from within a Tokio runtime.
    ///
    /// # Errors
    /// [`SessionError::MagnetWithoutV1Hash`] for a v2-only magnet,
    /// [`SessionError::StorageForMagnet`] if `options` carries a custom
    /// storage backend, [`SessionError::Duplicate`] if the torrent is
    /// already running.
    pub fn add_magnet(
        &self,
        magnet: &Magnet,
        options: AddTorrentOptions,
    ) -> Result<TorrentHandle, SessionError> {
        let info_hash = magnet.info_hash.ok_or(SessionError::MagnetWithoutV1Hash)?;
        if options.storage.is_some() {
            return Err(SessionError::StorageForMagnet);
        }
        // Each `tr` is its own tier: all of them are asked concurrently.
        let trackers = magnet.trackers.iter().map(|t| vec![t.clone()]).collect();
        let mut peers = options.peers;
        peers.extend(
            magnet
                .peers
                .iter()
                .filter_map(|p| p.parse::<SocketAddr>().ok()),
        );
        let download_dir = options.download_dir;
        self.spawn(info_hash, trackers, peers, || {
            Ok(Source::Magnet { download_dir })
        })
    }

    /// Registers and spawns a torrent task; `source` runs under the torrent
    /// map lock, after the duplicate check.
    fn spawn(
        &self,
        info_hash: InfoHash,
        trackers: Vec<Vec<String>>,
        peers: Vec<SocketAddr>,
        source: impl FnOnce() -> Result<Source, SessionError>,
    ) -> Result<TorrentHandle, SessionError> {
        let mut torrents = self
            .inner
            .torrents
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if torrents.get(&info_hash).is_some_and(|tx| !tx.is_closed()) {
            return Err(SessionError::Duplicate(info_hash));
        }
        let source = source()?;
        let ctx = TorrentContext {
            info_hash,
            trackers: prepare_tiers(trackers),
            tracker: self.inner.tracker.clone(),
            dht: self.inner.dht.clone(),
            peer_id: self.inner.peer_id,
            config: Arc::clone(&self.inner.config),
            alerts: self.inner.alerts.clone(),
            tracker_key: fastrand::u32(..),
        };
        let (tx, rx) = mpsc::channel(64);
        if !peers.is_empty() {
            // Fresh channel with spare capacity: cannot fail.
            let _ = tx.try_send(Command::AddPeers(peers));
        }
        tokio::spawn(TorrentTask::new(ctx, source, rx).run());
        torrents.insert(info_hash, tx.clone());
        Ok(TorrentHandle {
            info_hash,
            commands: tx,
        })
    }

    /// A handle to a running torrent.
    #[must_use]
    pub fn torrent(&self, info_hash: InfoHash) -> Option<TorrentHandle> {
        let torrents = self
            .inner
            .torrents
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let commands = torrents
            .get(&info_hash)
            .filter(|tx| !tx.is_closed())?
            .clone();
        Some(TorrentHandle {
            info_hash,
            commands,
        })
    }

    /// Stops every torrent and waits for them to finish.
    pub async fn shutdown(&self) {
        let handles: Vec<TorrentHandle> = {
            let mut torrents = self
                .inner
                .torrents
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            torrents
                .drain()
                .map(|(info_hash, commands)| TorrentHandle {
                    info_hash,
                    commands,
                })
                .collect()
        };
        for handle in handles {
            handle.stop().await;
        }
    }
}

/// Controls one torrent in a [`Session`]. Cheap to clone.
#[derive(Debug, Clone)]
pub struct TorrentHandle {
    info_hash: InfoHash,
    commands: mpsc::Sender<Command>,
}

impl TorrentHandle {
    /// The torrent's info-hash.
    #[must_use]
    pub const fn info_hash(&self) -> InfoHash {
        self.info_hash
    }

    /// Whether the torrent task is still running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        !self.commands.is_closed()
    }

    /// A progress snapshot.
    ///
    /// # Errors
    /// [`SessionError::NotRunning`] once the torrent has stopped.
    pub async fn stats(&self) -> Result<Stats, SessionError> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::Stats(tx))
            .await
            .map_err(|_| SessionError::NotRunning(self.info_hash))?;
        rx.await
            .map_err(|_| SessionError::NotRunning(self.info_hash))
    }

    /// Adds peers to connect to.
    ///
    /// # Errors
    /// [`SessionError::NotRunning`] once the torrent has stopped.
    pub async fn add_peers(
        &self,
        peers: impl IntoIterator<Item = SocketAddr>,
    ) -> Result<(), SessionError> {
        self.commands
            .send(Command::AddPeers(peers.into_iter().collect()))
            .await
            .map_err(|_| SessionError::NotRunning(self.info_hash))
    }

    /// Stops the torrent: disconnects peers, announces `stopped` (bounded by
    /// [`SessionConfig::stop_announce_timeout`]), flushes storage, and waits
    /// for all of that. A no-op if it already stopped.
    pub async fn stop(&self) {
        let (tx, rx) = oneshot::channel();
        if self.commands.send(Command::Stop(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }
}
