//! Dravity's own DHT node over one UDP socket. It answers the BEP 5
//! queries from its [`RoutingTable`], [`Tokens`] and [`PeerStore`], plus
//! BEP 33 scrapes, BEP 44 `get`/`put`, BEP 51 `sample_infohashes`; it
//! honours BEP 43 read-only nodes (and can be one), tells queriers their
//! address and enforces BEP 42 node ids (BEP 42), and runs iterative
//! lookups for all of them.
//!
//! The session uses it with [`crate::DhtConfig::native`]; `mainline`
//! stays the default until it has been validated on the live network.
//!
//! With a [`crate::Socks5Proxy`], [`NativeDht::bind_relayed`] starts
//! the node only after a SOCKS5 UDP ASSOCIATE succeeds, so nothing is
//! sent or accepted until the relay is up. Bootstrap host names go to
//! the proxy as domain addresses and are never resolved locally.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use ed25519_dalek::SigningKey;
use futures_util::future::join_all;
use tokio::net::UdpSocket;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::bloom::Bloom;
use crate::items::{self, Item, ItemStore, Mutable};
use crate::krpc::{Body, Ext, Message, Node, Query};
use crate::relay::{Contact, Socks5Proxy, Transport, UdpAssociate};
use crate::routing::{K, RoutingTable, distance};
use crate::secure_id::{is_secure, secure_node_id};
use crate::store::{PeerStore, TOKEN_ROTATION, Tokens};

/// Queries in flight at once during a lookup (BEP 5's alpha).
pub const ALPHA: usize = 3;
/// Rounds after which a lookup stops even if it still finds closer nodes.
const MAX_ROUNDS: usize = 32;
/// Peers returned per `get_peers` answer.
const MAX_VALUES: usize = 50;
/// Info-hashes per `sample_infohashes` answer (BEP 51 fits about 20).
const MAX_SAMPLES: usize = 20;
/// The `interval` we ask samplers to wait (BEP 51), in seconds.
const SAMPLE_INTERVAL: i64 = 60;

/// How a [`NativeDht`] behaves.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NativeConfig {
    /// Time allowed per query.
    pub timeout: Duration,
    /// BEP 43: mark our queries read-only and answer nobody (for nodes
    /// that cannot receive unsolicited datagrams).
    pub read_only: bool,
    /// BEP 42: keep only nodes whose id matches their address in the
    /// routing table (local and private addresses are exempt), and take
    /// a matching id for ourselves once a node tells us our address.
    pub enforce_bep42: bool,
}

impl Default for NativeConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(2),
            read_only: false,
            enforce_bep42: true,
        }
    }
}

impl NativeConfig {
    /// The defaults, with this query timeout.
    #[must_use]
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            timeout,
            ..Self::default()
        }
    }
}

/// What a `get_peers` lookup found.
#[derive(Debug, Clone, Default)]
pub struct Lookup {
    /// Peers for the info-hash, deduplicated.
    pub peers: Vec<SocketAddr>,
    /// The closest nodes that answered, closest first, with the token
    /// each gave (needed to announce or put to it).
    pub closest: Vec<(Node, Option<Vec<u8>>)>,
}

/// A BEP 33 scrape: the merged filters and their estimates.
#[derive(Debug, Clone, Default)]
pub struct Scrape {
    /// Seeds' filter.
    pub seeds: Bloom,
    /// Downloaders' filter.
    pub peers: Bloom,
}

#[derive(Debug)]
struct State {
    id: [u8; 20],
    table: RoutingTable,
    tokens: Tokens,
    rotated: Instant,
    store: PeerStore,
    items: ItemStore,
    /// Our address as other nodes report it (BEP 42 `ip`), with votes.
    seen_as: HashMap<IpAddr, u32>,
}

type Pending = Mutex<HashMap<[u8; 2], oneshot::Sender<(SocketAddr, Message)>>>;

#[derive(Debug)]
struct Shared {
    socket: Transport,
    state: Mutex<State>,
    pending: Pending,
    next_tid: AtomicU16,
    config: NativeConfig,
}

/// A native DHT node. Dropping the last handle stops it.
#[derive(Debug, Clone)]
pub struct NativeDht {
    shared: Arc<Shared>,
    _task: Arc<AbortOnDrop>,
}

#[derive(Debug)]
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    out.fill_with(|| fastrand::u8(..));
    out
}

type Answered = Vec<(Node, Option<Vec<u8>>, Ext)>;

impl NativeDht {
    /// Binds `addr` and starts answering queries, with a random node id
    /// and `timeout` per query.
    ///
    /// # Errors
    /// If the socket cannot be bound.
    pub async fn bind(addr: SocketAddr, timeout: Duration) -> io::Result<Self> {
        Self::bind_with(
            addr,
            NativeConfig {
                timeout,
                ..NativeConfig::default()
            },
        )
        .await
    }

    /// [`Self::bind`] with every setting.
    ///
    /// # Errors
    /// If the socket cannot be bound.
    pub async fn bind_with(addr: SocketAddr, config: NativeConfig) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr).await?;
        Ok(Self::start(
            Transport::Direct(socket),
            random_bytes(),
            config,
        ))
    }

    /// Like [`Self::bind`] with a chosen node id (for tests).
    ///
    /// # Errors
    /// If the socket cannot be bound.
    pub async fn with_id(addr: SocketAddr, id: [u8; 20], timeout: Duration) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr).await?;
        Ok(Self::start(
            Transport::Direct(socket),
            id,
            NativeConfig {
                timeout,
                ..NativeConfig::default()
            },
        ))
    }

    /// Starts a node whose datagrams go through `proxy`. Returns only
    /// after the SOCKS5 UDP ASSOCIATE has succeeded; until then (and if
    /// it fails) no UDP socket exists, so nothing is sent or accepted.
    ///
    /// # Errors
    /// If the proxy cannot be reached or refuses the association.
    pub async fn bind_relayed(proxy: &Socks5Proxy, config: NativeConfig) -> io::Result<Self> {
        let relay = UdpAssociate::open(proxy).await?;
        Ok(Self::start(
            Transport::Relayed(relay),
            random_bytes(),
            config,
        ))
    }

    /// A node on an already bound socket, without awaiting (must be
    /// called within a Tokio runtime).
    ///
    /// # Errors
    /// If the socket cannot be registered with the runtime.
    pub fn from_std(socket: std::net::UdpSocket, config: NativeConfig) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        Ok(Self::start(
            Transport::Direct(UdpSocket::from_std(socket)?),
            random_bytes(),
            config,
        ))
    }

    fn start(socket: Transport, id: [u8; 20], config: NativeConfig) -> Self {
        let shared = Arc::new(Shared {
            socket,
            state: Mutex::new(State {
                id,
                table: RoutingTable::new(id),
                tokens: Tokens::new(random_bytes()),
                rotated: Instant::now(),
                store: PeerStore::new(10_000, 200),
                items: ItemStore::new(10_000),
                seen_as: HashMap::new(),
            }),
            pending: Mutex::new(HashMap::new()),
            next_tid: AtomicU16::new(fastrand::u16(..)),
            config,
        });
        let task = tokio::spawn(receive(Arc::downgrade(&shared)));
        Self {
            shared,
            _task: Arc::new(AbortOnDrop(task)),
        }
    }

    /// Our node id (it changes once, if BEP 42 asks for a matching one).
    #[must_use]
    pub fn id(&self) -> [u8; 20] {
        self.shared.lock().id
    }

    /// Our address as most nodes report it (BEP 42 `ip`), if any did.
    #[must_use]
    pub fn external_ip(&self) -> Option<IpAddr> {
        self.shared.lock().external_ip()
    }

    /// The bound address.
    ///
    /// # Errors
    /// If the socket has no local address.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.shared.socket.local_addr()
    }

    /// Nodes in the routing table.
    #[must_use]
    pub fn table_len(&self) -> usize {
        self.shared.lock().table.len()
    }

    /// The addresses in the routing table, closest to our id first.
    #[must_use]
    pub fn nodes(&self) -> Vec<SocketAddr> {
        let state = self.shared.lock();
        state
            .table
            .closest(&state.id, usize::MAX)
            .into_iter()
            .map(|n| n.addr)
            .collect()
    }

    /// Joins the network through `nodes`: asks each for nodes close to
    /// our id, takes a BEP 42 id if the answers show ours does not match
    /// our address, then looks our id up. Returns the routing table size.
    pub async fn bootstrap(&self, nodes: &[SocketAddr]) -> usize {
        let contacts: Vec<Contact> = nodes.iter().copied().map(Contact::Addr).collect();
        self.bootstrap_contacts(&contacts).await
    }

    /// [`Self::bootstrap`] for addresses or, through a SOCKS5 relay,
    /// host names the proxy resolves (never the local resolver).
    pub async fn bootstrap_contacts(&self, nodes: &[Contact]) -> usize {
        let own = self.id();
        join_all(nodes.iter().cloned().map(|dest| {
            self.shared
                .query(dest, None, Query::FindNode { target: own }, Ext::default())
        }))
        .await;
        self.shared.adopt_secure_id();
        self.lookup(
            self.id(),
            Query::FindNode { target: self.id() },
            Ext::default(),
        )
        .await;
        self.table_len()
    }

    /// Finds peers for `info_hash`.
    pub async fn get_peers(&self, info_hash: [u8; 20]) -> Lookup {
        let answered = self
            .lookup(info_hash, Query::GetPeers { info_hash }, Ext::default())
            .await;
        Self::summarize(answered.0, answered.1)
    }

    fn summarize(peers: Vec<SocketAddr>, answered: Answered) -> Lookup {
        Lookup {
            peers,
            closest: answered
                .into_iter()
                .take(K)
                .map(|(n, token, _)| (n, token))
                .collect(),
        }
    }

    /// Looks `info_hash` up and announces us on `port` (or the source port
    /// of our datagrams with `None`), as a seed if `seed` (BEP 33), to the
    /// closest nodes that gave a token. Returns the lookup and how many
    /// nodes accepted.
    pub async fn announce_as(
        &self,
        info_hash: [u8; 20],
        port: Option<u16>,
        seed: bool,
    ) -> (Lookup, usize) {
        let lookup = self.get_peers(info_hash).await;
        let ext = Ext {
            seed,
            ..Ext::default()
        };
        let query = |token: Vec<u8>| Query::AnnouncePeer {
            info_hash,
            port: port.unwrap_or(0),
            token,
            implied_port: port.is_none(),
        };
        let accepted = self.send_to_closest(&lookup.closest, query, &ext).await;
        (lookup, accepted)
    }

    /// [`Self::announce_as`] for a downloader.
    pub async fn announce(&self, info_hash: [u8; 20], port: Option<u16>) -> (Lookup, usize) {
        self.announce_as(info_hash, port, false).await
    }

    async fn send_to_closest(
        &self,
        closest: &[(Node, Option<Vec<u8>>)],
        query: impl Fn(Vec<u8>) -> Query,
        ext: &Ext,
    ) -> usize {
        join_all(closest.iter().filter_map(|(node, token)| {
            Some(self.shared.query(
                Contact::Addr(node.addr),
                Some(node.id),
                query(token.clone()?),
                ext.clone(),
            ))
        }))
        .await
        .into_iter()
        .filter(|r| matches!(r, Some((_, Body::Response { .. }, _))))
        .count()
    }

    /// BEP 33: the merged seed and downloader filters of the nodes
    /// closest to `info_hash`.
    pub async fn scrape(&self, info_hash: [u8; 20]) -> Scrape {
        let ext = Ext {
            scrape: true,
            ..Ext::default()
        };
        let (_, answered) = self
            .lookup(info_hash, Query::GetPeers { info_hash }, ext)
            .await;
        let mut out = Scrape::default();
        for (_, _, ext) in answered.iter().take(K) {
            if let Some(b) = ext.bf_seeds.as_deref().and_then(Bloom::from_bytes) {
                out.seeds.merge(&b);
            }
            if let Some(b) = ext.bf_peers.as_deref().and_then(Bloom::from_bytes) {
                out.peers.merge(&b);
            }
        }
        out
    }

    /// BEP 51: info-hashes sampled from the nodes closest to `target`.
    pub async fn sample_infohashes(&self, target: [u8; 20]) -> Vec<[u8; 20]> {
        let (_, answered) = self
            .lookup(target, Query::SampleInfohashes { target }, Ext::default())
            .await;
        let mut out: Vec<[u8; 20]> = Vec::new();
        for (_, _, ext) in answered {
            for hash in ext.samples.unwrap_or_default() {
                if !out.contains(&hash) {
                    out.push(hash);
                }
            }
        }
        out
    }

    /// BEP 44: stores the bencoded `value` as an immutable item on the
    /// closest nodes. Returns its target and how many nodes stored it.
    pub async fn put_immutable(&self, value: &[u8]) -> ([u8; 20], usize) {
        let target = items::immutable_target(value);
        let ext = Ext {
            value: Some(value.to_vec()),
            ..Ext::default()
        };
        (target, self.put(target, ext).await)
    }

    /// BEP 44: signs and stores a mutable item. Returns its target and
    /// how many nodes stored it.
    pub async fn put_mutable(
        &self,
        key: &SigningKey,
        salt: &[u8],
        seq: i64,
        value: &[u8],
    ) -> ([u8; 20], usize) {
        let m = items::sign(key, salt, seq, value);
        let target = items::mutable_target(&m.key, salt);
        let ext = Ext {
            value: Some(value.to_vec()),
            key: Some(m.key),
            sig: Some(m.sig),
            seq: Some(seq),
            salt: (!salt.is_empty()).then(|| salt.to_vec()),
            ..Ext::default()
        };
        (target, self.put(target, ext).await)
    }

    async fn put(&self, target: [u8; 20], ext: Ext) -> usize {
        let (_, answered) = self
            .lookup(target, Query::Get { target }, Ext::default())
            .await;
        let closest: Vec<(Node, Option<Vec<u8>>)> = answered
            .into_iter()
            .take(K)
            .map(|(n, token, _)| (n, token))
            .collect();
        self.send_to_closest(&closest, |token| Query::Put { token }, &ext)
            .await
    }

    /// BEP 44: the immutable item stored under `target`, checked against
    /// its hash.
    pub async fn get_immutable(&self, target: [u8; 20]) -> Option<Vec<u8>> {
        let (_, answered) = self
            .lookup(target, Query::Get { target }, Ext::default())
            .await;
        answered
            .into_iter()
            .filter_map(|(_, _, ext)| ext.value)
            .find(|v| items::immutable_target(v) == target)
    }

    /// BEP 44: the newest validly signed value under `key` and `salt`,
    /// with its sequence number.
    pub async fn get_mutable(&self, key: &[u8; 32], salt: &[u8]) -> Option<(i64, Vec<u8>)> {
        let target = items::mutable_target(key, salt);
        let (_, answered) = self
            .lookup(target, Query::Get { target }, Ext::default())
            .await;
        answered
            .into_iter()
            .filter_map(|(_, _, ext)| {
                let m = Mutable {
                    key: ext.key?,
                    sig: ext.sig?,
                    seq: ext.seq?,
                    salt: salt.to_vec(),
                };
                let value = ext.value?;
                (m.key == *key && items::verify(&m, &value)).then_some((m.seq, value))
            })
            .max_by_key(|(seq, _)| *seq)
    }

    /// An iterative lookup towards `target` sending `query`; returns the
    /// peers seen and every node that answered, closest first.
    async fn lookup(
        &self,
        target: [u8; 20],
        query: Query,
        ext: Ext,
    ) -> (Vec<SocketAddr>, Answered) {
        let shared = &self.shared;
        let own = self.id();
        let mut candidates: Vec<Node> = shared.lock().table.closest(&target, K * 2);
        let mut queried: HashSet<[u8; 20]> = HashSet::new();
        let mut answered: Answered = Vec::new();
        let mut peers: Vec<SocketAddr> = Vec::new();
        for _ in 0..MAX_ROUNDS {
            candidates.sort_by_key(|n| distance(&n.id, &target));
            candidates.dedup_by_key(|n| n.id);
            let batch: Vec<Node> = candidates
                .iter()
                .take(K)
                .filter(|n| !queried.contains(&n.id))
                .take(ALPHA)
                .copied()
                .collect();
            if batch.is_empty() {
                break;
            }
            let replies = join_all(batch.iter().map(|n| {
                shared.query(
                    Contact::Addr(n.addr),
                    Some(n.id),
                    query.clone(),
                    ext.clone(),
                )
            }))
            .await;
            for (node, reply) in batch.into_iter().zip(replies) {
                queried.insert(node.id);
                let Some((
                    _,
                    Body::Response {
                        nodes,
                        values,
                        token,
                        ..
                    },
                    reply_ext,
                )) = reply
                else {
                    candidates.retain(|c| c.id != node.id);
                    continue;
                };
                answered.push((node, token, reply_ext));
                for value in values {
                    if !peers.contains(&value) {
                        peers.push(value);
                    }
                }
                candidates.extend(nodes.into_iter().filter(|n| {
                    n.id != own && n.addr.port() != 0 && shared.acceptable(&n.id, n.addr.ip())
                }));
            }
        }
        answered.sort_by_key(|(n, _, _)| distance(&n.id, &target));
        (peers, answered)
    }
}

impl State {
    fn external_ip(&self) -> Option<IpAddr> {
        self.seen_as
            .iter()
            .max_by_key(|(_, votes)| **votes)
            .map(|(ip, _)| *ip)
    }
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// BEP 42: whether `id` may stand for a node at `ip`.
    fn acceptable(&self, id: &[u8; 20], ip: IpAddr) -> bool {
        !self.config.enforce_bep42 || is_secure(id, ip)
    }

    /// Takes a BEP 42 id for the address nodes report, if ours does not
    /// match it, keeping the known nodes.
    fn adopt_secure_id(&self) {
        if !self.config.enforce_bep42 {
            return;
        }
        let mut state = self.lock();
        let Some(ip) = state.external_ip() else {
            return;
        };
        if is_secure(&state.id, ip) {
            return;
        }
        let id = secure_node_id(ip, random_bytes());
        let known = state.table.closest(&state.id, usize::MAX);
        let mut table = RoutingTable::new(id);
        let now = Instant::now().into_std();
        for node in known {
            table.insert(node, now);
        }
        state.id = id;
        state.table = table;
        tracing::debug!(%ip, "took a BEP 42 node id");
    }

    fn insert(&self, node: Node) {
        if self.acceptable(&node.id, node.addr.ip()) {
            self.lock().table.insert(node, Instant::now().into_std());
        }
    }

    /// Sends `query` to `addr` and waits for its answer. A response puts
    /// the node in the table; a timeout counts against `id` if known.
    async fn query(
        &self,
        dest: Contact,
        id: Option<[u8; 20]>,
        query: Query,
        mut ext: Ext,
    ) -> Option<(SocketAddr, Body, Ext)> {
        let tid = self.next_tid.fetch_add(1, Ordering::Relaxed).to_be_bytes();
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(tid, tx);
        ext.read_only = self.config.read_only;
        let message = Message {
            transaction: tid.to_vec(),
            body: Body::Query {
                id: self.lock().id,
                query,
            },
            ext,
        };
        let sent = self.socket.send_to(&message.encode(), &dest).await.is_ok();
        let reply = if sent {
            tokio::time::timeout(self.config.timeout, rx)
                .await
                .ok()
                .and_then(Result::ok)
        } else {
            None
        };
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&tid);
        match reply {
            Some((from, message)) => {
                if let Some(ip) = message.ext.ip {
                    *self.lock().seen_as.entry(ip.ip()).or_default() += 1;
                }
                if let Body::Response { id: their, .. } = &message.body {
                    self.insert(Node {
                        id: *their,
                        addr: from,
                    });
                }
                Some((from, message.body, message.ext))
            }
            None => {
                if let Some(id) = id {
                    self.lock().table.failed(&id);
                }
                None
            }
        }
    }

    /// The answer to a query from `from`.
    fn answer(&self, from: SocketAddr, querier: [u8; 20], query: Query, ext: &Ext) -> (Body, Ext) {
        // BEP 43: a read-only querier is answered but not remembered.
        if !ext.read_only {
            self.insert(Node {
                id: querier,
                addr: from,
            });
        }
        let now = Instant::now();
        let mut state = self.lock();
        if now.saturating_duration_since(state.rotated) >= TOKEN_ROTATION {
            state.tokens.rotate(random_bytes());
            state.rotated = now;
        }
        let id = state.id;
        let response = |nodes, values, token| Body::Response {
            id,
            nodes,
            values,
            token,
        };
        let error = |code, message: &str| Body::Error {
            code,
            message: message.to_owned(),
        };
        let mut out = Ext {
            ip: Some(from),
            ..Ext::default()
        };
        let body = match query {
            Query::Ping => response(Vec::new(), Vec::new(), None),
            Query::FindNode { target } => {
                response(state.table.closest(&target, K), Vec::new(), None)
            }
            Query::GetPeers { info_hash } => {
                let values = state.store.peers(&info_hash, now.into_std(), MAX_VALUES);
                if ext.scrape {
                    let (seeds, peers) = state.store.blooms(&info_hash, now.into_std());
                    out.bf_seeds = Some(seeds.as_bytes().to_vec());
                    out.bf_peers = Some(peers.as_bytes().to_vec());
                }
                let nodes = state.table.closest(&info_hash, K);
                response(nodes, values, Some(state.tokens.issue(from.ip())))
            }
            Query::AnnouncePeer {
                info_hash,
                port,
                token,
                implied_port,
            } => {
                let port = if implied_port { from.port() } else { port };
                if !state.tokens.check(&token, from.ip()) {
                    error(203, "bad token")
                } else if port == 0 {
                    error(203, "port 0")
                } else {
                    state.store.announce_as(
                        info_hash,
                        SocketAddr::new(from.ip(), port),
                        ext.seed,
                        now.into_std(),
                    );
                    response(Vec::new(), Vec::new(), None)
                }
            }
            Query::SampleInfohashes { target } => {
                out.samples = Some(state.store.sample(MAX_SAMPLES, fastrand::usize(..)));
                out.num = i64::try_from(state.store.len()).ok();
                out.interval = Some(SAMPLE_INTERVAL);
                response(state.table.closest(&target, K), Vec::new(), None)
            }
            Query::Get { target } => {
                if let Some(item) = state.items.get(&target, now.into_std()) {
                    out.value = Some(item.value.clone());
                    if let Some(m) = &item.mutable {
                        out.key = Some(m.key);
                        out.sig = Some(m.sig);
                        out.seq = Some(m.seq);
                    }
                }
                let nodes = state.table.closest(&target, K);
                response(nodes, Vec::new(), Some(state.tokens.issue(from.ip())))
            }
            Query::Put { token } => {
                if !state.tokens.check(&token, from.ip()) {
                    error(203, "bad token")
                } else if let Some(value) = ext.value.clone() {
                    let mutable = match (ext.key, ext.sig, ext.seq) {
                        (Some(key), Some(sig), Some(seq)) => Some(Mutable {
                            key,
                            sig,
                            seq,
                            salt: ext.salt.clone().unwrap_or_default(),
                        }),
                        _ => None,
                    };
                    match state
                        .items
                        .put(Item { value, mutable }, ext.cas, now.into_std())
                    {
                        Ok(_) => response(Vec::new(), Vec::new(), None),
                        Err(e) => error(e.code(), &e.to_string()),
                    }
                } else {
                    error(203, "no value")
                }
            }
        };
        (body, out)
    }
}

/// Reads datagrams until the node is dropped: answers queries, hands
/// responses and errors to the query waiting for them.
async fn receive(shared: std::sync::Weak<Shared>) {
    let mut buf = vec![0u8; 2048];
    loop {
        let Some(node) = shared.upgrade() else {
            return;
        };
        let Ok((n, from)) = node.socket.recv_from(&mut buf).await else {
            continue;
        };
        let Ok(message) = Message::decode(buf.get(..n).unwrap_or_default()) else {
            continue;
        };
        match message.body {
            Body::Query { id, query } => {
                if node.config.read_only {
                    continue;
                }
                let (body, ext) = node.answer(from, id, query, &message.ext);
                let reply = Message {
                    transaction: message.transaction,
                    body,
                    ext,
                };
                let _ = node
                    .socket
                    .send_to(&reply.encode(), &Contact::Addr(from))
                    .await;
            }
            Body::Response { .. } | Body::Error { .. } => {
                let Ok(tid) = <[u8; 2]>::try_from(message.transaction.as_slice()) else {
                    continue;
                };
                let waiter = node
                    .pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&tid);
                if let Some(waiter) = waiter {
                    let _ = waiter.send((from, message));
                }
            }
        }
    }
}
