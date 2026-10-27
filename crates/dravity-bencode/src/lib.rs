//! Zero-copy [bencode] for the Dravity BitTorrent engine.
//!
//! * [`Value`] — a dynamically typed tree that borrows byte strings from the input.
//! * [`from_bytes`] / [`to_vec`] — serde integration; deserialization borrows
//!   `&[u8]`/`&str` fields directly from the input.
//! * [`RawValue`] — captures the exact bytes of a sub-value (used for info-hashes).
//!
//! Decoding is hardened for untrusted input: integers and length prefixes must
//! be canonical, nesting depth and container size are bounded, duplicate keys
//! and trailing data are rejected, and no input can cause a panic (this is
//! continuously fuzzed).
//! Encoding is always canonical (sorted dictionary keys).
//!
//! ```
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Debug, PartialEq, Serialize, Deserialize)]
//! struct Peer<'a> {
//!     ip: &'a str,
//!     port: u16,
//! }
//!
//! let bytes = dravity_bencode::to_vec(&Peer { ip: "10.0.0.1", port: 6881 }).unwrap();
//! assert_eq!(bytes, b"d2:ip8:10.0.0.14:porti6881ee");
//! let peer: Peer<'_> = dravity_bencode::from_bytes(&bytes).unwrap();
//! assert_eq!(peer.port, 6881);
//! ```
//!
//! [bencode]: https://www.bittorrent.org/beps/bep_0003.html#bencoding

#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod de;
mod encode;
mod error;
mod parser;
mod raw;
pub mod ser;
mod value;

pub use de::{Deserializer, from_bytes, from_bytes_with};
pub use error::{Error, Result};
pub use parser::{DEFAULT_MAX_DEPTH, DEFAULT_MAX_ITEMS, DecodeOptions};
pub use raw::RawValue;
pub use ser::{Serializer, to_vec};
pub use value::{Dict, Value};
