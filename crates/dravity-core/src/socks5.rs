//! SOCKS5 (RFC 1928) and RFC 1929 message bytes, shared by the session's
//! peer proxy and the tracker client's UDP relay. Pure functions: the
//! callers own the sockets.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// The CONNECT command.
pub const CONNECT: u8 = 1;
/// The UDP ASSOCIATE command.
pub const UDP_ASSOCIATE: u8 = 3;

/// The greeting: offer "no auth", plus username/password if we have
/// credentials.
#[must_use]
pub fn greeting(with_auth: bool) -> Vec<u8> {
    if with_auth {
        vec![5, 2, 0, 2]
    } else {
        vec![5, 1, 0]
    }
}

/// The RFC 1929 username/password sub-negotiation, or `None` if either
/// is over 255 bytes.
#[must_use]
pub fn auth(user: &str, pass: &str) -> Option<Vec<u8>> {
    let ulen = u8::try_from(user.len()).ok()?;
    let plen = u8::try_from(pass.len()).ok()?;
    let mut out = vec![1, ulen];
    out.extend_from_slice(user.as_bytes());
    out.push(plen);
    out.extend_from_slice(pass.as_bytes());
    Some(out)
}

fn push_addr(out: &mut Vec<u8>, target: SocketAddr) {
    match target.ip() {
        IpAddr::V4(ip) => {
            out.push(1);
            out.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            out.push(4);
            out.extend_from_slice(&ip.octets());
        }
    }
    out.extend_from_slice(&target.port().to_be_bytes());
}

/// A request: [`CONNECT`] or [`UDP_ASSOCIATE`] for `target`.
#[must_use]
pub fn request(command: u8, target: SocketAddr) -> Vec<u8> {
    let mut out = vec![5, command, 0];
    push_addr(&mut out, target);
    out
}

/// How many bytes the reply whose first 5 bytes are `head` has in total,
/// or `None` if `head` is invalid.
#[must_use]
pub fn reply_len(head: &[u8; 5]) -> Option<usize> {
    let [5, _, 0, atyp, len] = *head else {
        return None;
    };
    match atyp {
        1 => Some(4 + 4 + 2),
        4 => Some(4 + 16 + 2),
        3 => Some(4 + 1 + usize::from(len) + 2),
        _ => None,
    }
}

/// The bound address of a complete reply with an IP address (the relay
/// of a UDP ASSOCIATE); `None` for a domain name or a malformed reply.
///
/// ```
/// use dravity_core::socks5;
///
/// let reply = [5, 0, 0, 1, 127, 0, 0, 1, 0x1f, 0x90];
/// assert_eq!(socks5::reply_addr(&reply), Some("127.0.0.1:8080".parse().unwrap()));
/// ```
#[must_use]
pub fn reply_addr(reply: &[u8]) -> Option<SocketAddr> {
    let (addr, rest) = split_addr(reply.get(3..)?)?;
    rest.is_empty().then_some(addr)
}

fn split_addr(data: &[u8]) -> Option<(SocketAddr, &[u8])> {
    match data {
        [1, a, b, c, d, p1, p2, rest @ ..] => Some((
            SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(*a, *b, *c, *d)),
                u16::from_be_bytes([*p1, *p2]),
            ),
            rest,
        )),
        [4, rest @ ..] => {
            let ip: [u8; 16] = rest.get(..16)?.try_into().ok()?;
            let port: [u8; 2] = rest.get(16..18)?.try_into().ok()?;
            Some((
                SocketAddr::new(IpAddr::from(ip), u16::from_be_bytes(port)),
                rest.get(18..)?,
            ))
        }
        _ => None,
    }
}

/// The header that prefixes a datagram sent through a UDP relay.
#[must_use]
pub fn udp_header(target: SocketAddr) -> Vec<u8> {
    let mut out = vec![0, 0, 0];
    push_addr(&mut out, target);
    out
}

/// The UDP relay header for a target named by host name (address type 3),
/// so the proxy resolves it; `None` for a name over 255 bytes.
///
/// ```
/// let h = dravity_core::socks5::udp_header_domain("t.example", 6969).unwrap();
/// assert_eq!(h, b"\0\0\0\x03\x09t.example\x1b\x39");
/// ```
#[must_use]
pub fn udp_header_domain(host: &str, port: u16) -> Option<Vec<u8>> {
    let len = u8::try_from(host.len()).ok()?;
    let mut out = vec![0, 0, 0, 3, len];
    out.extend_from_slice(host.as_bytes());
    out.extend_from_slice(&port.to_be_bytes());
    Some(out)
}

/// Splits a datagram received from a UDP relay into its source address
/// and payload. Fragmented and domain-addressed datagrams are refused
/// (`None`).
#[must_use]
pub fn parse_udp_header(datagram: &[u8]) -> Option<(SocketAddr, &[u8])> {
    match datagram {
        [0, 0, 0, rest @ ..] => split_addr(rest),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn messages_and_replies() {
        assert_eq!(greeting(false), [5, 1, 0]);
        assert_eq!(auth("u", "pw").unwrap(), [1, 1, b'u', 2, b'p', b'w']);
        assert!(auth(&"x".repeat(256), "").is_none());
        let target: SocketAddr = "[2001:db8::1]:6969".parse().unwrap();
        let req = request(UDP_ASSOCIATE, target);
        assert_eq!(req.get(..4), Some(&[5, 3, 0, 4][..]));
        assert_eq!(
            reply_addr(&[&[5, 0, 0][..], &req[3..]].concat()),
            Some(target)
        );
        assert_eq!(reply_len(&[5, 0, 0, 4, 0]), Some(22));
        assert_eq!(reply_len(&[4, 0, 0, 1, 0]), None);
        assert_eq!(reply_addr(&[5, 0, 0, 3, 1, b'a', 0, 80]), None);
        let mut datagram = udp_header(target);
        datagram.extend_from_slice(b"hi");
        assert_eq!(parse_udp_header(&datagram), Some((target, &b"hi"[..])));
        assert_eq!(parse_udp_header(&[0, 0, 1, 1, 0, 0, 0, 0, 0, 0]), None);
    }
}
