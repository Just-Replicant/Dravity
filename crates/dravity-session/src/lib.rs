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
//! With [`SessionConfig::listen`] the session accepts incoming connections
//! over TCP and uTP (BEP 29), IPv4 and IPv6, and routes them to torrents by
//! info-hash; outgoing connections use TCP with uTP as a fallback (or
//! first, with [`SessionConfig::prefer_utp`]); finished
//! torrents keep seeding until stopped (see the engine's choker and
//! [`dravity_engine::Config::seed`]).
//!
//! Product surface (Phase 4):
//!
//! * **File priorities**: [`AddTorrentOptions::with_file_priorities`] and
//!   [`Session::set_file_priorities`]; skipped files are not downloaded
//!   (pieces shared with a wanted file still are).
//! * **Streaming**: [`TorrentHandle::open_file`] returns a [`FileStream`]
//!   that reads at any position while the file downloads; the pieces ahead
//!   of the reader are fetched first.
//! * **Pause, resume, remove**: [`Session::pause`], [`Session::resume`],
//!   [`Session::remove`] (optionally deleting the files);
//!   [`Session::list`] shows every torrent, running or not.
//! * **Persistence**: with [`SessionConfig::state_dir`] the session writes
//!   what it holds ([`persist`]) and [`Session::restore`] brings it back.
//! * **Watch folder**: [`Session::watch`] adds `.torrent` and `.magnet`
//!   files dropped into a directory ([`watch`]).

mod catalog;
mod codec;
mod config;
mod error;
mod file_stream;
mod files;
mod listen;
mod peer;
pub mod persist;
mod stream;
mod torrent;
pub mod watch;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::Duration;

use dravity_core::{InfoHash, Magnet, Metainfo, PeerId};
use dravity_dht::Dht;
use dravity_engine::{Alert, Priority, Stats};
use dravity_portmap::{Mapping, PortMapper};
use dravity_storage::{FileStorage, Storage};
use dravity_tracker::TrackerClient;
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot};
use tokio_util::sync::{CancellationToken, DropGuard};

pub use codec::{CodecError, PeerCodec};
pub use config::SessionConfig;
pub use dravity_portmap::{
    Mapped as PortMapped, Method as PortMapMethod, PortMapConfig, Protocol as PortMapProtocol,
    Status as PortMapStatus,
};
pub use error::SessionError;
pub use file_stream::{DEFAULT_READAHEAD, FileStream};
pub use files::FileProgress;
pub use listen::ListenConfig;
pub use torrent::TorrentProgress;
pub use watch::{WatchConfig, WatchHandle};

use crate::catalog::{Catalog, Entry, EntrySource};
use crate::listen::{Acceptor, Bound, Registry, UtpSockets};
use crate::persist::{Persister, TorrentRecord};
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
    /// One priority per file (empty: all [`Priority::Normal`]). For a
    /// magnet link they apply once the metadata arrives, if the file count
    /// matches.
    pub file_priorities: Vec<Priority>,
    /// Add without starting; [`Session::resume`] starts it.
    pub paused: bool,
}

impl AddTorrentOptions {
    /// Download into `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            download_dir: dir.into(),
            peers: Vec::new(),
            storage: None,
            file_priorities: Vec::new(),
            paused: false,
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

    /// Sets one priority per file.
    #[must_use]
    pub fn with_file_priorities(mut self, priorities: impl IntoIterator<Item = Priority>) -> Self {
        self.file_priorities = priorities.into_iter().collect();
        self
    }

    /// Adds the torrent paused.
    #[must_use]
    pub const fn paused(mut self, paused: bool) -> Self {
        self.paused = paused;
        self
    }
}

/// A torrent of the session, running or not ([`Session::list`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TorrentSummary {
    /// The torrent.
    pub info_hash: InfoHash,
    /// Its name, or a magnet's display name.
    pub name: Option<String>,
    /// Download directory.
    pub download_dir: PathBuf,
    /// Paused by [`Session::pause`] (or added paused).
    pub paused: bool,
    /// A task is running. Neither running nor paused means it stopped after
    /// an error; [`Session::resume`] restarts it.
    pub running: bool,
    /// The metadata is known (always, except for magnet links still
    /// fetching it).
    pub has_metadata: bool,
    /// File priorities (empty: all normal).
    pub file_priorities: Vec<Priority>,
    /// When it was added, in seconds since the Unix epoch.
    pub added_at: u64,
}

/// What [`Session::restore`] found in the state directory.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct RestoreReport {
    /// Torrents added back (running or paused, as they were).
    pub restored: Vec<InfoHash>,
    /// State files that could not be used, and why.
    pub failed: Vec<(PathBuf, String)>,
}

#[derive(Debug)]
struct Inner {
    config: Arc<SessionConfig>,
    peer_id: PeerId,
    tracker: TrackerClient,
    dht: Option<Dht>,
    alerts: broadcast::Sender<SessionAlert>,
    /// Running torrents, by info-hash, for the listener.
    torrents: Registry,
    /// Every torrent, running or paused.
    catalog: Catalog,
    /// Session state writer, started with the first torrent.
    persist: OnceLock<Persister>,
    /// Bound sockets, until the first torrent starts the accept tasks
    /// (which need a Tokio runtime).
    listener: Mutex<Option<Bound>>,
    listen_port: Option<u16>,
    /// uTP sockets, once the listener started.
    utp: OnceLock<UtpSockets>,
    /// Router port mapping, once the listener started.
    port_mapper: Mutex<Option<PortMapper>>,
    /// Ends the accept tasks when the last session clone is dropped.
    _stop: DropGuard,
    stop: CancellationToken,
}

impl Inner {
    fn catalog(&self) -> std::sync::MutexGuard<'_, HashMap<InfoHash, Entry>> {
        self.catalog.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The state writer, if the session persists (started on first use;
    /// needs a Tokio runtime).
    fn persister(&self) -> Option<&Persister> {
        let dir = self.config.state_dir.as_ref()?;
        Some(self.persist.get_or_init(|| Persister::start(dir.clone())))
    }

    /// Where a torrent's fastresume data lives, if resume is enabled.
    fn resume_path(&self, info_hash: &InfoHash) -> Option<PathBuf> {
        self.config
            .resume_dir
            .as_ref()
            .map(|dir| dir.join(format!("{}.resume", info_hash.to_hex())))
    }
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
    /// Binds the listening sockets of [`SessionConfig::listen`]; they accept
    /// once the first torrent is added.
    ///
    /// # Errors
    /// [`SessionError::Tracker`] if the tracker HTTP/TLS client cannot be
    /// built; [`SessionError::Listen`] if no listening socket can be bound;
    /// [`SessionError::Dht`] if the DHT socket cannot be bound.
    pub fn new(config: SessionConfig) -> Result<Self, SessionError> {
        let suffix: Vec<u8> = std::iter::repeat_with(fastrand::alphanumeric)
            .take(PeerId::LEN)
            .map(|c| u8::try_from(c).unwrap_or(b'x'))
            .collect();
        let peer_id = PeerId::with_prefix(&config.peer_id_prefix, &suffix);
        let tracker = TrackerClient::new(config.tracker.clone())?;
        let listener = config
            .listen
            .as_ref()
            .map(listen::bind)
            .transpose()
            .map_err(SessionError::Listen)?;
        let listen_port = listener.as_ref().map(|bound| bound.port);
        let dht = config.dht.as_ref().map(Dht::new).transpose()?;
        let (alerts, _) = broadcast::channel(config.alert_buffer.max(1));
        let stop = CancellationToken::new();
        Ok(Self {
            inner: Arc::new(Inner {
                config: Arc::new(config),
                peer_id,
                tracker,
                dht,
                alerts,
                torrents: Arc::new(Mutex::new(HashMap::new())),
                catalog: Arc::new(Mutex::new(HashMap::new())),
                persist: OnceLock::new(),
                listener: Mutex::new(listener),
                listen_port,
                utp: OnceLock::new(),
                port_mapper: Mutex::new(None),
                _stop: stop.clone().drop_guard(),
                stop,
            }),
        })
    }

    /// The session configuration.
    #[must_use]
    pub fn config(&self) -> &SessionConfig {
        &self.inner.config
    }

    /// The TCP port the session accepts peers on, if it listens.
    #[must_use]
    pub fn listen_port(&self) -> Option<u16> {
        self.inner.listen_port
    }

    /// Starts accepting on the bound sockets (once; needs the runtime that
    /// `add_torrent` runs in).
    fn start_listening(&self) {
        let Some(bound) = self
            .inner
            .listener
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        else {
            return;
        };
        let config = &self.inner.config;
        let acceptor = Acceptor {
            registry: Arc::clone(&self.inner.torrents),
            our_id: self.inner.peer_id,
            reserved: peer::our_reserved(config.engine.fast_extension),
            handshake_timeout: config.connect_timeout.max(Duration::from_secs(1)),
            pending: Arc::new(Semaphore::new(
                config.listen.as_ref().map_or(1, |l| l.max_pending.max(1)),
            )),
        };
        match listen::start(bound, &acceptor, &self.inner.stop) {
            Ok(utp) => {
                let _ = self.inner.utp.set(utp);
            }
            Err(e) => {
                tracing::warn!(error = %e, "cannot accept peers");
                return;
            }
        }
        if let (Some(mapping), Some(port)) = (&config.port_mapping, self.inner.listen_port) {
            let mut mappings = vec![Mapping {
                protocol: dravity_portmap::Protocol::Tcp,
                port,
            }];
            if config.listen.as_ref().is_some_and(|l| l.utp.is_some()) {
                mappings.push(Mapping {
                    protocol: dravity_portmap::Protocol::Udp,
                    port,
                });
            }
            *self
                .inner
                .port_mapper
                .lock()
                .unwrap_or_else(PoisonError::into_inner) =
                Some(PortMapper::start(mapping.clone(), mappings));
        }
    }

    /// What router port mapping achieved so far; `None` when it is off or
    /// has not started (it starts with the first torrent).
    #[must_use]
    pub fn port_mapping(&self) -> Option<PortMapStatus> {
        self.inner
            .port_mapper
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(PortMapper::status)
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

    /// Adds a torrent and starts it (unless [`AddTorrentOptions::paused`]):
    /// existing data is checked (or resume data trusted), then trackers are
    /// announced to and pieces downloaded.
    ///
    /// Must be called from within a Tokio runtime. A paused torrent's handle
    /// is not running until [`Session::resume`].
    ///
    /// # Errors
    /// [`SessionError::Duplicate`] if the torrent is already in the session;
    /// [`SessionError::FileCount`] if the file priorities do not match;
    /// [`SessionError::Storage`] if the storage cannot be set up.
    pub fn add_torrent(
        &self,
        metainfo: Metainfo,
        options: AddTorrentOptions,
    ) -> Result<TorrentHandle, SessionError> {
        let metainfo = Arc::new(metainfo);
        let priorities = &options.file_priorities;
        if !priorities.is_empty() && priorities.len() != metainfo.info().files().len() {
            return Err(SessionError::FileCount {
                expected: metainfo.info().files().len(),
                got: priorities.len(),
            });
        }
        self.add_entry(
            metainfo.info_hash(),
            entry(EntrySource::Torrent(metainfo), options),
            true,
        )
    }

    /// Adds a magnet link and starts it (unless paused): the `info`
    /// dictionary is fetched from peers (BEP 9) found through the magnet's
    /// trackers and peers and the DHT, verified against the info-hash, and
    /// then the torrent downloads like one added from a file.
    ///
    /// Must be called from within a Tokio runtime.
    ///
    /// # Errors
    /// [`SessionError::MagnetWithoutV1Hash`] for a v2-only magnet,
    /// [`SessionError::StorageForMagnet`] if `options` carries a custom
    /// storage backend, [`SessionError::Duplicate`] if the torrent is
    /// already in the session.
    pub fn add_magnet(
        &self,
        magnet: &Magnet,
        options: AddTorrentOptions,
    ) -> Result<TorrentHandle, SessionError> {
        let info_hash = magnet.info_hash.ok_or(SessionError::MagnetWithoutV1Hash)?;
        if options.storage.is_some() {
            return Err(SessionError::StorageForMagnet);
        }
        self.add_entry(
            info_hash,
            entry(EntrySource::Magnet(Box::new(magnet.clone())), options),
            true,
        )
    }

    /// Registers `entry`, starts it unless paused, and (when `save`)
    /// persists it.
    fn add_entry(
        &self,
        info_hash: InfoHash,
        mut entry: Entry,
        save: bool,
    ) -> Result<TorrentHandle, SessionError> {
        let mut catalog = self.inner.catalog();
        // A torrent that stopped on an error may be added again.
        if catalog
            .get(&info_hash)
            .is_some_and(|e| e.paused || e.is_running())
        {
            return Err(SessionError::Duplicate(info_hash));
        }
        let handle = if entry.paused {
            self.idle_handle(info_hash)
        } else {
            self.spawn(info_hash, &mut entry)?
        };
        if save && let Some(persist) = self.inner.persister() {
            if let Some(metainfo) = entry.metainfo() {
                persist.save_torrent(metainfo);
            }
            persist.save_record(&entry.record(info_hash));
        }
        catalog.insert(info_hash, entry);
        Ok(handle)
    }

    /// A handle with no task behind it (paused torrents).
    fn idle_handle(&self, info_hash: InfoHash) -> TorrentHandle {
        let (commands, _) = mpsc::channel(1);
        TorrentHandle {
            info_hash,
            commands,
            session: Arc::downgrade(&self.inner),
        }
    }

    /// Starts a task for `entry`. Called with the catalog locked.
    fn spawn(&self, info_hash: InfoHash, entry: &mut Entry) -> Result<TorrentHandle, SessionError> {
        let (source, trackers, mut peers) = match &entry.source {
            EntrySource::Torrent(metainfo) => {
                let storage: Arc<dyn Storage> = match &entry.storage {
                    Some(storage) => Arc::clone(storage),
                    None => Arc::new(FileStorage::new(&entry.download_dir, metainfo.info())?),
                };
                let source = Source::Torrent {
                    metainfo: Arc::clone(metainfo),
                    storage,
                };
                (source, metainfo.trackers(), Vec::new())
            }
            EntrySource::Magnet(magnet) => {
                // Each `tr` is its own tier: all of them are asked concurrently.
                let trackers = magnet.trackers.iter().map(|t| vec![t.clone()]).collect();
                let peers = magnet
                    .peers
                    .iter()
                    .filter_map(|p| p.parse::<SocketAddr>().ok())
                    .collect();
                let source = Source::Magnet {
                    download_dir: entry.download_dir.clone(),
                };
                (source, trackers, peers)
            }
        };
        peers.extend(entry.peers.iter().copied());
        self.start_listening();
        let ctx = TorrentContext {
            info_hash,
            trackers: prepare_tiers(trackers),
            tracker: self.inner.tracker.clone(),
            dht: self.inner.dht.clone(),
            peer_id: self.inner.peer_id,
            config: Arc::clone(&self.inner.config),
            alerts: self.inner.alerts.clone(),
            tracker_key: fastrand::u32(..),
            listen_port: self.inner.listen_port,
            utp: self.inner.utp.get().cloned().unwrap_or_default(),
            catalog: Arc::clone(&self.inner.catalog),
            persist: self.inner.persister().cloned(),
            port_map: self
                .inner
                .port_mapper
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
                .map(PortMapper::subscribe),
        };
        let (tx, rx) = mpsc::channel(64);
        if !peers.is_empty() {
            // Fresh channel with spare capacity: cannot fail.
            let _ = tx.try_send(Command::AddPeers(peers));
        }
        tokio::spawn(TorrentTask::new(ctx, source, entry.priorities.clone(), rx).run());
        self.inner
            .torrents
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(info_hash, tx.clone());
        entry.commands = Some(tx.clone());
        Ok(TorrentHandle {
            info_hash,
            commands: tx,
            session: Arc::downgrade(&self.inner),
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
            session: Arc::downgrade(&self.inner),
        })
    }

    /// Every torrent in the session, running or not, oldest first.
    #[must_use]
    pub fn list(&self) -> Vec<TorrentSummary> {
        let mut list: Vec<TorrentSummary> = self
            .inner
            .catalog()
            .iter()
            .map(|(info_hash, entry)| TorrentSummary {
                info_hash: *info_hash,
                name: entry.name(),
                download_dir: entry.download_dir.clone(),
                paused: entry.paused,
                running: entry.is_running(),
                has_metadata: entry.metainfo().is_some(),
                file_priorities: entry.priorities.clone(),
                added_at: entry.added_at,
            })
            .collect();
        list.sort_by(|a, b| {
            a.added_at
                .cmp(&b.added_at)
                .then_with(|| a.info_hash.as_bytes().cmp(b.info_hash.as_bytes()))
        });
        list
    }

    /// The metainfo of a torrent in the session, once known.
    #[must_use]
    pub fn metainfo(&self, info_hash: InfoHash) -> Option<Arc<Metainfo>> {
        self.inner
            .catalog()
            .get(&info_hash)
            .and_then(Entry::metainfo)
            .cloned()
    }

    /// Stops a torrent but keeps it in the session (and, with a state
    /// directory, paused across restarts). A no-op if already paused.
    ///
    /// # Errors
    /// [`SessionError::Unknown`] if the torrent is not in the session.
    pub async fn pause(&self, info_hash: InfoHash) -> Result<(), SessionError> {
        let commands = {
            let mut catalog = self.inner.catalog();
            let entry = catalog
                .get_mut(&info_hash)
                .ok_or(SessionError::Unknown(info_hash))?;
            if entry.paused {
                return Ok(());
            }
            entry.paused = true;
            if let Some(persist) = self.inner.persister() {
                persist.save_record(&entry.record(info_hash));
            }
            entry.commands.take()
        };
        if let Some(commands) = commands {
            stop_task(&commands).await;
        }
        Ok(())
    }

    /// Starts a paused (or failed) torrent again. Returns its handle; a
    /// running torrent's handle is returned as is.
    ///
    /// Must be called from within a Tokio runtime.
    ///
    /// # Errors
    /// [`SessionError::Unknown`] if the torrent is not in the session;
    /// [`SessionError::Storage`] if its storage cannot be set up.
    pub fn resume(&self, info_hash: InfoHash) -> Result<TorrentHandle, SessionError> {
        let mut catalog = self.inner.catalog();
        let entry = catalog
            .get_mut(&info_hash)
            .ok_or(SessionError::Unknown(info_hash))?;
        if let Some(commands) = entry.commands.clone().filter(|tx| !tx.is_closed()) {
            return Ok(TorrentHandle {
                info_hash,
                commands,
                session: Arc::downgrade(&self.inner),
            });
        }
        let handle = self.spawn(info_hash, entry)?;
        if entry.paused {
            entry.paused = false;
            if let Some(persist) = self.inner.persister() {
                persist.save_record(&entry.record(info_hash));
            }
        }
        Ok(handle)
    }

    /// Stops a torrent and forgets it: its session state and fastresume
    /// data are removed and, with `delete_files`, its downloaded files (for
    /// torrents on the default file storage).
    ///
    /// # Errors
    /// [`SessionError::Unknown`] if the torrent is not in the session;
    /// [`SessionError::Io`] if deleting files failed (the torrent is gone
    /// from the session either way).
    pub async fn remove(
        &self,
        info_hash: InfoHash,
        delete_files: bool,
    ) -> Result<(), SessionError> {
        let entry = self
            .inner
            .catalog()
            .remove(&info_hash)
            .ok_or(SessionError::Unknown(info_hash))?;
        if let Some(commands) = &entry.commands {
            stop_task(commands).await;
        }
        self.forget(&info_hash).await;
        if delete_files
            && entry.storage.is_none()
            && let Some(metainfo) = entry.metainfo().cloned()
        {
            let dir = entry.download_dir.clone();
            tokio::task::spawn_blocking(move || files::delete_content(&dir, metainfo.info()))
                .await
                .map_err(std::io::Error::other)?
                .map_err(SessionError::Io)?;
        }
        Ok(())
    }

    /// Removes a torrent's state and resume files.
    async fn forget(&self, info_hash: &InfoHash) {
        if let Some(persist) = self.inner.persister() {
            persist.remove(info_hash);
        }
        if let Some(path) = self.inner.resume_path(info_hash) {
            let _ =
                tokio::task::spawn_blocking(move || dravity_storage::remove_resume(&path)).await;
        }
    }

    /// Sets one priority per file (all normal when empty). Works on paused
    /// torrents and magnet links whose metadata is not known yet.
    ///
    /// # Errors
    /// [`SessionError::Unknown`] if the torrent is not in the session;
    /// [`SessionError::FileCount`] if the count does not match its files.
    pub async fn set_file_priorities(
        &self,
        info_hash: InfoHash,
        priorities: Vec<Priority>,
    ) -> Result<(), SessionError> {
        let (commands, normalized) = {
            let mut catalog = self.inner.catalog();
            let entry = catalog
                .get_mut(&info_hash)
                .ok_or(SessionError::Unknown(info_hash))?;
            let normalized = match entry.metainfo() {
                Some(metainfo) => files::normalize(metainfo.info(), &priorities).ok_or(
                    SessionError::FileCount {
                        expected: metainfo.info().files().len(),
                        got: priorities.len(),
                    },
                )?,
                None => priorities,
            };
            entry.priorities.clone_from(&normalized);
            if let Some(persist) = self.inner.persister() {
                persist.save_record(&entry.record(info_hash));
            }
            (entry.commands.clone(), normalized)
        };
        if let Some(commands) = commands {
            // A stopped task picks the priorities up from the catalog when
            // it is resumed.
            let _ = commands.send(Command::SetFilePriorities(normalized)).await;
        }
        Ok(())
    }

    /// Sets the priority of one file.
    ///
    /// # Errors
    /// [`SessionError::Unknown`] if the torrent is not in the session;
    /// [`SessionError::NoMetadata`] before a magnet's metadata is known;
    /// [`SessionError::NoSuchFile`] for an index out of range.
    pub async fn set_file_priority(
        &self,
        info_hash: InfoHash,
        index: usize,
        priority: Priority,
    ) -> Result<(), SessionError> {
        let mut priorities = {
            let catalog = self.inner.catalog();
            let entry = catalog
                .get(&info_hash)
                .ok_or(SessionError::Unknown(info_hash))?;
            let metainfo = entry
                .metainfo()
                .ok_or(SessionError::NoMetadata(info_hash))?;
            files::normalize(metainfo.info(), &entry.priorities)
                .unwrap_or_else(|| vec![Priority::Normal; metainfo.info().files().len()])
        };
        *priorities
            .get_mut(index)
            .ok_or(SessionError::NoSuchFile(index))? = priority;
        self.set_file_priorities(info_hash, priorities).await
    }

    /// Adds back every torrent saved in [`SessionConfig::state_dir`], paused
    /// or running as it was. Torrents already in the session are skipped.
    ///
    /// Must be called from within a Tokio runtime.
    ///
    /// # Errors
    /// [`SessionError::NoStateDir`] without a state directory;
    /// [`SessionError::Io`] if it cannot be listed (a missing directory is
    /// an empty one).
    pub async fn restore(&self) -> Result<RestoreReport, SessionError> {
        let dir = self
            .inner
            .config
            .state_dir
            .clone()
            .ok_or(SessionError::NoStateDir)?;
        let loaded = tokio::task::spawn_blocking(move || load_state_dir(&dir))
            .await
            .map_err(std::io::Error::other)?
            .map_err(SessionError::Io)?;
        let mut report = RestoreReport {
            restored: Vec::new(),
            failed: loaded.failed,
        };
        for (path, record, metainfo) in loaded.torrents {
            let source = match (metainfo, &record.magnet) {
                (Some(metainfo), _) => EntrySource::Torrent(Arc::new(metainfo)),
                (None, Some(uri)) => match Magnet::parse(uri) {
                    Ok(magnet) if magnet.info_hash == Some(record.info_hash) => {
                        EntrySource::Magnet(Box::new(magnet))
                    }
                    Ok(_) => {
                        report
                            .failed
                            .push((path, "magnet is for another torrent".into()));
                        continue;
                    }
                    Err(e) => {
                        report.failed.push((path, format!("invalid magnet: {e}")));
                        continue;
                    }
                },
                (None, None) => {
                    report
                        .failed
                        .push((path, "no .torrent file and no magnet link".into()));
                    continue;
                }
            };
            let entry = Entry {
                source,
                download_dir: record.download_dir.clone(),
                peers: Vec::new(),
                storage: None,
                priorities: record.priorities.clone(),
                paused: record.paused,
                added_at: record.added_at,
                commands: None,
            };
            match self.add_entry(record.info_hash, entry, false) {
                Ok(_) => report.restored.push(record.info_hash),
                Err(e) => report.failed.push((path, e.to_string())),
            }
        }
        Ok(report)
    }

    /// Watches a directory for torrents to add (see [`watch`]). The task
    /// ends when the returned handle is dropped or stopped.
    ///
    /// Must be called from within a Tokio runtime.
    #[must_use]
    pub fn watch(&self, config: WatchConfig) -> WatchHandle {
        watch::start(self.clone(), config)
    }

    /// Stops every torrent and waits for them to finish. Torrents stay in
    /// the session state (a restart restores them); session state is
    /// flushed to disk.
    pub async fn shutdown(&self) {
        let running: Vec<mpsc::Sender<Command>> = self
            .inner
            .catalog()
            .values_mut()
            .filter_map(|entry| entry.commands.take())
            .collect();
        self.inner
            .torrents
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        for commands in running {
            stop_task(&commands).await;
        }
        if let Some(persist) = self.inner.persist.get() {
            persist.flush().await;
        }
        let mapper = self
            .inner
            .port_mapper
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(mapper) = mapper {
            // Removing the mappings is a courtesy; never hang on a router.
            let _ = tokio::time::timeout(Duration::from_secs(5), mapper.stop()).await;
        }
    }
}

/// A catalog entry for a newly added torrent.
fn entry(source: EntrySource, options: AddTorrentOptions) -> Entry {
    Entry {
        source,
        download_dir: options.download_dir,
        peers: options.peers,
        storage: options.storage,
        priorities: options.file_priorities,
        paused: options.paused,
        added_at: persist::unix_now(),
        commands: None,
    }
}

/// Stops a torrent task and waits for it. A no-op if it already stopped.
async fn stop_task(commands: &mpsc::Sender<Command>) {
    let (tx, rx) = oneshot::channel();
    if commands.send(Command::Stop(tx)).await.is_ok() {
        let _ = rx.await;
    }
}

/// Torrents found in a state directory, and the files that failed.
struct Loaded {
    torrents: Vec<(PathBuf, TorrentRecord, Option<Metainfo>)>,
    failed: Vec<(PathBuf, String)>,
}

/// Reads every `<info-hash>.state` (and its `.torrent`) in `dir`.
fn load_state_dir(dir: &Path) -> std::io::Result<Loaded> {
    let mut loaded = Loaded {
        torrents: Vec::new(),
        failed: Vec::new(),
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(loaded),
        Err(e) => return Err(e),
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "state"))
        .collect();
    paths.sort();
    for path in paths {
        let record = match persist::read_bounded(&path, persist::MAX_STATE_FILE)
            .map_err(|e| e.to_string())
            .and_then(|b| TorrentRecord::decode(&b).map_err(|e| e.to_string()))
        {
            Ok(record) => record,
            Err(e) => {
                loaded.failed.push((path, e));
                continue;
            }
        };
        if path.file_stem().and_then(|s| s.to_str()) != Some(&record.info_hash.to_hex()) {
            loaded
                .failed
                .push((path, "file name does not match its info-hash".into()));
            continue;
        }
        let torrent_path = path.with_extension("torrent");
        let metainfo = match persist::read_bounded(&torrent_path, persist::MAX_TORRENT_FILE) {
            Ok(bytes) => match Metainfo::from_bytes(&bytes) {
                Ok(m) if m.info_hash() == record.info_hash => Some(m),
                Ok(_) => {
                    loaded
                        .failed
                        .push((torrent_path, "metainfo is for another torrent".into()));
                    continue;
                }
                Err(e) => {
                    loaded.failed.push((torrent_path, e.to_string()));
                    continue;
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                loaded.failed.push((torrent_path, e.to_string()));
                continue;
            }
        };
        loaded.torrents.push((path, record, metainfo));
    }
    Ok(loaded)
}

/// Controls one torrent in a [`Session`]. Cheap to clone.
#[derive(Debug, Clone)]
pub struct TorrentHandle {
    info_hash: InfoHash,
    commands: mpsc::Sender<Command>,
    session: Weak<Inner>,
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

    fn not_running(&self) -> SessionError {
        SessionError::NotRunning(self.info_hash)
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
            .map_err(|_| self.not_running())?;
        rx.await.map_err(|_| self.not_running())
    }

    /// A progress snapshot with per-file progress and priorities.
    ///
    /// # Errors
    /// [`SessionError::NotRunning`] once the torrent has stopped.
    pub async fn progress(&self) -> Result<TorrentProgress, SessionError> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::Progress(tx))
            .await
            .map_err(|_| self.not_running())?;
        rx.await.map_err(|_| self.not_running())
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
            .map_err(|_| self.not_running())
    }

    /// Opens file `index` for reading while it downloads: reads wait for
    /// their pieces, which are fetched before others (see [`FileStream`]).
    ///
    /// # Errors
    /// [`SessionError::NotRunning`] once the torrent has stopped;
    /// [`SessionError::NoMetadata`] while a magnet's metadata is unknown;
    /// [`SessionError::NoSuchFile`] for an index out of range or a padding
    /// file.
    pub async fn open_file(&self, index: usize) -> Result<FileStream, SessionError> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::Content(tx))
            .await
            .map_err(|_| self.not_running())?;
        let content = rx
            .await
            .map_err(|_| self.not_running())?
            .ok_or(SessionError::NoMetadata(self.info_hash))?;
        let file = content
            .metainfo
            .info()
            .files()
            .get(index)
            .filter(|f| !f.is_padding())
            .ok_or(SessionError::NoSuchFile(index))?;
        let (start, len) = (file.offset(), file.length());
        Ok(FileStream::new(
            self.commands.clone(),
            content.metainfo,
            content.storage,
            start,
            len,
        ))
    }

    /// Stops the torrent and removes it from the session (like
    /// [`Session::remove`] without deleting files, but the fastresume data
    /// is kept): disconnects peers, announces `stopped` (bounded by
    /// [`SessionConfig::stop_announce_timeout`]), flushes storage, and waits
    /// for all of that. A no-op if it already stopped.
    pub async fn stop(&self) {
        stop_task(&self.commands).await;
        let Some(inner) = self.session.upgrade() else {
            return;
        };
        let removed = {
            let mut catalog = inner.catalog();
            let ours = catalog.get(&self.info_hash).is_some_and(|e| {
                e.commands
                    .as_ref()
                    .is_some_and(|tx| tx.same_channel(&self.commands))
            });
            ours && catalog.remove(&self.info_hash).is_some()
        };
        if removed && let Some(persist) = inner.persister() {
            persist.remove(&self.info_hash);
        }
    }
}
