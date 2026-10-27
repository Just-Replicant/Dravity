//! Wire protocol errors.

/// Errors decoding peer wire data. Any of these means the peer is speaking a
/// broken protocol and the connection should be dropped.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum WireError {
    /// The handshake does not start with `\x13BitTorrent protocol`.
    #[error("invalid handshake protocol string")]
    InvalidProtocol,
    /// A frame's length prefix exceeds the configured maximum.
    #[error("frame of {len} bytes exceeds maximum of {max}")]
    FrameTooLarge {
        /// Announced frame length.
        len: u32,
        /// Configured maximum.
        max: u32,
    },
    /// A message's payload length is wrong for its id.
    #[error("message id {id} has invalid payload length {len}")]
    InvalidPayloadLength {
        /// Message id.
        id: u8,
        /// Payload length (excluding the id byte).
        len: usize,
    },
    /// The payload does not fit in a 32-bit length prefix.
    #[error("message payload exceeds the 32-bit length prefix")]
    PayloadTooLarge,
    /// A bitfield's length or spare bits do not match the piece count.
    #[error("bitfield does not match the piece count")]
    BadBitfield,
}
