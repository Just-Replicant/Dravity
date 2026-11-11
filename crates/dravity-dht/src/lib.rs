//! Mainline DHT (BEP 5) peer discovery for the Dravity BitTorrent engine.
//!
//! A thin wrapper around the [`mainline`] crate, which runs the DHT node
//! (routing table, KRPC, token handling) on its own thread. This crate
//! exposes only what a BitTorrent client needs — "who has this info-hash?"
//! and "I have this info-hash" — in Dravity's own types, so the rest of the
//! workspace never depends on `mainline` directly and the backend can be
//! replaced later.
//!
//! The async methods are runtime-agnostic; the session wraps them in its own
//! timeouts.
//!
//! ```no_run
//! # async fn demo() -> Result<(), dravity_dht::DhtError> {
//! use dravity_dht::{Dht, DhtConfig};
//! let dht = Dht::new(&DhtConfig::default())?;
//! let hash = "c12fe1c06bba254a9dc9f519b335aa7c1367a88a".parse().unwrap();
//! let peers = dht.get_peers(hash).await;
//! # let _ = peers; Ok(()) }
//! ```

pub mod demux;
pub mod krpc;
pub mod native;
pub mod routing;
pub mod secure_id;
pub mod store;

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};

use dravity_core::InfoHash;
use futures_util::{Stream, StreamExt};
use mainline::async_dht::AsyncDht;

/// Default cap on peers collected by one [`Dht::get_peers`] lookup.
pub const DEFAULT_MAX_PEERS: usize = 500;

/// Errors from the DHT.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DhtError {
    /// The UDP socket could not be bound or the node thread not started.
    #[error("could not start the DHT node: {0}")]
    Start(#[from] std::io::Error),
    /// An `announce_peer` query reached no node that stored it.
    #[error("DHT announce failed: {0}")]
    Announce(String),
}

/// Settings for a [`Dht`] node.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DhtConfig {
    /// Bootstrap nodes as `host:port`. `None` uses `mainline`'s defaults
    /// (the well-known public routers); an empty list starts isolated.
    pub bootstrap: Option<Vec<String>>,
    /// UDP port to bind; `None` picks any free port.
    pub port: Option<u16>,
    /// Address to bind; `None` binds all interfaces.
    pub bind_address: Option<Ipv4Addr>,
    /// Answer queries from other nodes from the start instead of deciding
    /// automatically once the node knows it is reachable.
    pub server_mode: bool,
    /// Stop a lookup after collecting this many distinct peers.
    pub max_peers: usize,
    /// Run Dravity's own node ([`native::NativeDht`]) instead of
    /// `mainline` (default `false` until it has been checked against the
    /// live network). Needs a Tokio runtime when the node starts;
    /// `server_mode` does not apply (the native node always answers).
    pub native: bool,
}

impl Default for DhtConfig {
    fn default() -> Self {
        Self {
            bootstrap: None,
            port: None,
            bind_address: None,
            server_mode: false,
            max_peers: DEFAULT_MAX_PEERS,
            native: false,
        }
    }
}

impl DhtConfig {
    /// A configuration that bootstraps from `nodes` only (e.g. a local
    /// test network).
    #[must_use]
    pub fn with_bootstrap(nodes: Vec<String>) -> Self {
        Self {
            bootstrap: Some(nodes),
            ..Self::default()
        }
    }
}

/// A running DHT node. Cloning is cheap and shares the node; it shuts down
/// when the last clone is dropped.
#[derive(Debug, Clone)]
pub struct Dht {
    inner: Backend,
    max_peers: usize,
}

#[derive(Debug, Clone)]
enum Backend {
    Mainline(AsyncDht),
    Native {
        node: native::NativeDht,
        /// `host:port` entries, resolved at each (re)bootstrap.
        bootstrap: std::sync::Arc<Vec<String>>,
    },
}

/// The public routers a native node joins through by default.
pub const DEFAULT_BOOTSTRAP: &[&str] = &[
    "router.bittorrent.com:6881",
    "dht.transmissionbt.com:6881",
    "dht.libtorrent.org:25401",
];

/// Per-query timeout of the native node.
const NATIVE_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

async fn resolve(entries: &[String]) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    for entry in entries {
        if let Ok(addrs) = tokio::net::lookup_host(entry.as_str()).await {
            out.extend(addrs.filter(SocketAddr::is_ipv4));
        }
    }
    out
}

impl Dht {
    /// Starts a node.
    ///
    /// # Errors
    /// [`DhtError::Start`] if the socket cannot be bound.
    pub fn new(config: &DhtConfig) -> Result<Self, DhtError> {
        if config.native {
            return Self::new_native(config);
        }
        let mut builder = mainline::Dht::builder();
        match &config.bootstrap {
            Some(nodes) if nodes.is_empty() => {
                builder.no_bootstrap();
            }
            Some(nodes) => {
                builder.bootstrap(nodes);
            }
            None => {}
        }
        if let Some(port) = config.port {
            builder.port(port);
        }
        if let Some(addr) = config.bind_address {
            builder.bind_address(addr);
        }
        if config.server_mode {
            builder.server_mode();
        }
        Ok(Self {
            inner: Backend::Mainline(builder.build()?.as_async()),
            max_peers: config.max_peers.max(1),
        })
    }

    fn new_native(config: &DhtConfig) -> Result<Self, DhtError> {
        let runtime = tokio::runtime::Handle::try_current().map_err(std::io::Error::other)?;
        let _entered = runtime.enter();
        let socket = std::net::UdpSocket::bind((
            config.bind_address.unwrap_or(Ipv4Addr::UNSPECIFIED),
            config.port.unwrap_or(0),
        ))?;
        let node = native::NativeDht::from_std(socket, NATIVE_QUERY_TIMEOUT)?;
        let bootstrap = std::sync::Arc::new(
            config
                .bootstrap
                .clone()
                .unwrap_or_else(|| DEFAULT_BOOTSTRAP.iter().map(|s| (*s).to_owned()).collect()),
        );
        let joining = node.clone();
        let entries = std::sync::Arc::clone(&bootstrap);
        tokio::spawn(async move {
            joining.bootstrap(&resolve(&entries).await).await;
        });
        Ok(Self {
            inner: Backend::Native { node, bootstrap },
            max_peers: config.max_peers.max(1),
        })
    }

    /// Local address of the node's UDP socket.
    pub async fn local_addr(&self) -> SocketAddr {
        match &self.inner {
            Backend::Mainline(dht) => SocketAddr::V4(dht.info().await.local_addr()),
            Backend::Native { node, .. } => node
                .local_addr()
                .unwrap_or_else(|_| SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))),
        }
    }

    /// Asks the network for nodes near our id. `true` if any node answered.
    ///
    /// This sends a `find_node` query. It is not a snapshot of whether
    /// bootstrap has already filled the routing table; use [`Self::has_nodes`]
    /// for that.
    pub async fn bootstrapped(&self) -> bool {
        match &self.inner {
            Backend::Mainline(dht) => dht.bootstrapped().await,
            Backend::Native { node, bootstrap } => {
                node.bootstrap(&resolve(bootstrap).await).await > 0
            }
        }
    }

    /// Whether the routing table currently has a live neighbor.
    ///
    /// A local read. An empty table means bootstrap has not succeeded, so a
    /// peer lookup that found nobody should be retried soon instead of being
    /// treated as an empty swarm.
    pub async fn has_nodes(&self) -> bool {
        !self.to_bootstrap().await.is_empty()
    }

    /// The node's routing table as `host:port` bootstrap entries (useful to
    /// start other nodes from this one).
    pub async fn to_bootstrap(&self) -> Vec<String> {
        match &self.inner {
            Backend::Mainline(dht) => dht.to_bootstrap().await,
            Backend::Native { node, .. } => node.nodes().iter().map(ToString::to_string).collect(),
        }
    }

    /// Batches of peers for `info_hash`, as the DHT reports them.
    ///
    /// The session applies its own deadline so addresses found before a
    /// timeout are kept. Addresses are untrusted. Port 0 and unspecified
    /// addresses are dropped. The stream does not cap itself; callers stop
    /// at [`DEFAULT_MAX_PEERS`].
    pub fn peer_batches(
        &self,
        info_hash: InfoHash,
    ) -> impl Stream<Item = Vec<SocketAddr>> + Send + '_ {
        let usable = |addr: &SocketAddr| addr.port() != 0 && !addr.ip().is_unspecified();
        match &self.inner {
            Backend::Mainline(dht) => dht
                .get_peers(mainline::Id::from(*info_hash.as_bytes()))
                .map(move |batch| {
                    batch
                        .into_iter()
                        .map(SocketAddr::V4)
                        .filter(usable)
                        .collect()
                })
                .left_stream(),
            Backend::Native { node, bootstrap } => futures_util::stream::once(async move {
                if node.table_len() == 0 {
                    node.bootstrap(&resolve(bootstrap).await).await;
                }
                let mut peers = node.get_peers(*info_hash.as_bytes()).await.peers;
                peers.retain(usable);
                peers
            })
            .right_stream(),
        }
    }

    /// Looks up peers for `info_hash`, returning distinct addresses as they
    /// arrive until the query finishes or the peer cap is reached.
    ///
    /// A timeout wrapped around this method drops peers already collected.
    /// [`Self::peer_batches`] yields them as they arrive so the caller can
    /// keep a partial result. Addresses come from arbitrary DHT nodes and
    /// are untrusted.
    pub async fn get_peers(&self, info_hash: InfoHash) -> Vec<SocketAddr> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        let mut stream = std::pin::pin!(self.peer_batches(info_hash));
        while let Some(batch) = stream.next().await {
            for addr in batch {
                if seen.insert(addr) {
                    out.push(addr);
                    if out.len() >= self.max_peers {
                        return out;
                    }
                }
            }
        }
        out
    }

    /// Announces that we accept peers for `info_hash` on `port` (`None`:
    /// the DHT socket's own port, BEP 5 `implied_port`).
    ///
    /// # Errors
    /// [`DhtError::Announce`] if no node accepted the announce.
    pub async fn announce(&self, info_hash: InfoHash, port: Option<u16>) -> Result<(), DhtError> {
        match &self.inner {
            Backend::Mainline(dht) => dht
                .announce_peer(mainline::Id::from(*info_hash.as_bytes()), port)
                .await
                .map(drop)
                .map_err(|e| DhtError::Announce(e.to_string())),
            Backend::Native { node, .. } => {
                match node.announce(*info_hash.as_bytes(), port).await.1 {
                    0 => Err(DhtError::Announce("no node accepted".to_owned())),
                    _ => Ok(()),
                }
            }
        }
    }
}

/// A local DHT network for tests: `count` nodes on `127.0.0.1` that know
/// each other.
#[derive(Debug)]
pub struct Testnet {
    inner: mainline::Testnet,
}

impl Testnet {
    /// Starts the network.
    ///
    /// # Errors
    /// [`DhtError::Start`] if a socket cannot be bound.
    pub fn new(count: usize) -> Result<Self, DhtError> {
        Ok(Self {
            inner: mainline::Testnet::builder(count)
                .bind_address(Ipv4Addr::LOCALHOST)
                .build()?,
        })
    }

    /// Bootstrap entries for nodes joining the network.
    #[must_use]
    pub fn bootstrap(&self) -> Vec<String> {
        self.inner.bootstrap.clone()
    }

    /// A [`DhtConfig`] that joins this network on `127.0.0.1`.
    #[must_use]
    pub fn config(&self) -> DhtConfig {
        DhtConfig {
            bind_address: Some(Ipv4Addr::LOCALHOST),
            ..DhtConfig::with_bootstrap(self.bootstrap())
        }
    }
}
