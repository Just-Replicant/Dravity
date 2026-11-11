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
                    _ => return Err(KrpcError::UnknownMethod),
                };
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
        Ok(Self { transaction, body })
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
                };
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
        Value::Dict(d).to_vec()
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
            round(&Message {
                transaction: b"xy".to_vec(),
                body: Body::Query { id, query },
            });
        }
        round(&Message {
            transaction: b"r1".to_vec(),
            body: Body::Response {
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
        });
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
        let m = Message {
            transaction: b"t".to_vec(),
            body: Body::Response {
                id: [0; 20],
                nodes: Vec::new(),
                values: Vec::new(),
                token: None,
            },
        };
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
