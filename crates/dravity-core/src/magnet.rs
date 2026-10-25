//! Magnet URI parsing and formatting (BEP 9, plus BEP 52 `btmh`).

use std::fmt;
use std::str::FromStr;

use crate::info_hash::{InfoHash, InfoHashV2};

/// Errors parsing a magnet URI.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MagnetError {
    /// The URI does not start with `magnet:?`.
    #[error("not a magnet URI")]
    NotMagnet,
    /// No supported `xt` (`urn:btih:` or `urn:btmh:1220`) parameter.
    #[error("no BitTorrent info-hash (xt=urn:btih:/urn:btmh:) found")]
    MissingInfoHash,
    /// An `xt` value has a malformed hash.
    #[error("invalid info-hash in {0:?}")]
    InvalidInfoHash(String),
    /// Two `xt` parameters disagree.
    #[error("conflicting info-hashes in magnet URI")]
    ConflictingInfoHash,
    /// A `%` escape is malformed or the decoded text is not UTF-8.
    #[error("invalid percent-encoding in parameter {0:?}")]
    InvalidEncoding(String),
    /// `xl` is not a non-negative integer.
    #[error("invalid exact length {0:?}")]
    InvalidLength(String),
}

/// A parsed magnet link.
///
/// ```
/// use dravity_core::Magnet;
///
/// let m: Magnet = "magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a\
///                  &dn=Big+Buck+Bunny&tr=udp%3A%2F%2Ftracker.example%3A1337"
///     .parse()
///     .unwrap();
/// assert_eq!(m.display_name.as_deref(), Some("Big Buck Bunny"));
/// assert_eq!(m.trackers, ["udp://tracker.example:1337"]);
/// // Formatting and re-parsing is lossless.
/// assert_eq!(m.to_string().parse::<Magnet>().unwrap(), m);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Magnet {
    /// v1 info-hash (`xt=urn:btih:`).
    pub info_hash: Option<InfoHash>,
    /// v2 info-hash (`xt=urn:btmh:1220…`).
    pub info_hash_v2: Option<InfoHashV2>,
    /// Display name (`dn`).
    pub display_name: Option<String>,
    /// Tracker URLs (`tr`), in order, duplicates removed.
    pub trackers: Vec<String>,
    /// Peer addresses (`x.pe`).
    pub peers: Vec<String>,
    /// Web seeds (`ws`).
    pub web_seeds: Vec<String>,
    /// Exact content length in bytes (`xl`).
    pub exact_length: Option<u64>,
}

impl Magnet {
    /// Creates a magnet for a v1 info-hash.
    #[must_use]
    pub fn new(info_hash: InfoHash) -> Self {
        Self {
            info_hash: Some(info_hash),
            ..Self::default()
        }
    }

    /// Parses a magnet URI.
    ///
    /// Unknown parameters are ignored; the `.N` suffix form (`tr.1=`) is accepted.
    ///
    /// # Errors
    /// Returns a [`MagnetError`] if the URI is malformed or carries no
    /// BitTorrent info-hash.
    pub fn parse(uri: &str) -> Result<Self, MagnetError> {
        let query =
            strip_prefix_ignore_case(uri.trim(), "magnet:?").ok_or(MagnetError::NotMagnet)?;
        let mut m = Self::default();
        for param in query.split('&').filter(|p| !p.is_empty()) {
            let Some((key, raw)) = param.split_once('=') else {
                continue;
            };
            let key = key.split_once('.').map_or(key, |(k, n)| {
                // `tr.1` style indexes; keep `x.pe` intact.
                if n.bytes().all(|b| b.is_ascii_digit()) {
                    k
                } else {
                    key
                }
            });
            match key {
                "xt" => m.parse_xt(&decode(raw, false, param)?)?,
                "dn" => m.display_name = Some(decode(raw, true, param)?),
                "tr" => push_unique(&mut m.trackers, decode(raw, false, param)?),
                "x.pe" => push_unique(&mut m.peers, decode(raw, false, param)?),
                "ws" => push_unique(&mut m.web_seeds, decode(raw, false, param)?),
                "xl" => {
                    let v = decode(raw, false, param)?;
                    m.exact_length = Some(v.parse().map_err(|_| MagnetError::InvalidLength(v))?);
                }
                _ => {}
            }
        }
        if m.info_hash.is_none() && m.info_hash_v2.is_none() {
            return Err(MagnetError::MissingInfoHash);
        }
        Ok(m)
    }

    fn parse_xt(&mut self, xt: &str) -> Result<(), MagnetError> {
        if let Some(h) = strip_prefix_ignore_case(xt, "urn:btih:") {
            let h = h
                .parse()
                .map_err(|_| MagnetError::InvalidInfoHash(xt.to_owned()))?;
            set_once(&mut self.info_hash, h)
        } else if let Some(mh) = strip_prefix_ignore_case(xt, "urn:btmh:") {
            // Multihash: 0x12 (sha2-256) 0x20 (32 bytes) + digest.
            let h = strip_prefix_ignore_case(mh, "1220")
                .and_then(|d| InfoHashV2::from_hex(d).ok())
                .ok_or_else(|| MagnetError::InvalidInfoHash(xt.to_owned()))?;
            set_once(&mut self.info_hash_v2, h)
        } else {
            Ok(()) // Other URN types (ed2k, ...) are ignored.
        }
    }

    /// Formats the magnet as a URI.
    #[must_use]
    pub fn to_uri(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(h) = &self.info_hash {
            parts.push(format!("xt=urn:btih:{h}"));
        }
        if let Some(h) = &self.info_hash_v2 {
            parts.push(format!("xt=urn:btmh:1220{h}"));
        }
        if let Some(dn) = &self.display_name {
            parts.push(format!("dn={}", encode(dn)));
        }
        if let Some(xl) = self.exact_length {
            parts.push(format!("xl={xl}"));
        }
        parts.extend(self.trackers.iter().map(|t| format!("tr={}", encode(t))));
        parts.extend(self.web_seeds.iter().map(|w| format!("ws={}", encode(w))));
        parts.extend(self.peers.iter().map(|p| format!("x.pe={}", encode(p))));
        format!("magnet:?{}", parts.join("&"))
    }
}

impl FromStr for Magnet {
    type Err = MagnetError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl fmt::Display for Magnet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_uri())
    }
}

fn set_once<T: PartialEq>(slot: &mut Option<T>, v: T) -> Result<(), MagnetError> {
    match slot {
        Some(existing) if *existing != v => Err(MagnetError::ConflictingInfoHash),
        _ => {
            *slot = Some(v);
            Ok(())
        }
    }
}

fn push_unique(list: &mut Vec<String>, v: String) {
    if !v.is_empty() && !list.contains(&v) {
        list.push(v);
    }
}

fn strip_prefix_ignore_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| s.get(prefix.len()..))
        .flatten()
}

/// Percent-decodes `raw`; with `plus_is_space`, `+` decodes to a space
/// (form-encoding, which many clients use for `dn`).
fn decode(raw: &str, plus_is_space: bool, param: &str) -> Result<String, MagnetError> {
    let err = || MagnetError::InvalidEncoding(param.to_owned());
    let mut out = Vec::with_capacity(raw.len());
    let mut bytes = raw.bytes();
    while let Some(b) = bytes.next() {
        match b {
            b'%' => {
                let hi = bytes
                    .next()
                    .and_then(|c| char::from(c).to_digit(16))
                    .ok_or_else(err)?;
                let lo = bytes
                    .next()
                    .and_then(|c| char::from(c).to_digit(16))
                    .ok_or_else(err)?;
                out.push(u8::try_from(hi * 16 + lo).map_err(|_| err())?);
            }
            b'+' if plus_is_space => out.push(b' '),
            _ => out.push(b),
        }
    }
    String::from_utf8(out).map_err(|_| err())
}

/// Percent-encodes everything except RFC 3986 unreserved characters.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const HEX: &str = "c12fe1c06bba254a9dc9f519b335aa7c1367a88a";

    #[test]
    fn parses_full_magnet() {
        let uri = format!(
            "MAGNET:?xt=urn:BTIH:{}&dn=a%20b+c&tr=http%3A%2F%2Ft1%2Fann&tr.1=udp://t2:80&tr=udp://t2:80\
             &xl=1234&ws=https://ws/x&x.pe=10.0.0.1:6881&so=0-3&xt=urn:ed2k:abc",
            HEX.to_uppercase()
        );
        let m = Magnet::parse(&uri).unwrap();
        assert_eq!(m.info_hash.unwrap().to_hex(), HEX);
        assert_eq!(m.display_name.as_deref(), Some("a b c"));
        assert_eq!(m.trackers, ["http://t1/ann", "udp://t2:80"]);
        assert_eq!(m.exact_length, Some(1234));
        assert_eq!(m.web_seeds, ["https://ws/x"]);
        assert_eq!(m.peers, ["10.0.0.1:6881"]);
    }

    #[test]
    fn base32_and_v2() {
        let m = Magnet::parse("magnet:?xt=urn:btih:AAAQEAYEAUDAOCAJBIFQYDIOB4IBCEQT").unwrap();
        assert_eq!(m.info_hash.unwrap().as_bytes()[1], 1);
        let v2 = "ab".repeat(32);
        let m = Magnet::parse(&format!("magnet:?xt=urn:btmh:1220{v2}")).unwrap();
        assert_eq!(m.info_hash, None);
        assert_eq!(m.info_hash_v2.unwrap().to_hex(), v2);
        assert_eq!(m.to_string(), format!("magnet:?xt=urn:btmh:1220{v2}"));
    }

    #[test]
    fn errors() {
        assert_eq!(Magnet::parse("http://x"), Err(MagnetError::NotMagnet));
        assert_eq!(
            Magnet::parse("magnet:?dn=x"),
            Err(MagnetError::MissingInfoHash)
        );
        assert!(matches!(
            Magnet::parse("magnet:?xt=urn:btih:abc"),
            Err(MagnetError::InvalidInfoHash(_))
        ));
        assert!(matches!(
            Magnet::parse(&format!("magnet:?xt=urn:btih:{HEX}&dn=%zz")),
            Err(MagnetError::InvalidEncoding(_))
        ));
        assert!(matches!(
            Magnet::parse(&format!("magnet:?xt=urn:btih:{HEX}&dn=%ff")),
            Err(MagnetError::InvalidEncoding(_))
        ));
        assert_eq!(
            Magnet::parse(&format!(
                "magnet:?xt=urn:btih:{HEX}&xt=urn:btih:{}",
                "0".repeat(40)
            )),
            Err(MagnetError::ConflictingInfoHash)
        );
        assert!(matches!(
            Magnet::parse(&format!("magnet:?xt=urn:btih:{HEX}&xl=-1")),
            Err(MagnetError::InvalidLength(_))
        ));
    }

    fn arb_text() -> impl Strategy<Value = String> {
        "\\PC{1,24}"
    }

    proptest! {
        #[test]
        fn roundtrip(
            hash in any::<[u8; 20]>(),
            dn in proptest::option::of(arb_text()),
            trackers in proptest::collection::vec(arb_text(), 0..4),
            xl in proptest::option::of(any::<u64>()),
        ) {
            let mut m = Magnet::new(InfoHash::new(hash));
            m.display_name = dn;
            for t in trackers {
                push_unique(&mut m.trackers, t);
            }
            m.exact_length = xl;
            prop_assert_eq!(Magnet::parse(&m.to_uri()).unwrap(), m);
        }

        #[test]
        fn never_panics(s in "\\PC{0,200}") {
            let _ = Magnet::parse(&s);
            let _ = Magnet::parse(&format!("magnet:?{s}"));
        }
    }
}
