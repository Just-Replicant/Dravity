//! Error type shared by decoding and encoding.

use std::fmt::Display;

/// Convenient result alias for this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Errors produced while decoding or encoding bencode.
///
/// Decoding errors carry the byte `offset` into the input at which the problem
/// was detected, which makes malformed `.torrent` files easy to diagnose.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The input ended before a complete value was read.
    #[error("unexpected end of input at offset {offset}")]
    UnexpectedEof {
        /// Offset at which more input was required.
        offset: usize,
    },
    /// A byte that cannot start or continue a value at this position.
    #[error("unexpected byte {byte:#04x} at offset {offset}")]
    UnexpectedByte {
        /// The offending byte.
        byte: u8,
        /// Offset of the offending byte.
        offset: usize,
    },
    /// An integer that is not in canonical form (`i-0e`, `i03e`, `ie`, ...).
    #[error("invalid integer at offset {offset}")]
    InvalidInteger {
        /// Offset of the `i` that starts the integer.
        offset: usize,
    },
    /// An integer that does not fit the requested Rust type.
    #[error("integer out of range at offset {offset}")]
    IntegerOverflow {
        /// Offset of the `i` that starts the integer.
        offset: usize,
    },
    /// A byte-string length prefix that is malformed or too large.
    #[error("invalid byte string length at offset {offset}")]
    InvalidLength {
        /// Offset of the length prefix.
        offset: usize,
    },
    /// Dictionary keys are not in strictly ascending order (strict mode only).
    #[error("dictionary keys not sorted at offset {offset}")]
    UnsortedKeys {
        /// Offset of the out-of-order key.
        offset: usize,
    },
    /// The same key appears twice in a dictionary.
    #[error("duplicate dictionary key at offset {offset}")]
    DuplicateKey {
        /// Offset of the repeated key.
        offset: usize,
    },
    /// A dictionary key was not a byte string.
    #[error("dictionary key is not a byte string at offset {offset}")]
    KeyNotBytes {
        /// Offset of the offending key.
        offset: usize,
    },
    /// Lists/dictionaries are nested deeper than the configured limit.
    #[error("nesting deeper than {max} levels at offset {offset}")]
    DepthLimitExceeded {
        /// The configured limit.
        max: usize,
        /// Offset at which the limit was exceeded.
        offset: usize,
    },
    /// A list or dictionary contains more entries than the configured limit.
    #[error("more than {max} entries in a container at offset {offset}")]
    TooManyItems {
        /// The configured limit.
        max: usize,
        /// Offset of the entry that exceeded the limit.
        offset: usize,
    },
    /// Bytes remain after the top-level value.
    #[error("trailing data at offset {offset}")]
    TrailingData {
        /// Offset of the first trailing byte.
        offset: usize,
    },
    /// A byte string was requested as text but is not valid UTF-8.
    #[error("byte string at offset {offset} is not valid UTF-8")]
    InvalidUtf8 {
        /// Offset of the byte string.
        offset: usize,
    },
    /// The Rust type cannot be represented in bencode (floats, unit, `None` in a list...).
    #[error("unsupported type for bencode: {0}")]
    Unsupported(&'static str),
    /// Two map entries serialized to the same key.
    #[error("duplicate map key during serialization")]
    DuplicateMapKey,
    /// A map key did not serialize to a byte string.
    #[error("map key must serialize to a byte string")]
    MapKeyNotBytes,
    /// Error raised by a `serde` implementation.
    #[error("{0}")]
    Custom(String),
}

impl serde::de::Error for Error {
    fn custom<T: Display>(msg: T) -> Self {
        Self::Custom(msg.to_string())
    }
}

impl serde::ser::Error for Error {
    fn custom<T: Display>(msg: T) -> Self {
        Self::Custom(msg.to_string())
    }
}
