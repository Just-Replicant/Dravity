//! BEP 40 canonical peer priority.
//!
//! Both ends of a connection compute the same number from the two
//! addresses, so a swarm agrees on which connections to keep when peers
//! are at their limits. The address bytes are masked (more bits kept the
//! closer the two addresses are), ordered, and hashed with CRC-32C.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};

/// CRC-32C (Castagnoli), as BEP 40 uses.
///
/// ```
/// assert_eq!(dravity_core::priority::crc32c(b"123456789"), 0xe306_9283);
/// ```
#[must_use]
pub fn crc32c(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let lsb = crc & 1;
            crc >>= 1;
            if lsb != 0 {
                crc ^= 0x82f6_3b78;
            }
        }
    }
    !crc
}

/// The BEP 40 priority of the connection between `a` and `b`; the same
/// whichever side computes it. Higher is kept first. An IPv4-mapped IPv6
/// address counts as IPv4; mixing an IPv4 and an IPv6 address compares
/// the IPv4 one in its mapped form.
///
/// ```
/// use std::net::SocketAddr;
/// use dravity_core::priority::peer_priority;
///
/// let a: SocketAddr = "123.213.32.10:6881".parse().unwrap();
/// let b: SocketAddr = "98.76.54.32:6881".parse().unwrap();
/// assert_eq!(peer_priority(a, b), 0xec2d_7224);
/// assert_eq!(peer_priority(b, a), 0xec2d_7224);
/// ```
#[must_use]
pub fn peer_priority(a: SocketAddr, b: SocketAddr) -> u32 {
    let (ia, ib) = (canonical(a.ip()), canonical(b.ip()));
    if ia == ib {
        // Same host: the ports decide.
        let (lo, hi) = order(a.port().to_be_bytes(), b.port().to_be_bytes());
        return crc32c(&[lo, hi].concat());
    }
    match (ia, ib) {
        (IpAddr::V4(x), IpAddr::V4(y)) => {
            let (x, y) = (x.octets(), y.octets());
            let mask: [u8; 4] = if x[..3] == y[..3] {
                [0xff; 4]
            } else if x[..2] == y[..2] {
                [0xff, 0xff, 0xff, 0x55]
            } else {
                [0xff, 0xff, 0x55, 0x55]
            };
            let (lo, hi) = order(masked(x, mask), masked(y, mask));
            crc32c(&[lo, hi].concat())
        }
        _ => {
            let (x, y) = (v6(ia).octets(), v6(ib).octets());
            // The /48 and /56 of the two addresses decide how much is kept.
            let keep = if x[..7] == y[..7] {
                8
            } else if x[..6] == y[..6] {
                7
            } else {
                6
            };
            let mut mask = [0x55u8; 16];
            for m in mask.iter_mut().take(keep) {
                *m = 0xff;
            }
            let (lo, hi) = order(masked(x, mask), masked(y, mask));
            crc32c(&[lo, hi].concat())
        }
    }
}

fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        IpAddr::V4(_) => ip,
    }
}

fn v6(ip: IpAddr) -> Ipv6Addr {
    match ip {
        IpAddr::V4(v4) => v4.to_ipv6_mapped(),
        IpAddr::V6(v6) => v6,
    }
}

fn masked<const N: usize>(mut bytes: [u8; N], mask: [u8; N]) -> [u8; N] {
    for (b, m) in bytes.iter_mut().zip(mask) {
        *b &= m;
    }
    bytes
}

fn order<const N: usize>(a: [u8; N], b: [u8; N]) -> ([u8; N], [u8; N]) {
    if a <= b { (a, b) } else { (b, a) }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::net::Ipv4Addr;

    use super::*;

    fn s(a: &str) -> SocketAddr {
        a.parse().unwrap()
    }

    #[test]
    fn bep40_vectors() {
        // The examples given in BEP 40.
        assert_eq!(
            peer_priority(s("123.213.32.10:0"), s("98.76.54.32:0")),
            0xec2d_7224
        );
        assert_eq!(
            peer_priority(s("123.213.32.10:0"), s("123.213.32.234:0")),
            0x9956_8189
        );
    }

    #[test]
    fn symmetric_and_port_sensitive_on_one_host() {
        let (a, b) = (s("10.1.2.3:6881"), s("10.1.2.3:51413"));
        assert_eq!(peer_priority(a, b), peer_priority(b, a));
        assert_ne!(peer_priority(a, b), peer_priority(a, s("10.1.2.3:51414")));
        let mapped = SocketAddr::new(
            IpAddr::V6(Ipv4Addr::new(98, 76, 54, 32).to_ipv6_mapped()),
            1,
        );
        assert_eq!(peer_priority(s("123.213.32.10:1"), mapped), 0xec2d_7224);
        let (x, y) = (s("[2001:db8::1]:1"), s("[2001:db8:1::2]:1"));
        assert_eq!(peer_priority(x, y), peer_priority(y, x));
    }
}
