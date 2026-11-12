//! BEP 55 `ut_holepunch`: a peer connected to two NATed peers relays a
//! rendezvous so both dial each other over uTP at the same time.
//!
//! Wire format (binary, not bencoded): `msg_type: u8`, `addr_type: u8`
//! (0 IPv4, 1 IPv6), the address (4 or 16 bytes), `port: u16` and
//! `err_code: u32`, all big-endian.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// BEP 55 extension name.
pub const UT_HOLEPUNCH: &str = "ut_holepunch";

/// Why a relay could not arrange a holepunch (BEP 55 error codes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HolepunchError {
    /// The target is not in the relay's swarm.
    NoSuchPeer,
    /// The relay is not connected to the target.
    NotConnected,
    /// The target does not support `ut_holepunch`.
    NoSupport,
    /// The target is the sender itself.
    NoSelf,
    /// A code this version does not know.
    Other(u32),
}

impl HolepunchError {
    /// The wire code.
    #[must_use]
    pub const fn code(self) -> u32 {
        match self {
            Self::NoSuchPeer => 1,
            Self::NotConnected => 2,
            Self::NoSupport => 3,
            Self::NoSelf => 4,
            Self::Other(code) => code,
        }
    }

    /// From a wire code (0 is not an error).
    #[must_use]
    pub const fn from_code(code: u32) -> Option<Self> {
        match code {
            0 => None,
            1 => Some(Self::NoSuchPeer),
            2 => Some(Self::NotConnected),
            3 => Some(Self::NoSupport),
            4 => Some(Self::NoSelf),
            other => Some(Self::Other(other)),
        }
    }
}

/// A `ut_holepunch` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Holepunch {
    /// To a relay: please connect me with the peer at this address.
    Rendezvous(SocketAddr),
    /// From a relay: dial this peer now (it is dialing you too).
    Connect(SocketAddr),
    /// From a relay: the rendezvous for this address failed.
    Error(SocketAddr, HolepunchError),
}

impl Holepunch {
    /// The address the message is about.
    #[must_use]
    pub const fn addr(&self) -> SocketAddr {
        match *self {
            Self::Rendezvous(a) | Self::Connect(a) | Self::Error(a, _) => a,
        }
    }

    /// Encodes the payload.
    ///
    /// ```
    /// use dravity_wire::Holepunch;
    ///
    /// let m = Holepunch::Connect("1.2.3.4:6881".parse().unwrap());
    /// assert_eq!(m.encode(), [1, 0, 1, 2, 3, 4, 0x1a, 0xe1, 0, 0, 0, 0]);
    /// assert_eq!(Holepunch::decode(&m.encode()), Some(m));
    /// ```
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let (kind, err) = match *self {
            Self::Rendezvous(_) => (0, 0),
            Self::Connect(_) => (1, 0),
            Self::Error(_, e) => (2, e.code()),
        };
        let addr = self.addr();
        let mut out = Vec::with_capacity(24);
        out.push(kind);
        match addr.ip() {
            IpAddr::V4(ip) => {
                out.push(0);
                out.extend_from_slice(&ip.octets());
            }
            IpAddr::V6(ip) => {
                out.push(1);
                out.extend_from_slice(&ip.octets());
            }
        }
        out.extend_from_slice(&addr.port().to_be_bytes());
        out.extend_from_slice(&err.to_be_bytes());
        out
    }

    /// Decodes a payload; `None` for an unknown type, a bad length, or an
    /// error message with code 0.
    #[must_use]
    pub fn decode(payload: &[u8]) -> Option<Self> {
        let (&kind, rest) = payload.split_first()?;
        let (&addr_type, rest) = rest.split_first()?;
        let (ip, rest) = match addr_type {
            0 => {
                let (ip, rest) = rest.split_first_chunk::<4>()?;
                (IpAddr::V4(Ipv4Addr::from(*ip)), rest)
            }
            1 => {
                let (ip, rest) = rest.split_first_chunk::<16>()?;
                (IpAddr::V6(Ipv6Addr::from(*ip)), rest)
            }
            _ => return None,
        };
        let (port, rest) = rest.split_first_chunk::<2>()?;
        let err: [u8; 4] = rest.try_into().ok()?;
        let addr = SocketAddr::new(ip, u16::from_be_bytes(*port));
        let err = u32::from_be_bytes(err);
        match kind {
            0 => Some(Self::Rendezvous(addr)),
            1 => Some(Self::Connect(addr)),
            2 => HolepunchError::from_code(err).map(|e| Self::Error(addr, e)),
            _ => None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_refuses_junk() {
        let v6: SocketAddr = "[2001:db8::7]:51413".parse().unwrap();
        for m in [
            Holepunch::Rendezvous(v6),
            Holepunch::Connect("10.0.0.1:1".parse().unwrap()),
            Holepunch::Error(v6, HolepunchError::NoSupport),
            Holepunch::Error(v6, HolepunchError::Other(99)),
        ] {
            assert_eq!(Holepunch::decode(&m.encode()), Some(m));
        }
        assert_eq!(Holepunch::Rendezvous(v6).encode().len(), 24);
        let good = Holepunch::Connect("10.0.0.1:1".parse().unwrap()).encode();
        assert!(Holepunch::decode(&good[..good.len() - 1]).is_none());
        let mut extra = good.clone();
        extra.push(0);
        assert!(Holepunch::decode(&extra).is_none());
        let mut bad = good.clone();
        bad[0] = 3;
        assert!(Holepunch::decode(&bad).is_none());
        bad = good.clone();
        bad[1] = 2;
        assert!(Holepunch::decode(&bad).is_none());
        // An error message must carry a non-zero code.
        bad = good;
        bad[0] = 2;
        assert!(Holepunch::decode(&bad).is_none());
        assert!(Holepunch::decode(&[]).is_none());
    }
}
