//! The BEP 33 scrape bloom filter: 2048 bits, two bit indices per IP
//! address taken from its SHA-1, and the count estimate the BEP gives.

use std::net::IpAddr;

use sha1::{Digest, Sha1};

/// Filter size in bits.
const M: usize = 2048;

/// A 256-byte BEP 33 bloom filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bloom([u8; M / 8]);

impl Default for Bloom {
    fn default() -> Self {
        Self::new()
    }
}

impl Bloom {
    /// An empty filter.
    #[must_use]
    pub const fn new() -> Self {
        Self([0; M / 8])
    }

    /// A filter from its 256 bytes, or `None` for another length.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(Self)
    }

    /// The 256 bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; M / 8] {
        &self.0
    }

    /// Adds `ip` (its 4 or 16 address bytes are hashed, BEP 33).
    pub fn insert(&mut self, ip: IpAddr) {
        let digest = match ip {
            IpAddr::V4(v4) => Sha1::digest(v4.octets()),
            IpAddr::V6(v6) => Sha1::digest(v6.octets()),
        };
        for pair in [0usize, 2] {
            let lo = usize::from(digest.get(pair).copied().unwrap_or(0));
            let hi = usize::from(digest.get(pair + 1).copied().unwrap_or(0));
            let index = (lo | (hi << 8)) % M;
            if let Some(byte) = self.0.get_mut(index / 8) {
                *byte |= 1 << (index % 8);
            }
        }
    }

    /// Adds every member of `other` (filters from several nodes).
    pub fn merge(&mut self, other: &Self) {
        for (a, b) in self.0.iter_mut().zip(other.0) {
            *a |= b;
        }
    }

    /// The estimated number of distinct addresses (BEP 33's formula,
    /// capped where the filter saturates).
    #[must_use]
    pub fn estimate(&self) -> u32 {
        let zeros: u32 = self.0.iter().map(|b| b.count_zeros()).sum();
        let m = f64::from(u32::try_from(M).unwrap_or(u32::MAX));
        let c = f64::from(zeros.clamp(1, u32::try_from(M - 1).unwrap_or(u32::MAX)));
        let n = (c / m).ln() / (2.0 * (1.0 - 1.0 / m).ln());
        // At most 6000 or so fit in 2048 bits; the cast is in range.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let n = n.round().clamp(0.0, 1_000_000.0) as u32;
        if zeros == u32::try_from(M).unwrap_or(u32::MAX) {
            0
        } else {
            n
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing)]

    use std::net::Ipv4Addr;

    use super::*;

    #[test]
    fn estimates_track_the_count() {
        let mut bloom = Bloom::new();
        assert_eq!(bloom.estimate(), 0);
        for i in 0..200u32 {
            bloom.insert(IpAddr::V4(Ipv4Addr::from(0x0a00_0000 + i)));
        }
        let n = bloom.estimate();
        assert!((180..=220).contains(&n), "estimate {n}");
        let mut merged = Bloom::new();
        merged.merge(&bloom);
        merged.insert(IpAddr::V4(Ipv4Addr::from(0x0a00_0000)));
        assert_eq!(merged, bloom, "re-adding a member changes nothing");
        assert!(Bloom::from_bytes(&[0; 10]).is_none());
    }

    #[test]
    fn bep33_test_vector() {
        // BEP 33: 192.0.2.0..=192.0.2.255 and 2001:db8::..=2001:db8::3e7
        // give a filter starting F6C3F5EA A07FFD91 and ending FD8F72EF,
        // estimated at 1224.9308.
        let mut bloom = Bloom::new();
        for i in 0..256u32 {
            bloom.insert(IpAddr::V4(Ipv4Addr::from(0xc000_0200 + i)));
        }
        for i in 0..1000u128 {
            bloom.insert(IpAddr::V6(std::net::Ipv6Addr::from(
                0x2001_0db8_0000_0000_0000_0000_0000_0000u128 + i,
            )));
        }
        let bytes = bloom.as_bytes();
        assert_eq!(bytes[..8], [0xF6, 0xC3, 0xF5, 0xEA, 0xA0, 0x7F, 0xFD, 0x91]);
        assert_eq!(bytes[252..], [0xFD, 0x8F, 0x72, 0xEF]);
        assert_eq!(bloom.estimate(), 1225);
    }
}
