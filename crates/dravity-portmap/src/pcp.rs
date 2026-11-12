//! PCP (RFC 6887) MAP messages, the successor of NAT-PMP on the same UDP
//! port 5351.
//!
//! ```text
//! request header (24):  version(2) R=0|opcode(1 MAP) reserved(2)
//!                       lifetime(4) client IP(16)
//! response header (24): version(2) R=1|opcode reserved(1) result(1)
//!                       lifetime(4) epoch(4) reserved(12)
//! MAP body (36):        nonce(12) protocol(1) reserved(3)
//!                       internal port(2) external port(2) external IP(16)
//! ```
//!
//! Addresses are 16 bytes; IPv4 ones are IPv4-mapped (`::ffff:a.b.c.d`).
//! The nonce ties responses (and later renewals and deletes) to the
//! request. A NAT-PMP-only gateway answers a PCP request with a version 0
//! "unsupported version" packet, which [`decode`] reports as
//! [`PcpError::NatPmpOnly`] so the caller can fall back.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use crate::natpmp::Protocol;

/// PCP version.
pub const VERSION: u8 = 2;
/// The MAP opcode.
pub const OPCODE_MAP: u8 = 1;
/// Length of a MAP request or response.
pub const MAP_LEN: usize = 60;

/// A MAP request: create, renew or (lifetime 0) delete a mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MapRequest {
    /// Ties the response, renewals and the delete to this mapping.
    pub nonce: [u8; 12],
    /// Protocol.
    pub protocol: Protocol,
    /// Our port.
    pub internal_port: u16,
    /// Preferred public port (0: any).
    pub external_port: u16,
    /// Preferred public address (unspecified: any; IPv4 mappings use
    /// `0.0.0.0`).
    pub external_ip: IpAddr,
    /// Our address as the gateway sees it (the source of the request).
    pub client_ip: IpAddr,
    /// Requested lifetime in seconds.
    pub lifetime: u32,
}

impl MapRequest {
    /// Wire encoding ([`MAP_LEN`] bytes).
    ///
    /// ```
    /// use std::net::Ipv4Addr;
    /// use dravity_portmap::pcp::{MapRequest, MAP_LEN};
    /// use dravity_portmap::Protocol;
    ///
    /// let r = MapRequest {
    ///     nonce: [7; 12],
    ///     protocol: Protocol::Tcp,
    ///     internal_port: 6881,
    ///     external_port: 6881,
    ///     external_ip: Ipv4Addr::UNSPECIFIED.into(),
    ///     client_ip: Ipv4Addr::new(192, 168, 1, 5).into(),
    ///     lifetime: 7200,
    /// };
    /// let bytes = r.encode();
    /// assert_eq!(bytes.len(), MAP_LEN);
    /// assert_eq!(&bytes[..2], &[2, 1]);
    /// ```
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(MAP_LEN);
        out.extend_from_slice(&[VERSION, OPCODE_MAP, 0, 0]);
        out.extend_from_slice(&self.lifetime.to_be_bytes());
        out.extend_from_slice(&v6(self.client_ip).octets());
        out.extend_from_slice(&self.nonce);
        out.extend_from_slice(&[protocol_number(self.protocol), 0, 0, 0]);
        out.extend_from_slice(&self.internal_port.to_be_bytes());
        out.extend_from_slice(&self.external_port.to_be_bytes());
        out.extend_from_slice(&v6(self.external_ip).octets());
        out
    }
}

/// A successful MAP response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MapResponse {
    /// The request's nonce.
    pub nonce: [u8; 12],
    /// Protocol.
    pub protocol: Protocol,
    /// Our port.
    pub internal_port: u16,
    /// Public port the gateway assigned.
    pub external_port: u16,
    /// Public address the gateway assigned (IPv4 when mapped).
    pub external_ip: IpAddr,
    /// Granted lifetime.
    pub lifetime: Duration,
    /// Seconds since the gateway's state was reset.
    pub epoch: u32,
}

impl MapResponse {
    /// Whether this answers `request` (same nonce, protocol and port).
    #[must_use]
    pub fn answers(&self, request: &MapRequest) -> bool {
        self.nonce == request.nonce
            && self.protocol == request.protocol
            && self.internal_port == request.internal_port
    }
}

/// A PCP failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PcpError {
    /// Not a PCP MAP response.
    #[error("malformed PCP response")]
    Malformed,
    /// The gateway only speaks NAT-PMP (it answered with version 0).
    #[error("the gateway speaks NAT-PMP, not PCP")]
    NatPmpOnly,
    /// The gateway refused (RFC 6887 result codes).
    #[error("PCP gateway refused: {}", describe(*.0))]
    Refused(u8),
}

const fn describe(code: u8) -> &'static str {
    match code {
        1 => "unsupported version",
        2 => "not authorized",
        3 => "malformed request",
        4 => "unsupported opcode",
        5 => "unsupported option",
        6 => "malformed option",
        7 => "network failure",
        8 => "no resources",
        9 => "unsupported protocol",
        10 => "user exceeded quota",
        11 => "cannot provide external address",
        12 => "address mismatch",
        13 => "excessive remote peers",
        _ => "unknown result code",
    }
}

/// Decodes a MAP response datagram.
///
/// # Errors
/// [`PcpError`] for anything but a successful MAP response.
pub fn decode(data: &[u8]) -> Result<MapResponse, PcpError> {
    match data.first() {
        Some(0) => return Err(PcpError::NatPmpOnly),
        Some(&VERSION) => {}
        _ => return Err(PcpError::Malformed),
    }
    let bytes: &[u8; MAP_LEN] = data
        .get(..MAP_LEN)
        .and_then(|d| d.try_into().ok())
        .ok_or(PcpError::Malformed)?;
    let [_, op, _, result, l0, l1, l2, l3, e0, e1, e2, e3, rest @ ..] = bytes;
    if *op != 0x80 | OPCODE_MAP {
        return Err(PcpError::Malformed);
    }
    if *result != 0 {
        return Err(PcpError::Refused(*result));
    }
    let body = rest.get(12..).ok_or(PcpError::Malformed)?;
    let nonce: [u8; 12] = body
        .get(..12)
        .and_then(|n| n.try_into().ok())
        .ok_or(PcpError::Malformed)?;
    let protocol = match body.get(12) {
        Some(6) => Protocol::Tcp,
        Some(17) => Protocol::Udp,
        _ => return Err(PcpError::Malformed),
    };
    let port = |i: usize| -> Result<u16, PcpError> {
        body.get(i..i + 2)
            .and_then(|b| <[u8; 2]>::try_from(b).ok())
            .map(u16::from_be_bytes)
            .ok_or(PcpError::Malformed)
    };
    let ip: [u8; 16] = body
        .get(20..36)
        .and_then(|n| n.try_into().ok())
        .ok_or(PcpError::Malformed)?;
    let ip = Ipv6Addr::from(ip);
    Ok(MapResponse {
        nonce,
        protocol,
        internal_port: port(16)?,
        external_port: port(18)?,
        external_ip: ip.to_ipv4_mapped().map_or(IpAddr::V6(ip), IpAddr::V4),
        lifetime: Duration::from_secs(u64::from(u32::from_be_bytes([*l0, *l1, *l2, *l3]))),
        epoch: u32::from_be_bytes([*e0, *e1, *e2, *e3]),
    })
}

const fn protocol_number(p: Protocol) -> u8 {
    match p {
        Protocol::Tcp => 6,
        Protocol::Udp => 17,
    }
}

fn v6(ip: IpAddr) -> Ipv6Addr {
    match ip {
        IpAddr::V4(v4) => v4.to_ipv6_mapped(),
        IpAddr::V6(v6) => v6,
    }
}

/// The external address to suggest for a mapping of `client`: `0.0.0.0`
/// (mapped) for IPv4, `::` for IPv6.
#[must_use]
pub const fn any_external(client: IpAddr) -> IpAddr {
    match client {
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    fn request() -> MapRequest {
        MapRequest {
            nonce: [9; 12],
            protocol: Protocol::Udp,
            internal_port: 6881,
            external_port: 6881,
            external_ip: Ipv4Addr::UNSPECIFIED.into(),
            client_ip: Ipv4Addr::new(10, 0, 0, 2).into(),
            lifetime: 3600,
        }
    }

    /// The response a gateway builds from `req` (RFC 6887 §11.4).
    fn respond(req: &[u8], result: u8, port: u16, ip: Ipv6Addr) -> Vec<u8> {
        let mut out = vec![VERSION, 0x81, 0, result];
        out.extend_from_slice(&req[4..8]);
        out.extend_from_slice(&42u32.to_be_bytes());
        out.extend_from_slice(&[0; 12]);
        out.extend_from_slice(&req[24..42]);
        out.extend_from_slice(&port.to_be_bytes());
        out.extend_from_slice(&ip.octets());
        out
    }

    #[test]
    fn request_layout() {
        let b = request().encode();
        assert_eq!(b.len(), MAP_LEN);
        assert_eq!(&b[4..8], &3600u32.to_be_bytes());
        assert_eq!(
            &b[8..24],
            &Ipv4Addr::new(10, 0, 0, 2).to_ipv6_mapped().octets()
        );
        assert_eq!(&b[24..36], &[9; 12]);
        assert_eq!(b[36], 17);
        assert_eq!(&b[40..44], &[0x1a, 0xe1, 0x1a, 0xe1]);
        assert_eq!(&b[44..60], &Ipv4Addr::UNSPECIFIED.to_ipv6_mapped().octets());
    }

    #[test]
    fn responses() {
        let req = request();
        let wire = req.encode();
        let public = Ipv4Addr::new(203, 0, 113, 7).to_ipv6_mapped();
        let ok = decode(&respond(&wire, 0, 7000, public)).unwrap();
        assert!(ok.answers(&req));
        assert_eq!(ok.external_port, 7000);
        assert_eq!(ok.external_ip, IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)));
        assert_eq!(ok.lifetime, Duration::from_secs(3600));
        assert_eq!(ok.epoch, 42);
        let mut other = req;
        other.nonce = [1; 12];
        assert!(!ok.answers(&other));
        let v6 = decode(&respond(&wire, 0, 7000, "2001:db8::5".parse().unwrap())).unwrap();
        assert_eq!(v6.external_ip, "2001:db8::5".parse::<IpAddr>().unwrap());
        assert_eq!(
            decode(&respond(&wire, 8, 0, public)),
            Err(PcpError::Refused(8))
        );
        assert_eq!(decode(&[0, 129, 0, 1]), Err(PcpError::NatPmpOnly));
        assert_eq!(decode(&wire), Err(PcpError::Malformed), "a request");
        assert_eq!(decode(&wire[..30]), Err(PcpError::Malformed));
        assert_eq!(decode(&[]), Err(PcpError::Malformed));
        assert!(PcpError::Refused(2).to_string().contains("not authorized"));
    }

    #[test]
    fn never_panics_on_short_or_odd_input() {
        let wire = request().encode();
        for n in 0..=wire.len() {
            let _ = decode(&wire[..n]);
        }
        let mut odd = respond(&wire, 0, 1, Ipv6Addr::UNSPECIFIED);
        odd[36] = 99;
        assert_eq!(decode(&odd), Err(PcpError::Malformed));
    }
}
