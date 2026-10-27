//! `tokio_util::codec` adapter for the sans-IO wire decoder.

use bytes::BytesMut;
use dravity_wire::{Message, MessageDecoder, WireError};
use tokio_util::codec::{Decoder, Encoder};

/// Errors from a framed peer connection.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// The peer violated the wire protocol.
    #[error(transparent)]
    Wire(#[from] WireError),
    /// Socket error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Frames post-handshake peer messages for `FramedRead`/`FramedWrite`.
///
/// ```
/// use bytes::BytesMut;
/// use dravity_session::PeerCodec;
/// use dravity_wire::Message;
/// use tokio_util::codec::{Decoder, Encoder};
///
/// let mut codec = PeerCodec::default();
/// let mut buf = BytesMut::new();
/// codec.encode(Message::Unchoke, &mut buf).unwrap();
/// assert_eq!(codec.decode(&mut buf).unwrap(), Some(Message::Unchoke));
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct PeerCodec {
    decoder: MessageDecoder,
}

impl PeerCodec {
    /// A codec with a custom maximum frame length.
    #[must_use]
    pub const fn with_max_frame_len(max: u32) -> Self {
        Self {
            decoder: MessageDecoder::with_max_frame_len(max),
        }
    }
}

impl Decoder for PeerCodec {
    type Item = Message;
    type Error = CodecError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Message>, CodecError> {
        Ok(self.decoder.decode(src)?)
    }
}

impl Encoder<Message> for PeerCodec {
    type Error = CodecError;

    fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<(), CodecError> {
        Ok(item.encode(dst)?)
    }
}
