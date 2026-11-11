//! Dravity's own DHT node over one UDP socket: it answers the four BEP 5
//! queries from its [`RoutingTable`], [`Tokens`] and [`PeerStore`], and
//! runs iterative `find_node` / `get_peers` lookups and `announce_peer`.
//!
//! This is stage 3 of moving off `mainline` (irontide item 22): the
//! session still uses [`crate::Dht`]; `NativeDht` is exercised by local
//! multi-node tests until it has seen enough of the live network to
//! replace it.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::future::join_all;
use tokio::net::UdpSocket;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::krpc::{Body, Message, Node, Query};
use crate::routing::{K, RoutingTable, distance};
use crate::store::{PeerStore, TOKEN_ROTATION, Tokens};

/// Queries in flight at once during a lookup (BEP 5's alpha).
pub const ALPHA: usize = 3;
/// Rounds after which a lookup stops even if it still finds closer nodes.
const MAX_ROUNDS: usize = 32;
/// Peers returned per `get_peers` answer.
const MAX_VALUES: usize = 50;

/// What a `get_peers` lookup found.
#[derive(Debug, Clone, Default)]
pub struct Lookup {
    /// Peers for the info-hash, deduplicated.
    pub peers: Vec<SocketAddr>,
    /// The closest nodes that answered, closest first, with the token
    /// each gave (needed to announce to it).
    pub closest: Vec<(Node, Option<Vec<u8>>)>,
}

#[derive(Debug)]
struct State {
    table: RoutingTable,
    tokens: Tokens,
    rotated: Instant,
    store: PeerStore,
}

type Pending = Mutex<HashMap<[u8; 2], oneshot::Sender<(SocketAddr, Message)>>>;

#[derive(Debug)]
struct Shared {
    socket: UdpSocket,
    id: [u8; 20],
    state: Mutex<State>,
    pending: Pending,
    next_tid: AtomicU16,
    timeout: Duration,
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

impl NativeDht {
    /// Binds `addr` and starts answering queries, with a random node id
    /// and `timeout` per query.
    ///
    /// # Errors
    /// If the socket cannot be bound.
    pub async fn bind(addr: SocketAddr, timeout: Duration) -> io::Result<Self> {
        Self::with_id(addr, random_bytes(), timeout).await
    }

    /// Like [`Self::bind`] with a chosen node id (for BEP 42 ids, or tests).
    ///
    /// # Errors
    /// If the socket cannot be bound.
    pub async fn with_id(addr: SocketAddr, id: [u8; 20], timeout: Duration) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr).await?;
        Ok(Self::start(socket, id, timeout))
    }

    /// A node on an already bound socket, without awaiting (must be
    /// called within a Tokio runtime).
    ///
    /// # Errors
    /// If the socket cannot be registered with the runtime.
    pub fn from_std(socket: std::net::UdpSocket, timeout: Duration) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        Ok(Self::start(
            UdpSocket::from_std(socket)?,
            random_bytes(),
            timeout,
        ))
    }

    fn start(socket: UdpSocket, id: [u8; 20], timeout: Duration) -> Self {
        let shared = Arc::new(Shared {
            socket,
            id,
            state: Mutex::new(State {
                table: RoutingTable::new(id),
                tokens: Tokens::new(random_bytes()),
                rotated: Instant::now(),
                store: PeerStore::new(10_000, 200),
            }),
            pending: Mutex::new(HashMap::new()),
            next_tid: AtomicU16::new(fastrand::u16(..)),
            timeout,
        });
        let task = tokio::spawn(receive(Arc::downgrade(&shared)));
        Self {
            shared,
            _task: Arc::new(AbortOnDrop(task)),
        }
    }

    /// The addresses in the routing table, closest to our id first.
    #[must_use]
    pub fn nodes(&self) -> Vec<SocketAddr> {
        let state = self.shared.lock();
        state
            .table
            .closest(&self.shared.id, usize::MAX)
            .into_iter()
            .map(|n| n.addr)
            .collect()
    }

    /// Our node id.
    #[must_use]
    pub fn id(&self) -> [u8; 20] {
        self.shared.id
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

    /// Joins the network through `nodes`: asks each for nodes close to
    /// our id, then looks our id up. Returns the routing table size.
    pub async fn bootstrap(&self, nodes: &[SocketAddr]) -> usize {
        let own = self.shared.id;
        join_all(nodes.iter().map(|addr| {
            self.shared
                .query(*addr, None, Query::FindNode { target: own })
        }))
        .await;
        self.lookup(own, false).await;
        self.table_len()
    }

    /// Finds peers for `info_hash`.
    pub async fn get_peers(&self, info_hash: [u8; 20]) -> Lookup {
        self.lookup(info_hash, true).await
    }

    /// Looks `info_hash` up and announces us on `port` (or the source port
    /// of our datagrams with `None`) to the closest nodes that gave a
    /// token. Returns the lookup and how many nodes accepted.
    pub async fn announce(&self, info_hash: [u8; 20], port: Option<u16>) -> (Lookup, usize) {
        let lookup = self.lookup(info_hash, true).await;
        let accepted = join_all(lookup.closest.iter().filter_map(|(node, token)| {
            let token = token.clone()?;
            Some(self.shared.query(
                node.addr,
                Some(node.id),
                Query::AnnouncePeer {
                    info_hash,
                    port: port.unwrap_or(0),
                    token,
                    implied_port: port.is_none(),
                },
            ))
        }))
        .await
        .into_iter()
        .filter(|r| matches!(r, Some((_, Body::Response { .. }))))
        .count();
        (lookup, accepted)
    }

    async fn lookup(&self, target: [u8; 20], get_peers: bool) -> Lookup {
        let shared = &self.shared;
        let mut candidates: Vec<Node> = shared.lock().table.closest(&target, K * 2);
        let mut queried: HashSet<[u8; 20]> = HashSet::new();
        let mut answered: Vec<(Node, Option<Vec<u8>>)> = Vec::new();
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
            let query = if get_peers {
                Query::GetPeers { info_hash: target }
            } else {
                Query::FindNode { target }
            };
            let replies = join_all(
                batch
                    .iter()
                    .map(|n| shared.query(n.addr, Some(n.id), query.clone())),
            )
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
                )) = reply
                else {
                    candidates.retain(|c| c.id != node.id);
                    continue;
                };
                answered.push((node, token));
                for value in values {
                    if !peers.contains(&value) {
                        peers.push(value);
                    }
                }
                candidates.extend(
                    nodes
                        .into_iter()
                        .filter(|n| n.id != shared.id && n.addr.port() != 0),
                );
            }
        }
        answered.sort_by_key(|(n, _)| distance(&n.id, &target));
        answered.truncate(K);
        Lookup {
            peers,
            closest: answered,
        }
    }
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sends `query` to `addr` and waits for its answer. A response puts
    /// the node in the table; a timeout counts against `id` if known.
    async fn query(
        &self,
        addr: SocketAddr,
        id: Option<[u8; 20]>,
        query: Query,
    ) -> Option<(SocketAddr, Body)> {
        let tid = self.next_tid.fetch_add(1, Ordering::Relaxed).to_be_bytes();
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(tid, tx);
        let message = Message {
            transaction: tid.to_vec(),
            body: Body::Query { id: self.id, query },
        };
        let sent = self.socket.send_to(&message.encode(), addr).await.is_ok();
        let reply = if sent {
            tokio::time::timeout(self.timeout, rx)
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
                if let Body::Response { id: their, .. } = &message.body {
                    self.lock().table.insert(
                        Node {
                            id: *their,
                            addr: from,
                        },
                        Instant::now().into_std(),
                    );
                }
                Some((from, message.body))
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
    fn answer(&self, from: SocketAddr, querier: [u8; 20], query: Query) -> Body {
        let now = Instant::now();
        let mut state = self.lock();
        state.table.insert(
            Node {
                id: querier,
                addr: from,
            },
            now.into_std(),
        );
        if now.saturating_duration_since(state.rotated) >= TOKEN_ROTATION {
            state.tokens.rotate(random_bytes());
            state.rotated = now;
        }
        let response = |nodes, values, token| Body::Response {
            id: self.id,
            nodes,
            values,
            token,
        };
        match query {
            Query::Ping => response(Vec::new(), Vec::new(), None),
            Query::FindNode { target } => {
                response(state.table.closest(&target, K), Vec::new(), None)
            }
            Query::GetPeers { info_hash } => {
                let values = state.store.peers(&info_hash, now.into_std(), MAX_VALUES);
                let nodes = state.table.closest(&info_hash, K);
                response(nodes, values, Some(state.tokens.issue(from.ip())))
            }
            Query::AnnouncePeer {
                info_hash,
                port,
                token,
                implied_port,
            } => {
                if !state.tokens.check(&token, from.ip()) {
                    return Body::Error {
                        code: 203,
                        message: "bad token".to_owned(),
                    };
                }
                let port = if implied_port { from.port() } else { port };
                if port == 0 {
                    return Body::Error {
                        code: 203,
                        message: "port 0".to_owned(),
                    };
                }
                state
                    .store
                    .announce(info_hash, SocketAddr::new(from.ip(), port), now.into_std());
                response(Vec::new(), Vec::new(), None)
            }
        }
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
                let body = node.answer(from, id, query);
                let reply = Message {
                    transaction: message.transaction,
                    body,
                };
                let _ = node.socket.send_to(&reply.encode(), from).await;
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
