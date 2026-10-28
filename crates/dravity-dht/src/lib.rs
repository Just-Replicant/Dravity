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

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};

use dravity_core::InfoHash;
use futures_util::StreamExt;
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
}

impl Default for DhtConfig {
    fn default() -> Self {
        Self {
            bootstrap: None,
            port: None,
            bind_address: None,
            server_mode: false,
            max_peers: DEFAULT_MAX_PEERS,
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
    inner: AsyncDht,
    max_peers: usize,
}

impl Dht {
    /// Starts a node.
    ///
    /// # Errors
    /// [`DhtError::Start`] if the socket cannot be bound.
    pub fn new(config: &DhtConfig) -> Result<Self, DhtError> {
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
            inner: builder.build()?.as_async(),
            max_peers: config.max_peers.max(1),
        })
    }

    /// Local address of the node's UDP socket.
    pub async fn local_addr(&self) -> SocketAddr {
        SocketAddr::V4(self.inner.info().await.local_addr())
    }

    /// Asks the network for nodes near our id. `true` if any node answered.
    ///
    /// This sends a `find_node` query. It is not a snapshot of whether
    /// bootstrap has already filled the routing table; use [`Self::has_nodes`]
    /// for that.
    pub async fn bootstrapped(&self) -> bool {
        self.inner.bootstrapped().await
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
        self.inner.to_bootstrap().await
    }

    /// Looks up peers for `info_hash`, returning distinct addresses as they
    /// arrive until the query finishes or the peer cap is reached.
    ///
    /// The result is untrusted: addresses come from arbitrary DHT nodes.
    pub async fn get_peers(&self, info_hash: InfoHash) -> Vec<SocketAddr> {
        let mut stream = self
            .inner
            .get_peers(mainline::Id::from(*info_hash.as_bytes()));
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        while let Some(batch) = stream.next().await {
            for addr in batch {
                let addr = SocketAddr::V4(addr);
                if addr.port() != 0 && !addr.ip().is_unspecified() && seen.insert(addr) {
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
        self.inner
            .announce_peer(mainline::Id::from(*info_hash.as_bytes()), port)
            .await
            .map(drop)
            .map_err(|e| DhtError::Announce(e.to_string()))
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
