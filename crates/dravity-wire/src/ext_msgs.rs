//! Small BEP 10 extension messages: BEP 54 `lt_donthave` and the
//! `lt_tex` tracker exchange.

use std::borrow::Cow;

use dravity_bencode::{Dict, Value};

use crate::ExtendedError;

/// BEP 54 extension name: the sender lost a piece it announced before.
pub const LT_DONTHAVE: &str = "lt_donthave";
/// Tracker exchange extension name (as libtorrent and qBittorrent use it).
pub const LT_TEX: &str = "lt_tex";
/// Most tracker URLs decoded from or written to one `lt_tex` message.
pub const MAX_TEX_TRACKERS: usize = 50;
/// Longest tracker URL accepted in an `lt_tex` message.
pub const MAX_TEX_URL_LEN: usize = 512;

/// Encodes an `lt_donthave` payload: the piece index, big-endian.
#[must_use]
pub const fn encode_donthave(piece: u32) -> [u8; 4] {
    piece.to_be_bytes()
}

/// Decodes an `lt_donthave` payload; `None` unless it is exactly 4 bytes.
///
/// ```
/// use dravity_wire::{decode_donthave, encode_donthave};
///
/// assert_eq!(decode_donthave(&encode_donthave(7)), Some(7));
/// assert_eq!(decode_donthave(&[0, 1]), None);
/// ```
#[must_use]
pub fn decode_donthave(payload: &[u8]) -> Option<u32> {
    <[u8; 4]>::try_from(payload).ok().map(u32::from_be_bytes)
}

/// Whether `url` may be shared or taken from tracker exchange: an
/// `http`, `https` or `udp` URL of sane length with no whitespace or
/// control characters.
#[must_use]
pub fn is_exchangeable_tracker(url: &str) -> bool {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    let scheme_ok = ["http://", "https://", "udp://"]
        .iter()
        .any(|s| lower.starts_with(s));
    scheme_ok
        && url.len() <= MAX_TEX_URL_LEN
        && !url
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
}

/// An `lt_tex` message: trackers the sender uses that it has not sent
/// before (`added`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackerExchange {
    /// Tracker URLs.
    pub added: Vec<String>,
}

impl TrackerExchange {
    /// Decodes a payload. URLs that are not exchangeable
    /// ([`is_exchangeable_tracker`]) or not UTF-8 are skipped; at most
    /// [`MAX_TEX_TRACKERS`] are kept.
    ///
    /// # Errors
    /// [`ExtendedError`] when the payload is not a dictionary or `added`
    /// is not a list.
    pub fn decode(payload: &[u8]) -> Result<Self, ExtendedError> {
        let value = Value::decode(payload)?;
        let dict = value.as_dict().ok_or(ExtendedError::NotADict)?;
        let added = match dict.get(b"added".as_slice()) {
            None => Vec::new(),
            Some(list) => list
                .as_list()
                .ok_or(ExtendedError::BadType { field: "added" })?
                .iter()
                .filter_map(|v| std::str::from_utf8(v.as_bytes()?).ok())
                .filter(|u| is_exchangeable_tracker(u))
                .take(MAX_TEX_TRACKERS)
                .map(str::to_owned)
                .collect(),
        };
        Ok(Self { added })
    }

    /// Encodes the message (at most [`MAX_TEX_TRACKERS`] URLs).
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let list = self
            .added
            .iter()
            .take(MAX_TEX_TRACKERS)
            .map(|u| Value::Bytes(Cow::Owned(u.as_bytes().to_vec())))
            .collect();
        let mut dict = Dict::new();
        dict.insert(Cow::Borrowed(b"added"), Value::List(list));
        Value::Dict(dict).to_vec()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn donthave_is_four_bytes() {
        assert_eq!(encode_donthave(0x0102_0304), [1, 2, 3, 4]);
        assert_eq!(decode_donthave(&[1, 2, 3, 4]), Some(0x0102_0304));
        assert_eq!(decode_donthave(&[1, 2, 3, 4, 5]), None);
    }

    #[test]
    fn tex_round_trips_and_filters() {
        let m = TrackerExchange {
            added: vec!["udp://t:1/announce".into(), "https://x/a".into()],
        };
        assert_eq!(TrackerExchange::decode(&m.encode()).unwrap(), m);
        let hostile = b"d5:addedl11:file:///etc8:http://a3:x y5:udp:/i3eee";
        assert_eq!(
            TrackerExchange::decode(hostile).unwrap().added,
            ["http://a"]
        );
        assert!(TrackerExchange::decode(b"le").is_err());
        assert!(TrackerExchange::decode(b"d5:addedi1ee").is_err());
        let long = format!("http://{}", "a".repeat(MAX_TEX_URL_LEN));
        assert!(!is_exchangeable_tracker(&long));
        assert!(is_exchangeable_tracker("HTTP://T/announce"));
        let many = TrackerExchange {
            added: (0..80).map(|i| format!("http://t{i}/a")).collect(),
        };
        assert_eq!(
            TrackerExchange::decode(&many.encode()).unwrap().added.len(),
            MAX_TEX_TRACKERS
        );
    }
}
