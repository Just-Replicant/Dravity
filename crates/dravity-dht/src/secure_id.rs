//! BEP 42 DHT security extension: node ids derived from the node's IP.
//!
//! The first 21 bits of a node id must match CRC-32C of the masked IP
//! with three random bits `r` mixed in; the last byte of the id carries a
//! random byte whose low three bits are `r`. A node that cannot pick its
//! id freely cannot place itself next to an info-hash to censor it.

use std::net::IpAddr;

use dravity_core::priority::crc32c;

const V4_MASK: [u8; 4] = [0x03, 0x0f, 0x3f, 0xff];
const V6_MASK: [u8; 8] = [0x01, 0x03, 0x07, 0x0f, 0x1f, 0x3f, 0x7f, 0xff];

/// The 32-bit CRC prefix BEP 42 binds to `ip` and random byte `rand`.
fn prefix(ip: IpAddr, rand: u8) -> u32 {
    let r = rand & 0x07;
    match ip {
        IpAddr::V4(v4) => {
            let mut b = v4.octets();
            for (x, m) in b.iter_mut().zip(V4_MASK) {
                *x &= m;
            }
            b[0] |= r << 5;
            crc32c(&b)
        }
        IpAddr::V6(v6) => {
            let mut b = [0u8; 8];
            b.copy_from_slice(&v6.octets()[..8]);
            for (x, m) in b.iter_mut().zip(V6_MASK) {
                *x &= m;
            }
            b[0] |= r << 5;
            crc32c(&b)
        }
    }
}

/// A node id for `ip` (BEP 42): `random` fills the free bits and its last
/// byte becomes the id's last byte.
///
/// ```
/// use dravity_dht::secure_id::{is_secure, secure_node_id};
///
/// let ip = "124.31.75.21".parse().unwrap();
/// let id = secure_node_id(ip, [1; 20]);
/// assert!(is_secure(&id, ip));
/// ```
#[must_use]
pub fn secure_node_id(ip: IpAddr, random: [u8; 20]) -> [u8; 20] {
    let mut id = random;
    let rand = random[19];
    let crc = prefix(ip, rand).to_be_bytes();
    id[0] = crc[0];
    id[1] = crc[1];
    id[2] = (crc[2] & 0xf8) | (random[2] & 0x07);
    id[19] = rand;
    id
}

/// Whether `id` is a valid BEP 42 id for `ip`. Local and private
/// addresses are exempt (BEP 42), so they always pass.
#[must_use]
pub fn is_secure(id: &[u8; 20], ip: IpAddr) -> bool {
    if is_exempt(ip) {
        return true;
    }
    let crc = prefix(ip, id[19]).to_be_bytes();
    id[0] == crc[0] && id[1] == crc[1] && (id[2] & 0xf8) == (crc[2] & 0xf8)
}

fn is_exempt(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unicast_link_local() || v6.is_unique_local(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    /// The BEP 42 examples: (IP, rand, id); only bits 0..21 and the last
    /// byte are determined.
    const VECTORS: [(&str, u8, &str); 5] = [
        (
            "124.31.75.21",
            1,
            "5fbfbff10c5d6a4ec8a88e4c6ab4c28b95eee401",
        ),
        (
            "21.75.31.124",
            86,
            "5a3ce9c14e7a08645677bbd1cfe7d8f956d53256",
        ),
        (
            "65.23.51.170",
            22,
            "a5d43220bc8f112a3d426c84764f8c2a1150e616",
        ),
        (
            "84.124.73.14",
            65,
            "1b0321dd1bb1fe518101ceef99462b947a01ff41",
        ),
        (
            "43.213.53.83",
            90,
            "e56f6cbf5b7c4be0237986d5243b87aa6d51305a",
        ),
    ];

    fn hex(s: &str) -> [u8; 20] {
        let mut out = [0u8; 20];
        for (i, o) in out.iter_mut().enumerate() {
            *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
        }
        out
    }

    #[test]
    fn bep42_vectors() {
        for (ip, rand, id) in VECTORS {
            let ip: IpAddr = ip.parse().unwrap();
            let expected = hex(id);
            assert!(is_secure(&expected, ip), "{ip}");
            let mut random = expected;
            random[19] = rand;
            let made = secure_node_id(ip, random);
            assert_eq!(made[..2], expected[..2], "{ip}");
            assert_eq!(made[2] & 0xf8, expected[2] & 0xf8, "{ip}");
            assert_eq!(made[19], rand);
        }
    }

    #[test]
    fn a_wrong_id_fails_and_private_addresses_are_exempt() {
        let ip: IpAddr = "124.31.75.21".parse().unwrap();
        let mut id = hex(VECTORS[0].2);
        id[0] ^= 0x80;
        assert!(!is_secure(&id, ip));
        assert!(is_secure(&id, "192.168.1.1".parse().unwrap()));
        let v6: IpAddr = "2001:db8::1".parse().unwrap();
        assert!(is_secure(&secure_node_id(v6, [9; 20]), v6));
    }
}
