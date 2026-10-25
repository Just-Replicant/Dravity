//! Info-hash and peer-id types.

use std::fmt;
use std::str::FromStr;

use sha1::{Digest, Sha1};

use crate::encoding::{base32_decode_20, hex_decode, hex_encode};

/// Error parsing an info-hash or peer id from text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind}: expected {expected}")]
pub struct ParseHashError {
    kind: &'static str,
    expected: &'static str,
}

/// A BitTorrent v1 info-hash: the SHA-1 of the bencoded `info` dictionary.
///
/// Displays as 40 lower-case hex digits; parses from 40 hex digits or the
/// 32-character base32 form found in older magnet links.
///
/// ```
/// use dravity_core::InfoHash;
///
/// let h: InfoHash = "c12fe1c06bba254a9dc9f519b335aa7c1367a88a".parse().unwrap();
/// assert_eq!(h.to_string(), "c12fe1c06bba254a9dc9f519b335aa7c1367a88a");
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct InfoHash([u8; 20]);

impl InfoHash {
    /// Length in bytes.
    pub const LEN: usize = 20;

    /// Wraps raw bytes.
    #[must_use]
    pub const fn new(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    /// Computes the info-hash of the raw, bencoded `info` dictionary bytes.
    #[must_use]
    pub fn from_info_bytes(info: &[u8]) -> Self {
        Self(Sha1::digest(info).into())
    }

    /// Raw bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// Lower-case hex representation.
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex_encode(&self.0)
    }

    /// Parses 40 hex digits (any case).
    ///
    /// # Errors
    /// Returns [`ParseHashError`] on wrong length or non-hex characters.
    pub fn from_hex(s: &str) -> Result<Self, ParseHashError> {
        hex_decode(s).map(Self).ok_or(ParseHashError {
            kind: "v1 info-hash",
            expected: "40 hex digits",
        })
    }

    /// Parses the 32-character base32 form.
    ///
    /// # Errors
    /// Returns [`ParseHashError`] on wrong length or invalid characters.
    pub fn from_base32(s: &str) -> Result<Self, ParseHashError> {
        base32_decode_20(s).map(Self).ok_or(ParseHashError {
            kind: "v1 info-hash",
            expected: "32 base32 characters",
        })
    }
}

impl FromStr for InfoHash {
    type Err = ParseHashError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.len() {
            32 => Self::from_base32(s),
            _ => Self::from_hex(s),
        }
    }
}

impl fmt::Display for InfoHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for InfoHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InfoHash({self})")
    }
}

impl From<[u8; 20]> for InfoHash {
    fn from(b: [u8; 20]) -> Self {
        Self(b)
    }
}

impl AsRef<[u8]> for InfoHash {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// A BitTorrent v2 info-hash (SHA-256, BEP 52).
///
/// Phase 0 only parses and prints it (for `urn:btmh:` magnet links); v2
/// metainfo support is scheduled for Phase 5.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct InfoHashV2([u8; 32]);

impl InfoHashV2 {
    /// Length in bytes.
    pub const LEN: usize = 32;

    /// Wraps raw bytes.
    #[must_use]
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Raw bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The 20-byte truncation used on the v1 wire protocol for hybrid swarms.
    #[must_use]
    pub fn truncated(&self) -> InfoHash {
        let mut b = [0u8; 20];
        b.copy_from_slice(self.0.get(..20).unwrap_or(&[0; 20]));
        InfoHash(b)
    }

    /// Lower-case hex representation.
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex_encode(&self.0)
    }

    /// Parses 64 hex digits (any case).
    ///
    /// # Errors
    /// Returns [`ParseHashError`] on wrong length or non-hex characters.
    pub fn from_hex(s: &str) -> Result<Self, ParseHashError> {
        hex_decode(s).map(Self).ok_or(ParseHashError {
            kind: "v2 info-hash",
            expected: "64 hex digits",
        })
    }
}

impl FromStr for InfoHashV2 {
    type Err = ParseHashError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_hex(s)
    }
}

impl fmt::Display for InfoHashV2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for InfoHashV2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "InfoHashV2({self})")
    }
}

/// A 20-byte peer id sent in the handshake.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct PeerId([u8; 20]);

impl PeerId {
    /// Length in bytes.
    pub const LEN: usize = 20;

    /// Wraps raw bytes.
    #[must_use]
    pub const fn new(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    /// Builds an Azureus-style id: `prefix` (e.g. `-DV0001-`) followed by
    /// `suffix` bytes, truncated/zero-padded to 20 bytes. The caller supplies
    /// the random suffix so this crate stays free of RNG dependencies.
    #[must_use]
    pub fn with_prefix(prefix: &[u8], suffix: &[u8]) -> Self {
        let mut b = [0u8; 20];
        for (dst, src) in b.iter_mut().zip(prefix.iter().chain(suffix)) {
            *dst = *src;
        }
        Self(b)
    }

    /// Raw bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }
}

impl fmt::Debug for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PeerId(b\"{}\")", self.0.escape_ascii())
    }
}

impl From<[u8; 20]> for PeerId {
    fn from(b: [u8; 20]) -> Self {
        Self(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_of_info() {
        // FIPS 180 test vector: SHA-1("abc").
        let h = InfoHash::from_info_bytes(b"abc");
        assert_eq!(h.to_hex(), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn parse_forms() {
        let hex = "0001020304050607080910111213141516171819";
        let h: InfoHash = hex.parse().unwrap();
        assert_eq!(h.to_string(), hex);
        assert_eq!(h, hex.to_uppercase().parse().unwrap());
        assert!("zz".parse::<InfoHash>().is_err());
        let b32: InfoHash = "AAAQEAYEAUDAOCAJBIFQYDIOB4IBCEQT".parse().unwrap();
        assert_eq!(b32.as_bytes()[19], 19);
        assert_eq!(format!("{h:?}"), format!("InfoHash({hex})"));
    }

    #[test]
    fn v2_truncation() {
        let v2 = InfoHashV2::from_hex(&"ab".repeat(32)).unwrap();
        assert_eq!(v2.truncated().to_hex(), "ab".repeat(20));
    }

    #[test]
    fn peer_id_prefix() {
        let id = PeerId::with_prefix(b"-DV0001-", b"0123456789abcdefXYZ");
        assert_eq!(id.as_bytes(), b"-DV0001-0123456789ab");
    }
}
