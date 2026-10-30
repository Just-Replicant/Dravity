//! Compact peer addresses (BEP 23 / BEP 7 / BEP 11): 6 bytes per IPv4
//! endpoint, 18 bytes per IPv6 endpoint, port in network byte order.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

/// Bytes per compact IPv4 endpoint.
pub const V4_LEN: usize = 6;
/// Bytes per compact IPv6 endpoint.
pub const V6_LEN: usize = 18;

/// A compact list whose length is not a multiple of the entry size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("compact peer list length {len} is not a multiple of {entry}")]
pub struct CompactError {
    /// Length of the input.
    pub len: usize,
    /// Expected entry size.
    pub entry: usize,
}

/// Parses compact IPv4 endpoints.
///
/// ```
/// use dravity_core::compact;
///
/// let peers = compact::parse_v4(&[127, 0, 0, 1, 0x1a, 0xe1]).unwrap();
/// assert_eq!(peers, ["127.0.0.1:6881".parse().unwrap()]);
/// assert!(compact::parse_v4(&[1, 2, 3]).is_err());
/// ```
///
/// # Errors
/// [`CompactError`] if the length is not a multiple of [`V4_LEN`].
pub fn parse_v4(bytes: &[u8]) -> Result<Vec<SocketAddr>, CompactError> {
    let (chunks, rest) = bytes.as_chunks::<V4_LEN>();
    if !rest.is_empty() {
        return Err(CompactError {
            len: bytes.len(),
            entry: V4_LEN,
        });
    }
    Ok(chunks
        .iter()
        .map(|&[a, b, c, d, p0, p1]| {
            SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(a, b, c, d),
                u16::from_be_bytes([p0, p1]),
            ))
        })
        .collect())
}

/// Parses compact IPv6 endpoints.
///
/// # Errors
/// [`CompactError`] if the length is not a multiple of [`V6_LEN`].
pub fn parse_v6(bytes: &[u8]) -> Result<Vec<SocketAddr>, CompactError> {
    let (chunks, rest) = bytes.as_chunks::<V6_LEN>();
    if !rest.is_empty() {
        return Err(CompactError {
            len: bytes.len(),
            entry: V6_LEN,
        });
    }
    Ok(chunks
        .iter()
        .filter_map(|c| {
            let (ip, port) = c.split_first_chunk::<16>()?;
            let port = port.first_chunk::<2>()?;
            Some(SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from(*ip),
                u16::from_be_bytes(*port),
                0,
                0,
            )))
        })
        .collect())
}

/// Appends `addr` in compact form to the IPv4 or IPv6 buffer.
pub fn push(addr: SocketAddr, v4: &mut Vec<u8>, v6: &mut Vec<u8>) {
    match addr {
        SocketAddr::V4(a) => {
            v4.extend_from_slice(&a.ip().octets());
            v4.extend_from_slice(&a.port().to_be_bytes());
        }
        SocketAddr::V6(a) => {
            v6.extend_from_slice(&a.ip().octets());
            v6.extend_from_slice(&a.port().to_be_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn rejects_partial_entries() {
        assert_eq!(parse_v4(&[0; 7]), Err(CompactError { len: 7, entry: 6 }));
        assert!(parse_v6(&[0; 17]).is_err());
        assert_eq!(parse_v4(&[]), Ok(vec![]));
    }

    proptest! {
        #[test]
        fn roundtrip(addrs in proptest::collection::vec(any::<SocketAddr>(), 0..20)) {
            let mut v4 = Vec::new();
            let mut v6 = Vec::new();
            let normalized: Vec<SocketAddr> = addrs
                .iter()
                .map(|a| match a {
                    SocketAddr::V6(v) => SocketAddr::V6(SocketAddrV6::new(*v.ip(), v.port(), 0, 0)),
                    v4 => *v4,
                })
                .collect();
            for a in &normalized {
                push(*a, &mut v4, &mut v6);
            }
            let mut parsed = parse_v4(&v4).unwrap();
            parsed.extend(parse_v6(&v6).unwrap());
            let mut expected: Vec<SocketAddr> = normalized.iter().copied().filter(SocketAddr::is_ipv4).collect();
            expected.extend(normalized.iter().copied().filter(SocketAddr::is_ipv6));
            prop_assert_eq!(parsed, expected);
        }
    }
}
