//! [`ClientBuilder`] and [`Client`].

use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dravity_core::{Magnet, Metainfo};
use dravity_session::transport::Network;
use dravity_session::{
    AddTorrentOptions, EncryptionPolicy, ListenConfig, LsdConfig, Session, SessionConfig,
    TorrentHandle,
};

use crate::{Error, Result};

/// Builds a [`Client`]. Starts from [`SessionConfig::default`]; use
/// [`config`](Self::config) for anything without a dedicated method.
#[derive(Debug, Clone, Default)]
#[must_use]
pub struct ClientBuilder {
    config: SessionConfig,
}

impl ClientBuilder {
    /// A builder with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Accept peers on `port` (TCP and uTP; 0 picks a free port).
    pub fn listen_port(mut self, port: u16) -> Self {
        self.config.listen = Some(ListenConfig::new(port));
        self.config.announce_port = port;
        self
    }

    /// Only dial out; accept no incoming peers.
    pub fn no_listen(mut self) -> Self {
        self.config.listen = None;
        self
    }

    /// Use the DHT (default on).
    pub fn dht(mut self, on: bool) -> Self {
        if !on {
            self.config.dht = None;
        } else if self.config.dht.is_none() {
            self.config.dht = SessionConfig::default().dht;
        }
        self
    }

    /// Local Service Discovery (default off).
    pub fn lsd(mut self, on: bool) -> Self {
        self.config.lsd = on.then(LsdConfig::default);
        self
    }

    /// Router port mapping (default on).
    pub fn port_mapping(mut self, on: bool) -> Self {
        if !on {
            self.config.port_mapping = None;
        } else if self.config.port_mapping.is_none() {
            self.config.port_mapping = SessionConfig::default().port_mapping;
        }
        self
    }

    /// Session-wide download limit in bytes per second (`None`: unlimited).
    pub fn download_limit(mut self, bytes_per_second: Option<u64>) -> Self {
        self.config.download_rate_limit = bytes_per_second;
        self
    }

    /// Session-wide upload limit in bytes per second (`None`: unlimited).
    pub fn upload_limit(mut self, bytes_per_second: Option<u64>) -> Self {
        self.config.upload_rate_limit = bytes_per_second;
        self
    }

    /// Open peer connections across all torrents (`None`: unlimited).
    pub fn max_connections(mut self, max: Option<usize>) -> Self {
        self.config.max_connections = max;
        self
    }

    /// Remember torrents in `dir` and restore them with
    /// [`Session::restore`]; fastresume data goes to `dir/resume`.
    pub fn state_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        self.config.resume_dir = Some(dir.join("resume"));
        self.config.state_dir = Some(dir);
        self
    }

    /// Message Stream Encryption policy.
    pub fn encryption(mut self, policy: EncryptionPolicy) -> Self {
        self.config.encryption = policy;
        self
    }

    /// Dial peers through a proxy URL (`socks5://`, `socks4://` or
    /// `http://`).
    ///
    /// # Errors
    /// [`Error::Proxy`] for an invalid URL.
    pub fn proxy(mut self, url: &str) -> Result<Self> {
        self.config.proxy = Some(dravity_session::proxy::Proxy::parse(url)?);
        Ok(self)
    }

    /// Carry peer connections over a custom [`Network`] (for example an
    /// in-memory one in tests) instead of sockets.
    pub fn network(mut self, network: Arc<dyn Network>) -> Self {
        self.config.network = Some(network);
        self
    }

    /// Changes any other setting.
    pub fn config(mut self, f: impl FnOnce(&mut SessionConfig)) -> Self {
        f(&mut self.config);
        self
    }

    /// Starts the client. Call from within a Tokio runtime.
    ///
    /// # Errors
    /// [`Error::Session`] if the configuration is invalid or a socket
    /// cannot be bound.
    pub fn build(self) -> Result<Client> {
        Ok(Client {
            session: Session::new(self.config)?,
        })
    }
}

/// A running BitTorrent client: a [`Session`] with conveniences for the
/// common cases. Every [`Session`] method is available through `Deref`.
#[derive(Debug, Clone)]
pub struct Client {
    session: Session,
}

impl Client {
    /// A builder.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    /// The underlying session.
    #[must_use]
    pub const fn session(&self) -> &Session {
        &self.session
    }

    /// Adds a `.torrent` file's bytes, downloading into `dir`.
    ///
    /// # Errors
    /// [`Error::Metainfo`] for invalid bytes, [`Error::Session`] if the
    /// session refuses the torrent.
    pub fn add_torrent_bytes(
        &self,
        bytes: &[u8],
        dir: impl Into<PathBuf>,
    ) -> Result<TorrentHandle> {
        let metainfo = Metainfo::from_bytes(bytes)?;
        Ok(self
            .session
            .add_torrent(metainfo, AddTorrentOptions::new(dir))?)
    }

    /// Adds a `.torrent` file, downloading into `dir`.
    ///
    /// # Errors
    /// [`Error::Io`] if the file cannot be read, otherwise as
    /// [`add_torrent_bytes`](Self::add_torrent_bytes).
    pub fn add_torrent_file(&self, path: &Path, dir: impl Into<PathBuf>) -> Result<TorrentHandle> {
        let bytes = std::fs::read(path).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        self.add_torrent_bytes(&bytes, dir)
    }

    /// Adds a magnet link, downloading into `dir`.
    ///
    /// # Errors
    /// [`Error::Magnet`] for an invalid link, [`Error::Session`] if the
    /// session refuses it.
    pub fn add_magnet(&self, uri: &str, dir: impl Into<PathBuf>) -> Result<TorrentHandle> {
        let magnet = Magnet::parse(uri)?;
        Ok(self
            .session
            .add_magnet(&magnet, AddTorrentOptions::new(dir))?)
    }

    /// Stops every torrent and waits for them.
    pub async fn shutdown(&self) {
        self.session.shutdown().await;
    }
}

impl Deref for Client {
    type Target = Session;

    fn deref(&self) -> &Session {
        &self.session
    }
}
