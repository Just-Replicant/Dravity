//! KRPC (BEP 5) messages, sans-IO: the four DHT queries, their responses
//! and errors, with compact node lists for IPv4 (`nodes`) and IPv6
//! (`nodes6`, BEP 32).

use std::borrow::Cow;
use std::net::{IpAddr, SocketAddr};

use dravity_bencode::{Dict, Value};

/// A node: its id and UDP address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Node {
    /// Node id.
    pub id: [u8; 20],
    /// UDP address.
    pub addr: SocketAddr,
}

/// A query (`y = q`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    /// `ping`.
    Ping,
    /// `find_node` for `target`.
    FindNode {
        /// Wanted id.
        target: [u8; 20],
    },
    /// `get_peers` for an info-hash.
    GetPeers {
        /// The info-hash.
        info_hash: [u8; 20],
    },
    /// `announce_peer`.
    AnnouncePeer {
        /// The info-hash.
        info_hash: [u8; 20],
        /// Our port (ignored with `implied_port`).
        port: u16,
        /// The token from `get_peers`.
        token: Vec<u8>,
        /// Use the source port of the datagram instead of `port`.
        implied_port: bool,
    },
    /// `sample_infohashes` (BEP 51) near `target`.
    SampleInfohashes {
        /// Where to look.
        target: [u8; 20],
    },
    /// `get` (BEP 44) of the item stored under `target`.
    Get {
        /// SHA-1 of the value (immutable) or of key and salt (mutable).
        target: [u8; 20],
    },
    /// `put` (BEP 44): the item is in [`Ext::value`] (and, mutable,
    /// [`Ext::key`], [`Ext::sig`], [`Ext::seq`], [`Ext::salt`],
    /// [`Ext::cas`]).
    Put {
        /// The token from `get`.
        token: Vec<u8>,
    },
}

/// The extension fields of BEPs 33, 42, 43, 44 and 51, wherever they sit
/// in the message (top level, `a` or `r`). All empty by default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Ext {
    /// `ro` (BEP 43): the sender is read-only; do not add it to routing
    /// tables or query it.
    pub read_only: bool,
    /// `ip` (BEP 42): the address the responder saw us at.
    pub ip: Option<SocketAddr>,
    /// `scrape` in `get_peers` (BEP 33): also return the bloom filters.
    pub scrape: bool,
    /// `seed` in `announce_peer` (BEP 33): the announcer is a seed.
    pub seed: bool,
    /// `BFsd` (BEP 33): the 256-byte bloom filter of seeds.
    pub bf_seeds: Option<Vec<u8>>,
    /// `BFpe` (BEP 33): the 256-byte bloom filter of downloaders.
    pub bf_peers: Option<Vec<u8>>,
    /// `samples` (BEP 51): sampled info-hashes.
    pub samples: Option<Vec<[u8; 20]>>,
    /// `num` (BEP 51): info-hashes the responder stores.
    pub num: Option<i64>,
    /// `interval` (BEP 51): seconds before sampling this node again.
    pub interval: Option<i64>,
    /// `v` (BEP 44): the bencoded value, verbatim.
    pub value: Option<Vec<u8>>,
    /// `k` (BEP 44): the ed25519 public key of a mutable item.
    pub key: Option<[u8; 32]>,
    /// `sig` (BEP 44): its signature.
    pub sig: Option<[u8; 64]>,
    /// `seq` (BEP 44): its sequence number.
    pub seq: Option<i64>,
    /// `salt` (BEP 44).
    pub salt: Option<Vec<u8>>,
    /// `cas` (BEP 44): only replace this sequence number.
    pub cas: Option<i64>,
}

/// The body of a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// A query from node `id`.
    Query {
        /// Sender id.
        id: [u8; 20],
        /// The query.
        query: Query,
    },
    /// A response from node `id`.
    Response {
        /// Sender id.
        id: [u8; 20],
        /// Closer nodes (`nodes` and `nodes6`).
        nodes: Vec<Node>,
        /// Peers (`values`).
        values: Vec<SocketAddr>,
        /// Write token (`token`).
        token: Option<Vec<u8>>,
    },
    /// An error (`y = e`).
    Error {
        /// Error code (201 generic, 202 server, 203 protocol, 204 method).
        code: i64,
        /// Message.
        message: String,
    },
}

/// A KRPC message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Transaction id (`t`), echoed by the response.
    pub transaction: Vec<u8>,
    /// What it says.
    pub body: Body,
    /// Extension fields (BEPs 33, 42, 43, 44, 51).
    pub ext: Ext,
}

impl Message {
    /// A message with no extension fields.
    #[must_use]
    pub fn new(transaction: Vec<u8>, body: Body) -> Self {
        Self {
            transaction,
            body,
            ext: Ext::default(),
        }
    }
}

/// Why a datagram is not a KRPC message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum KrpcError {
    /// Not bencode, or not a dictionary.
    #[error("not a bencoded KRPC dictionary")]
    NotADict,
    /// A required field is missing or has the wrong type.
    #[error("bad or missing KRPC field {0:?}")]
    Field(&'static str),
    /// A query method this codec does not know.
    #[error("unknown KRPC method")]
    UnknownMethod,
}

/// Most nodes or peers kept from one response.
pub const MAX_ENTRIES: usize = 100;

impl Message {
    /// Decodes a datagram.
    ///
    /// # Errors
    /// [`KrpcError`] when it is not a well-formed message.
    pub fn decode(data: &[u8]) -> Result<Self, KrpcError> {
        let value = Value::decode(data).map_err(|_| KrpcError::NotADict)?;
        let dict = value.as_dict().ok_or(KrpcError::NotADict)?;
        let transaction = bytes(dict, "t")?.to_vec();
        let mut ext = Ext {
            read_only: dict
                .get(b"ro".as_slice())
                .and_then(Value::as_int)
                .is_some_and(|v| v != 0),
            ip: dict
                .get(b"ip".as_slice())
                .and_then(Value::as_bytes)
                .and_then(compact_peer),
            ..Ext::default()
        };
        let body = match bytes(dict, "y")? {
            b"q" => {
                let args = dict
                    .get(b"a".as_slice())
                    .and_then(Value::as_dict)
                    .ok_or(KrpcError::Field("a"))?;
                let id = id20(args, "id")?;
                let query = match bytes(dict, "q")? {
                    b"ping" => Query::Ping,
                    b"find_node" => Query::FindNode {
                        target: id20(args, "target")?,
                    },
                    b"get_peers" => Query::GetPeers {
                        info_hash: id20(args, "info_hash")?,
                    },
                    b"announce_peer" => Query::AnnouncePeer {
                        info_hash: id20(args, "info_hash")?,
                        port: args
                            .get(b"port".as_slice())
                            .and_then(Value::as_int)
                            .and_then(|p| u16::try_from(p).ok())
                            .ok_or(KrpcError::Field("port"))?,
                        token: bytes(args, "token")?.to_vec(),
                        implied_port: args
                            .get(b"implied_port".as_slice())
                            .and_then(Value::as_int)
                            .is_some_and(|v| v != 0),
                    },
                    b"sample_infohashes" => Query::SampleInfohashes {
                        target: id20(args, "target")?,
                    },
                    b"get" => Query::Get {
                        target: id20(args, "target")?,
                    },
                    b"put" => Query::Put {
                        token: bytes(args, "token")?.to_vec(),
                    },
                    _ => return Err(KrpcError::UnknownMethod),
                };
                read_ext(args, &mut ext);
                Body::Query { id, query }
            }
            b"r" => {
                let r = dict
                    .get(b"r".as_slice())
                    .and_then(Value::as_dict)
                    .ok_or(KrpcError::Field("r"))?;
                let mut nodes = Vec::new();
                for (key, len) in [("nodes", 26), ("nodes6", 38)] {
                    if let Ok(raw) = bytes(r, key) {
                        nodes.extend(
                            raw.chunks_exact(len)
                                .filter_map(compact_node)
                                .take(MAX_ENTRIES),
                        );
                    }
                }
                nodes.truncate(MAX_ENTRIES);
                let values = r
                    .get(b"values".as_slice())
                    .and_then(Value::as_list)
                    .map(|list| {
                        list.iter()
                            .filter_map(Value::as_bytes)
                            .filter_map(compact_peer)
                            .take(MAX_ENTRIES)
                            .collect()
                    })
                    .unwrap_or_default();
                read_ext(r, &mut ext);
                Body::Response {
                    id: id20(r, "id")?,
                    nodes,
                    values,
                    token: bytes(r, "token").ok().map(<[u8]>::to_vec),
                }
            }
            b"e" => {
                let list = dict
                    .get(b"e".as_slice())
                    .and_then(Value::as_list)
                    .ok_or(KrpcError::Field("e"))?;
                Body::Error {
                    code: list
                        .first()
                        .and_then(Value::as_int)
                        .ok_or(KrpcError::Field("e"))?,
                    message: list
                        .get(1)
                        .and_then(Value::as_bytes)
                        .map(|b| String::from_utf8_lossy(b).into_owned())
                        .unwrap_or_default(),
                }
            }
            _ => return Err(KrpcError::Field("y")),
        };
        Ok(Self {
            transaction,
            body,
            ext,
        })
    }

    /// Encodes the message.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut d = Dict::new();
        d.insert(key("t"), bin(&self.transaction));
        match &self.body {
            Body::Query { id, query } => {
                let mut a = Dict::new();
                a.insert(key("id"), bin(id));
                let method = match query {
                    Query::Ping => "ping",
                    Query::FindNode { target } => {
                        a.insert(key("target"), bin(target));
                        "find_node"
                    }
                    Query::GetPeers { info_hash } => {
                        a.insert(key("info_hash"), bin(info_hash));
                        "get_peers"
                    }
                    Query::AnnouncePeer {
                        info_hash,
                        port,
                        token,
                        implied_port,
                    } => {
                        a.insert(key("info_hash"), bin(info_hash));
                        a.insert(key("port"), Value::Int(i64::from(*port)));
                        a.insert(key("token"), bin(token));
                        if *implied_port {
                            a.insert(key("implied_port"), Value::Int(1));
                        }
                        "announce_peer"
                    }
                    Query::SampleInfohashes { target } => {
                        a.insert(key("target"), bin(target));
                        "sample_infohashes"
                    }
                    Query::Get { target } => {
                        a.insert(key("target"), bin(target));
                        "get"
                    }
                    Query::Put { token } => {
                        a.insert(key("token"), bin(token));
                        "put"
                    }
                };
                write_ext(&mut a, &self.ext);
                d.insert(key("y"), bin(b"q"));
                d.insert(key("q"), bin(method.as_bytes()));
                d.insert(key("a"), Value::Dict(a));
            }
            Body::Response {
                id,
                nodes,
                values,
                token,
            } => {
                let mut r = Dict::new();
                r.insert(key("id"), bin(id));
                let (mut v4, mut v6) = (Vec::new(), Vec::new());
                for n in nodes {
                    let out = if n.addr.is_ipv4() { &mut v4 } else { &mut v6 };
                    out.extend_from_slice(&n.id);
                    out.extend_from_slice(&peer_bytes(n.addr));
                }
                if !v4.is_empty() {
                    r.insert(key("nodes"), bin(&v4));
                }
                if !v6.is_empty() {
                    r.insert(key("nodes6"), bin(&v6));
                }
                if !values.is_empty() {
                    let list = values.iter().map(|p| bin(&peer_bytes(*p))).collect();
                    r.insert(key("values"), Value::List(list));
                }
                if let Some(token) = token {
                    r.insert(key("token"), bin(token));
                }
                write_ext(&mut r, &self.ext);
                d.insert(key("y"), bin(b"r"));
                d.insert(key("r"), Value::Dict(r));
            }
            Body::Error { code, message } => {
                d.insert(key("y"), bin(b"e"));
                d.insert(
                    key("e"),
                    Value::List(vec![Value::Int(*code), bin(message.as_bytes())]),
                );
            }
        }
        if self.ext.read_only {
            d.insert(key("ro"), Value::Int(1));
        }
        if let Some(ip) = self.ext.ip {
            d.insert(key("ip"), bin(&peer_bytes(ip)));
        }
        Value::Dict(d).to_vec()
    }
}

/// Reads the extension fields of an `a` or `r` dictionary.
fn read_ext(dict: &Dict<'_>, ext: &mut Ext) {
    let int = |k: &str| dict.get(k.as_bytes()).and_then(Value::as_int);
    let raw = |k: &str| dict.get(k.as_bytes()).and_then(Value::as_bytes);
    ext.scrape = int("scrape").is_some_and(|v| v != 0);
    ext.seed = int("seed").is_some_and(|v| v != 0);
    ext.bf_seeds = raw("BFsd").filter(|b| b.len() == 256).map(<[u8]>::to_vec);
    ext.bf_peers = raw("BFpe").filter(|b| b.len() == 256).map(<[u8]>::to_vec);
    ext.samples = raw("samples").map(|b| {
        b.as_chunks::<20>()
            .0
            .iter()
            .take(MAX_ENTRIES)
            .copied()
            .collect()
    });
    ext.num = int("num");
    ext.interval = int("interval");
    ext.value = dict.get(b"v".as_slice()).map(Value::to_vec);
    ext.key = raw("k").and_then(|b| b.try_into().ok());
    ext.sig = raw("sig").and_then(|b| b.try_into().ok());
    ext.seq = int("seq");
    ext.salt = raw("salt").map(<[u8]>::to_vec);
    ext.cas = int("cas");
}

/// Writes the extension fields that belong in an `a` or `r` dictionary.
fn write_ext<'a>(dict: &mut Dict<'a>, ext: &'a Ext) {
    if ext.scrape {
        dict.insert(key("scrape"), Value::Int(1));
    }
    if ext.seed {
        dict.insert(key("seed"), Value::Int(1));
    }
    if let Some(bf) = &ext.bf_seeds {
        dict.insert(key("BFsd"), bin(bf));
    }
    if let Some(bf) = &ext.bf_peers {
        dict.insert(key("BFpe"), bin(bf));
    }
    if let Some(samples) = &ext.samples {
        dict.insert(key("samples"), bin(&samples.concat()));
    }
    for (k, v) in [
        ("num", ext.num),
        ("interval", ext.interval),
        ("seq", ext.seq),
        ("cas", ext.cas),
    ] {
        if let Some(v) = v {
            dict.insert(key(k), Value::Int(v));
        }
    }
    if let Some(value) = ext.value.as_deref().and_then(|v| Value::decode(v).ok()) {
        dict.insert(key("v"), value);
    }
    if let Some(k) = &ext.key {
        dict.insert(key("k"), bin(k));
    }
    if let Some(sig) = &ext.sig {
        dict.insert(key("sig"), bin(sig));
    }
    if let Some(salt) = &ext.salt {
        dict.insert(key("salt"), bin(salt));
    }
}

fn key(k: &'static str) -> Cow<'static, [u8]> {
    Cow::Borrowed(k.as_bytes())
}

fn bin(b: &[u8]) -> Value<'static> {
    Value::Bytes(Cow::Owned(b.to_vec()))
}

fn bytes<'a>(dict: &'a Dict<'_>, k: &'static str) -> Result<&'a [u8], KrpcError> {
    dict.get(k.as_bytes())
        .and_then(Value::as_bytes)
        .ok_or(KrpcError::Field(k))
}

fn id20(dict: &Dict<'_>, k: &'static str) -> Result<[u8; 20], KrpcError> {
    bytes(dict, k)?.try_into().map_err(|_| KrpcError::Field(k))
}

/// Compact form: 4 or 16 address bytes and the big-endian port.
fn peer_bytes(addr: SocketAddr) -> Vec<u8> {
    let mut out = match addr.ip() {
        IpAddr::V4(v4) => v4.octets().to_vec(),
        IpAddr::V6(v6) => v6.octets().to_vec(),
    };
    out.extend_from_slice(&addr.port().to_be_bytes());
    out
}

fn compact_peer(raw: &[u8]) -> Option<SocketAddr> {
    let (ip, port): (IpAddr, &[u8]) = match raw.len() {
        6 => (
            IpAddr::from(<[u8; 4]>::try_from(raw.get(..4)?).ok()?),
            raw.get(4..)?,
        ),
        18 => (
            IpAddr::from(<[u8; 16]>::try_from(raw.get(..16)?).ok()?),
            raw.get(16..)?,
        ),
        _ => return None,
    };
    let port = u16::from_be_bytes(port.try_into().ok()?);
    (port != 0).then_some(SocketAddr::new(ip, port))
}

fn compact_node(raw: &[u8]) -> Option<Node> {
    Some(Node {
        id: raw.get(..20)?.try_into().ok()?,
        addr: compact_peer(raw.get(20..)?)?,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn round(m: &Message) {
        assert_eq!(&Message::decode(&m.encode()).unwrap(), m);
    }

    #[test]
    fn bep5_examples_decode() {
        let ping =
            Message::decode(b"d1:ad2:id20:abcdefghij0123456789e1:q4:ping1:t2:aa1:y1:qe").unwrap();
        assert_eq!(ping.transaction, b"aa");
        assert_eq!(
            ping.body,
            Body::Query {
                id: *b"abcdefghij0123456789",
                query: Query::Ping
            }
        );
        let err = Message::decode(b"d1:eli201e23:A Generic Error Ocurrede1:t2:aa1:y1:ee").unwrap();
        assert_eq!(
            err.body,
            Body::Error {
                code: 201,
                message: "A Generic Error Ocurred".into()
            }
        );
        let announce = Message::decode(
            b"d1:ad2:id20:abcdefghij01234567899:info_hash20:mnopqrstuvwxyz123456\
              12:implied_porti1e4:porti6881e5:token8:aoeusnthe1:q13:announce_peer1:t2:aa1:y1:qe",
        )
        .unwrap();
        assert!(matches!(
            announce.body,
            Body::Query {
                query: Query::AnnouncePeer {
                    port: 6881,
                    implied_port: true,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn round_trips() {
        let id = [7; 20];
        for query in [
            Query::Ping,
            Query::FindNode { target: [1; 20] },
            Query::GetPeers { info_hash: [2; 20] },
            Query::AnnouncePeer {
                info_hash: [3; 20],
                port: 51413,
                token: b"tok".to_vec(),
                implied_port: false,
            },
        ] {
            round(&Message::new(b"xy".to_vec(), Body::Query { id, query }));
        }
        // The extension queries and fields of BEPs 33, 42, 43, 44 and 51.
        let ext = Ext {
            read_only: true,
            ip: Some("9.8.7.6:5432".parse().unwrap()),
            scrape: true,
            seed: true,
            bf_seeds: Some(vec![1; 256]),
            bf_peers: Some(vec![2; 256]),
            samples: Some(vec![[3; 20], [4; 20]]),
            num: Some(7),
            interval: Some(60),
            value: Some(b"d1:ai1ee".to_vec()),
            key: Some([5; 32]),
            sig: Some([6; 64]),
            seq: Some(3),
            salt: Some(b"salt".to_vec()),
            cas: Some(2),
        };
        for query in [
            Query::SampleInfohashes { target: [7; 20] },
            Query::Get { target: [8; 20] },
            Query::Put {
                token: b"tk".to_vec(),
            },
            Query::GetPeers { info_hash: [9; 20] },
        ] {
            round(&Message {
                transaction: b"e1".to_vec(),
                body: Body::Query { id, query },
                ext: ext.clone(),
            });
        }
        round(&Message {
            transaction: b"e2".to_vec(),
            body: Body::Response {
                id,
                nodes: Vec::new(),
                values: Vec::new(),
                token: None,
            },
            ext,
        });
        round(&Message::new(
            b"r1".to_vec(),
            Body::Response {
                id,
                nodes: vec![
                    Node {
                        id: [4; 20],
                        addr: "1.2.3.4:6881".parse().unwrap(),
                    },
                    Node {
                        id: [5; 20],
                        addr: "[2001:db8::9]:6882".parse().unwrap(),
                    },
                ],
                values: vec!["5.6.7.8:1000".parse().unwrap()],
                token: Some(b"t0k".to_vec()),
            },
        ));
    }

    #[test]
    fn hostile_input_is_refused_or_trimmed() {
        assert_eq!(Message::decode(b"le"), Err(KrpcError::NotADict));
        assert_eq!(
            Message::decode(b"d1:t2:aa1:y1:qe"),
            Err(KrpcError::Field("a"))
        );
        assert_eq!(
            Message::decode(b"d1:ad2:id3:abce1:q4:ping1:t2:aa1:y1:qe"),
            Err(KrpcError::Field("id"))
        );
        assert_eq!(
            Message::decode(b"d1:ad2:id20:abcdefghij0123456789e1:q3:foo1:t2:aa1:y1:qe"),
            Err(KrpcError::UnknownMethod)
        );
        // 300 nodes in, MAX_ENTRIES out; a port-0 node is dropped.
        let mut nodes = Vec::new();
        for i in 0..300u16 {
            nodes.extend_from_slice(&[9; 20]);
            nodes.extend_from_slice(&[10, 0, 0, 1]);
            nodes.extend_from_slice(&i.to_be_bytes());
        }
        let m = Message::new(
            b"t".to_vec(),
            Body::Response {
                id: [0; 20],
                nodes: Vec::new(),
                values: Vec::new(),
                token: None,
            },
        );
        let mut raw = m.encode();
        let tail = format!("5:nodes{}:", nodes.len());
        let at = raw.windows(2).position(|w| w == b"id").unwrap() + 2 + 23;
        let mut injected = raw[..at].to_vec();
        injected.extend_from_slice(tail.as_bytes());
        injected.extend_from_slice(&nodes);
        injected.extend_from_slice(&raw.split_off(at));
        let Body::Response { nodes, .. } = Message::decode(&injected).unwrap().body else {
            panic!()
        };
        assert_eq!(nodes.len(), MAX_ENTRIES);
        assert!(nodes.iter().all(|n| n.addr.port() != 0));
    }
}
