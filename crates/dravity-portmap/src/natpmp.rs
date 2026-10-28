//! NAT-PMP (RFC 6886) messages.
//!
//! A client sends requests to UDP port 5351 of its default gateway:
//!
//! ```text
//! external address:  version(0) opcode(0)
//! map:               version(0) opcode(1 UDP | 2 TCP) reserved(2)
//!                    internal port(2) suggested external port(2) lifetime(4)
//! ```
//!
//! Responses echo `128 + opcode`, a result code and the gateway's epoch
//! (seconds since its mappings were last reset). A lifetime of 0 deletes a
//! mapping.

use std::net::Ipv4Addr;
use std::time::Duration;

/// The gateway's NAT-PMP port.
pub const SERVER_PORT: u16 = 5351;
const VERSION: u8 = 0;

/// Transport protocol of a mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protocol {
    /// UDP (uTP, DHT).
    Udp,
    /// TCP.
    Tcp,
}

impl Protocol {
    const fn opcode(self) -> u8 {
        match self {
            Self::Udp => 1,
            Self::Tcp => 2,
        }
    }
}

/// A request to the gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Ask for the gateway's public address.
    ExternalAddress,
    /// Create, renew or (with a zero lifetime) delete a mapping.
    Map {
        /// Protocol.
        protocol: Protocol,
        /// Our port.
        internal_port: u16,
        /// Preferred public port (0: any).
        external_port: u16,
        /// Requested lifetime in seconds.
        lifetime: u32,
    },
}

impl Request {
    /// Wire encoding.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        match *self {
            Self::ExternalAddress => vec![VERSION, 0],
            Self::Map {
                protocol,
                internal_port,
                external_port,
                lifetime,
            } => {
                let mut out = vec![VERSION, protocol.opcode(), 0, 0];
                out.extend_from_slice(&internal_port.to_be_bytes());
                out.extend_from_slice(&external_port.to_be_bytes());
                out.extend_from_slice(&lifetime.to_be_bytes());
                out
            }
        }
    }
}

/// A successful response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Response {
    /// The gateway's public address.
    ExternalAddress {
        /// Seconds since the gateway's mappings were reset.
        epoch: u32,
        /// Public IPv4 address.
        addr: Ipv4Addr,
    },
    /// A mapping was created, renewed or deleted.
    Map {
        /// Protocol.
        protocol: Protocol,
        /// Seconds since the gateway's mappings were reset.
        epoch: u32,
        /// Our port.
        internal_port: u16,
        /// Public port the gateway chose.
        external_port: u16,
        /// Granted lifetime.
        lifetime: Duration,
    },
}

/// A NAT-PMP failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum NatPmpError {
    /// Not a NAT-PMP response.
    #[error("malformed NAT-PMP response")]
    Malformed,
    /// The gateway speaks another version (PCP gateways answer 2).
    #[error("unsupported NAT-PMP version {0}")]
    Version(u8),
    /// The gateway refused (RFC 6886 result codes 1–5).
    #[error("NAT-PMP gateway refused: {}", describe(*.0))]
    Refused(u16),
}

const fn describe(code: u16) -> &'static str {
    match code {
        1 => "unsupported version",
        2 => "not authorized",
        3 => "network failure",
        4 => "out of resources",
        5 => "unsupported opcode",
        _ => "unknown result code",
    }
}

impl Response {
    /// Decodes a response datagram.
    ///
    /// # Errors
    /// [`NatPmpError`] for anything but a successful response.
    pub fn decode(data: &[u8]) -> Result<Self, NatPmpError> {
        let [version, op, r0, r1, e0, e1, e2, e3, rest @ ..] = data else {
            return Err(NatPmpError::Malformed);
        };
        if *version != VERSION {
            return Err(NatPmpError::Version(*version));
        }
        let result = u16::from_be_bytes([*r0, *r1]);
        if result != 0 {
            return Err(NatPmpError::Refused(result));
        }
        let epoch = u32::from_be_bytes([*e0, *e1, *e2, *e3]);
        match (op, rest) {
            (128, [a, b, c, d, ..]) => Ok(Self::ExternalAddress {
                epoch,
                addr: Ipv4Addr::new(*a, *b, *c, *d),
            }),
            (129 | 130, [i0, i1, x0, x1, l0, l1, l2, l3, ..]) => Ok(Self::Map {
                protocol: if *op == 129 {
                    Protocol::Udp
                } else {
                    Protocol::Tcp
                },
                epoch,
                internal_port: u16::from_be_bytes([*i0, *i1]),
                external_port: u16::from_be_bytes([*x0, *x1]),
                lifetime: Duration::from_secs(u64::from(u32::from_be_bytes([*l0, *l1, *l2, *l3]))),
            }),
            _ => Err(NatPmpError::Malformed),
        }
    }

    /// Whether this answers `request`.
    #[must_use]
    pub fn answers(&self, request: &Request) -> bool {
        match (self, request) {
            (Self::ExternalAddress { .. }, Request::ExternalAddress) => true,
            (
                Self::Map {
                    protocol,
                    internal_port,
                    ..
                },
                Request::Map {
                    protocol: p,
                    internal_port: i,
                    ..
                },
            ) => protocol == p && internal_port == i,
            _ => false,
        }
    }
}

/// Retransmission delays: 250 ms doubling (RFC 6886 §3.1), cut short
/// after four tries (3.75 s) since a missing gateway is the common case.
pub const RETRIES: [Duration; 4] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_follow_rfc_6886() {
        assert_eq!(Request::ExternalAddress.encode(), [0, 0]);
        let map = Request::Map {
            protocol: Protocol::Tcp,
            internal_port: 6881,
            external_port: 6881,
            lifetime: 7200,
        };
        assert_eq!(
            map.encode(),
            [0, 2, 0, 0, 0x1a, 0xe1, 0x1a, 0xe1, 0, 0, 0x1c, 0x20]
        );
    }

    #[test]
    fn responses_decode() {
        let ext = [0, 128, 0, 0, 0, 0, 0, 9, 203, 0, 113, 7];
        assert_eq!(
            Response::decode(&ext),
            Ok(Response::ExternalAddress {
                epoch: 9,
                addr: Ipv4Addr::new(203, 0, 113, 7)
            })
        );
        let map = [
            0, 129, 0, 0, 0, 0, 1, 0, 0x1a, 0xe1, 0x1a, 0xe2, 0, 0, 0x0e, 0x10,
        ];
        let decoded = Response::decode(&map).unwrap();
        assert_eq!(
            decoded,
            Response::Map {
                protocol: Protocol::Udp,
                epoch: 256,
                internal_port: 6881,
                external_port: 6882,
                lifetime: Duration::from_secs(3600)
            }
        );
        assert!(decoded.answers(&Request::Map {
            protocol: Protocol::Udp,
            internal_port: 6881,
            external_port: 0,
            lifetime: 1
        }));
        assert!(!decoded.answers(&Request::ExternalAddress));
    }

    #[test]
    fn failures_are_reported() {
        assert_eq!(
            Response::decode(&[0, 130, 0, 2, 0, 0, 0, 0]),
            Err(NatPmpError::Refused(2))
        );
        assert_eq!(
            Response::decode(&[2, 130, 0, 0, 0, 0, 0, 0]),
            Err(NatPmpError::Version(2))
        );
        assert_eq!(Response::decode(&[0, 128, 0]), Err(NatPmpError::Malformed));
        assert_eq!(
            Response::decode(&[0, 131, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4]),
            Err(NatPmpError::Malformed)
        );
        assert!(
            NatPmpError::Refused(3)
                .to_string()
                .contains("network failure")
        );
    }
}
