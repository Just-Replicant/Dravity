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

pub mod bloom;
pub mod demux;
pub mod items;
pub mod krpc;
pub mod native;
pub mod relay;
pub mod routing;
pub mod secure_id;
pub mod store;

pub use relay::{Contact, Socks5Proxy};

use std::collections::HashSet;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

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
    /// Send the native node's datagrams through this SOCKS5 UDP relay.
    /// The node is not started (and sends or accepts nothing) until the
    /// association is up; bootstrap host names go to the proxy as domain
    /// addresses and are never resolved locally. Ignored when [`Self::native`]
    /// is `false` (`mainline` has no proxy support).
    pub socks5: Option<Socks5Proxy>,
    /// Run a second native node on an IPv6 socket (BEP 32 dual-stack).
    /// Ignored when [`Self::native`] is `false`. The IPv6 node has its own
    /// routing table and is bound with `IPV6_V6ONLY` so it can share
    /// [`Self::port`] with the IPv4 node.
    pub ipv6: bool,
    /// Address to bind the IPv6 node; `None` binds all IPv6 interfaces.
    pub bind_address_v6: Option<Ipv6Addr>,
    /// UDP port for the IPv6 node; `None` uses [`Self::port`] (or any free
    /// port).
    pub port_v6: Option<u16>,
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
            socks5: None,
            ipv6: false,
            bind_address_v6: None,
            port_v6: None,
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
        node: NativeSlot,
        /// A second node on an IPv6 socket when [`DhtConfig::ipv6`] is set.
        node6: Option<NativeSlot>,
        /// `host:port` entries, resolved at each (re)bootstrap unless
        /// the node is proxied (then they are sent as domain addresses).
        bootstrap: std::sync::Arc<Vec<String>>,
        /// Through a SOCKS5 relay: never resolve names locally.
        proxied: bool,
    },
}

/// The native node, or a delayed start that is waiting for a SOCKS5
/// association (during which nothing is sent or accepted).
#[derive(Debug, Clone)]
enum NativeSlot {
    Ready(native::NativeDht),
    Starting(tokio::sync::watch::Receiver<std::sync::Arc<NativePhase>>),
}

#[derive(Debug, Clone)]
enum NativePhase {
    Pending,
    Up(native::NativeDht),
    Failed(std::sync::Arc<str>),
}

impl NativeSlot {
    /// The node if it is already running (a local read; does not wait).
    fn ready(&self) -> Option<native::NativeDht> {
        match self {
            Self::Ready(node) => Some(node.clone()),
            Self::Starting(rx) => match rx.borrow().as_ref() {
                NativePhase::Up(node) => Some(node.clone()),
                NativePhase::Pending | NativePhase::Failed(_) => None,
            },
        }
    }

    /// Waits until the SOCKS5 association succeeds or fails.
    async fn wait(&self) -> Option<native::NativeDht> {
        match self {
            Self::Ready(node) => Some(node.clone()),
            Self::Starting(rx) => {
                let mut rx = rx.clone();
                loop {
                    let phase = std::sync::Arc::clone(&rx.borrow());
                    match phase.as_ref() {
                        NativePhase::Up(node) => return Some(node.clone()),
                        NativePhase::Failed(reason) => {
                            tracing::debug!(%reason, "native DHT SOCKS5 associate failed");
                            return None;
                        }
                        NativePhase::Pending => {
                            rx.changed().await.ok()?;
                        }
                    }
                }
            }
        }
    }
}

/// The public routers a native node joins through by default.
pub const DEFAULT_BOOTSTRAP: &[&str] = &[
    "router.bittorrent.com:6881",
    "dht.transmissionbt.com:6881",
    "dht.libtorrent.org:25401",
];

/// Per-query timeout of the native node.
const NATIVE_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

async fn resolve(entries: &[String], ipv6: bool) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    for entry in entries {
        if let Ok(addrs) = tokio::net::lookup_host(entry.as_str()).await {
            out.extend(addrs.filter(|a| a.is_ipv6() == ipv6));
        }
    }
    out
}

fn bind_v6_only(addr: Ipv6Addr, port: u16) -> std::io::Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_only_v6(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&SocketAddr::from((addr, port)).into())?;
    Ok(socket.into())
}

fn spawn_relayed(
    proxy: Socks5Proxy,
    native_config: native::NativeConfig,
    bootstrap: std::sync::Arc<Vec<String>>,
) -> NativeSlot {
    let (tx, rx) = tokio::sync::watch::channel(std::sync::Arc::new(NativePhase::Pending));
    tokio::spawn(async move {
        match native::NativeDht::bind_relayed(&proxy, native_config).await {
            Ok(node) => {
                let joining = node.clone();
                let _ = tx.send(std::sync::Arc::new(NativePhase::Up(node)));
                joining
                    .bootstrap_contacts(&Contact::parse_all(&bootstrap))
                    .await;
            }
            Err(error) => {
                tracing::warn!(%error, "native DHT SOCKS5 associate failed");
                let _ = tx.send(std::sync::Arc::new(NativePhase::Failed(
                    error.to_string().into(),
                )));
            }
        }
    });
    NativeSlot::Starting(rx)
}

fn spawn_direct(
    socket: std::net::UdpSocket,
    native_config: native::NativeConfig,
    bootstrap: std::sync::Arc<Vec<String>>,
    ipv6: bool,
) -> Result<NativeSlot, DhtError> {
    let node = native::NativeDht::from_std(socket, native_config)?;
    let joining = node.clone();
    tokio::spawn(async move {
        joining.bootstrap(&resolve(&bootstrap, ipv6).await).await;
    });
    Ok(NativeSlot::Ready(node))
}

async fn bootstrap_slot(
    slot: &NativeSlot,
    bootstrap: &[String],
    proxied: bool,
    ipv6: bool,
) -> bool {
    let Some(node) = slot.wait().await else {
        return false;
    };
    if proxied {
        node.bootstrap_contacts(&Contact::parse_all(bootstrap))
            .await
            > 0
    } else {
        node.bootstrap(&resolve(bootstrap, ipv6).await).await > 0
    }
}

async fn peers_from_slot(
    slot: &NativeSlot,
    info_hash: [u8; 20],
    bootstrap: &[String],
    proxied: bool,
    ipv6: bool,
) -> Vec<SocketAddr> {
    let Some(node) = slot.wait().await else {
        return Vec::new();
    };
    if node.table_len() == 0 {
        if proxied {
            node.bootstrap_contacts(&Contact::parse_all(bootstrap))
                .await;
        } else {
            node.bootstrap(&resolve(bootstrap, ipv6).await).await;
        }
    }
    node.get_peers(info_hash).await.peers
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
        let bootstrap = std::sync::Arc::new(
            config
                .bootstrap
                .clone()
                .unwrap_or_else(|| DEFAULT_BOOTSTRAP.iter().map(|s| (*s).to_owned()).collect()),
        );
        let native_config = native::NativeConfig {
            timeout: NATIVE_QUERY_TIMEOUT,
            ..native::NativeConfig::default()
        };
        let node = if let Some(proxy) = config.socks5.clone() {
            spawn_relayed(
                proxy,
                native_config.clone(),
                std::sync::Arc::clone(&bootstrap),
            )
        } else {
            let socket = std::net::UdpSocket::bind((
                config.bind_address.unwrap_or(Ipv4Addr::UNSPECIFIED),
                config.port.unwrap_or(0),
            ))?;
            spawn_direct(
                socket,
                native_config.clone(),
                std::sync::Arc::clone(&bootstrap),
                false,
            )?
        };
        let node6 = if config.ipv6 {
            Some(if let Some(proxy) = config.socks5.clone() {
                spawn_relayed(proxy, native_config, std::sync::Arc::clone(&bootstrap))
            } else {
                let socket = bind_v6_only(
                    config.bind_address_v6.unwrap_or(Ipv6Addr::UNSPECIFIED),
                    config.port_v6.or(config.port).unwrap_or(0),
                )?;
                spawn_direct(
                    socket,
                    native_config,
                    std::sync::Arc::clone(&bootstrap),
                    true,
                )?
            })
        } else {
            None
        };
        Ok(Self {
            inner: Backend::Native {
                node,
                node6,
                bootstrap,
                proxied: config.socks5.is_some(),
            },
            max_peers: config.max_peers.max(1),
        })
    }

    /// Local address of the node's UDP socket.
    pub async fn local_addr(&self) -> SocketAddr {
        match &self.inner {
            Backend::Mainline(dht) => SocketAddr::V4(dht.info().await.local_addr()),
            Backend::Native { node, .. } => node
                .ready()
                .and_then(|n| n.local_addr().ok())
                .unwrap_or_else(|| SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))),
        }
    }

    /// Local address of the IPv6 node's UDP socket, if [`DhtConfig::ipv6`]
    /// is on and the node is running.
    pub async fn local_addr_v6(&self) -> Option<SocketAddr> {
        match &self.inner {
            Backend::Mainline(_) => None,
            Backend::Native { node6, .. } => node6
                .as_ref()
                .and_then(NativeSlot::ready)
                .and_then(|n| n.local_addr().ok()),
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
            Backend::Native {
                node,
                node6,
                bootstrap,
                proxied,
            } => {
                let v4 = bootstrap_slot(node, bootstrap, *proxied, false).await;
                let v6 = if let Some(node6) = node6 {
                    bootstrap_slot(node6, bootstrap, *proxied, true).await
                } else {
                    false
                };
                v4 || v6
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
            Backend::Native { node, node6, .. } => {
                let mut out: Vec<String> = node
                    .ready()
                    .map(|n| n.nodes().iter().map(ToString::to_string).collect())
                    .unwrap_or_default();
                if let Some(node6) = node6
                    && let Some(n) = node6.ready()
                {
                    out.extend(n.nodes().iter().map(ToString::to_string));
                }
                out
            }
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
            Backend::Native {
                node,
                node6,
                bootstrap,
                proxied,
            } => {
                let node = node.clone();
                let node6 = node6.clone();
                let bootstrap = std::sync::Arc::clone(bootstrap);
                let proxied = *proxied;
                futures_util::stream::once(async move {
                    let hash = *info_hash.as_bytes();
                    let mut peers = peers_from_slot(&node, hash, &bootstrap, proxied, false).await;
                    if let Some(node6) = &node6 {
                        peers.extend(peers_from_slot(node6, hash, &bootstrap, proxied, true).await);
                    }
                    peers.retain(usable);
                    peers
                })
                .right_stream()
            }
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
            Backend::Native { node, node6, .. } => {
                let hash = *info_hash.as_bytes();
                let mut accepted = 0usize;
                let mut last_err: Option<DhtError> = None;
                for slot in std::iter::once(node).chain(node6.as_ref()) {
                    match slot.wait().await {
                        Some(n) => accepted += n.announce(hash, port).await.1,
                        None => {
                            last_err =
                                Some(DhtError::Announce("SOCKS5 relay is not up".to_owned()));
                        }
                    }
                }
                if accepted > 0 {
                    Ok(())
                } else {
                    Err(last_err
                        .unwrap_or_else(|| DhtError::Announce("no node accepted".to_owned())))
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
