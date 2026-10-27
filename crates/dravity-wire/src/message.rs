//! BEP 3 peer messages and their length-prefixed framing.
//!
//! Every message after the handshake is `<u32 length><u8 id><payload>`; a
//! zero length is a keep-alive. Decoding is streaming and zero-copy: block
//! payloads and bitfields are [`Bytes`] slices of the receive buffer.
//!
//! ## Extending
//!
//! The decoder dispatches on [`MessageId`]. BEP 6 (ids 13–17) and BEP 10
//! (id 20) are named variants. Any other id decodes to [`Message::Unknown`].
//! Both enums are `#[non_exhaustive]`, so further messages can be added
//! without breaking the codec's structure.

use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::error::WireError;

/// Default maximum frame length: a 16 KiB block plus room for a bitfield of
/// a very large torrent (16 MiB covers 2^27 pieces).
pub const DEFAULT_MAX_FRAME_LEN: u32 = 16 * 1024 * 1024;

/// Known message ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
#[non_exhaustive]
pub enum MessageId {
    /// `choke`
    Choke = 0,
    /// `unchoke`
    Unchoke = 1,
    /// `interested`
    Interested = 2,
    /// `not interested`
    NotInterested = 3,
    /// `have`
    Have = 4,
    /// `bitfield`
    Bitfield = 5,
    /// `request`
    Request = 6,
    /// `piece`
    Piece = 7,
    /// `cancel`
    Cancel = 8,
    /// `port` (BEP 5)
    Port = 9,
    /// `suggest` (BEP 6)
    Suggest = 13,
    /// `have all` (BEP 6)
    HaveAll = 14,
    /// `have none` (BEP 6)
    HaveNone = 15,
    /// `reject` (BEP 6)
    Reject = 16,
    /// `allowed fast` (BEP 6)
    AllowedFast = 17,
    /// `extended` (BEP 10)
    Extended = 20,
}

impl TryFrom<u8> for MessageId {
    type Error = u8;

    fn try_from(id: u8) -> Result<Self, u8> {
        Ok(match id {
            0 => Self::Choke,
            1 => Self::Unchoke,
            2 => Self::Interested,
            3 => Self::NotInterested,
            4 => Self::Have,
            5 => Self::Bitfield,
            6 => Self::Request,
            7 => Self::Piece,
            8 => Self::Cancel,
            9 => Self::Port,
            13 => Self::Suggest,
            14 => Self::HaveAll,
            15 => Self::HaveNone,
            16 => Self::Reject,
            17 => Self::AllowedFast,
            20 => Self::Extended,
            other => return Err(other),
        })
    }
}

/// Address of a block: used by `request` and `cancel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockRequest {
    /// Piece index.
    pub piece: u32,
    /// Byte offset within the piece.
    pub offset: u32,
    /// Requested length.
    pub length: u32,
}

impl From<dravity_core::BlockRef> for BlockRequest {
    fn from(b: dravity_core::BlockRef) -> Self {
        Self {
            piece: b.piece,
            offset: b.offset,
            length: b.length,
        }
    }
}

/// A peer wire message.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Message {
    /// Zero-length frame.
    KeepAlive,
    /// Sender will not serve requests.
    Choke,
    /// Sender will serve requests.
    Unchoke,
    /// Sender wants pieces the receiver has.
    Interested,
    /// Sender wants nothing from the receiver.
    NotInterested,
    /// Sender completed a piece.
    Have {
        /// Piece index.
        piece: u32,
    },
    /// Sender's piece bitmap (high bit of byte 0 is piece 0). Length is
    /// validated against the torrent by the caller, which knows the piece count.
    Bitfield(Bytes),
    /// Request a block.
    Request(BlockRequest),
    /// Block data.
    Piece {
        /// Piece index.
        piece: u32,
        /// Byte offset within the piece.
        offset: u32,
        /// Block contents.
        data: Bytes,
    },
    /// Cancel an earlier request.
    Cancel(BlockRequest),
    /// Sender's DHT port (BEP 5).
    Port(u16),
    /// Suggest a piece (BEP 6).
    Suggest {
        /// Piece index.
        piece: u32,
    },
    /// Sender has every piece (BEP 6).
    HaveAll,
    /// Sender has no pieces (BEP 6).
    HaveNone,
    /// Reject a request (BEP 6).
    Reject(BlockRequest),
    /// A piece the sender will serve while choked (BEP 6).
    AllowedFast {
        /// Piece index.
        piece: u32,
    },
    /// BEP 10 extended message. Extended id 0 is the handshake; parse that
    /// payload with [`crate::ExtendedHandshake`].
    Extended {
        /// Extended message id.
        id: u8,
        /// Payload after the extended id.
        payload: Bytes,
    },
    /// A message with an id this crate does not (yet) understand.
    /// Never produced for ids listed in [`MessageId`].
    Unknown {
        /// Message id.
        id: u8,
        /// Raw payload.
        payload: Bytes,
    },
}

impl Message {
    /// The message id, or `None` for keep-alive.
    #[must_use]
    pub fn id(&self) -> Option<u8> {
        let id = match self {
            Self::KeepAlive => return None,
            Self::Choke => MessageId::Choke,
            Self::Unchoke => MessageId::Unchoke,
            Self::Interested => MessageId::Interested,
            Self::NotInterested => MessageId::NotInterested,
            Self::Have { .. } => MessageId::Have,
            Self::Bitfield(_) => MessageId::Bitfield,
            Self::Request(_) => MessageId::Request,
            Self::Piece { .. } => MessageId::Piece,
            Self::Cancel(_) => MessageId::Cancel,
            Self::Port(_) => MessageId::Port,
            Self::Suggest { .. } => MessageId::Suggest,
            Self::HaveAll => MessageId::HaveAll,
            Self::HaveNone => MessageId::HaveNone,
            Self::Reject(_) => MessageId::Reject,
            Self::AllowedFast { .. } => MessageId::AllowedFast,
            Self::Extended { .. } => MessageId::Extended,
            Self::Unknown { id, .. } => return Some(*id),
        };
        Some(id as u8)
    }

    /// Payload length, excluding the id byte.
    fn payload_len(&self) -> usize {
        match self {
            Self::KeepAlive
            | Self::Choke
            | Self::Unchoke
            | Self::Interested
            | Self::NotInterested
            | Self::HaveAll
            | Self::HaveNone => 0,
            Self::Have { .. } | Self::Suggest { .. } | Self::AllowedFast { .. } => 4,
            Self::Bitfield(b) | Self::Unknown { payload: b, .. } => b.len(),
            Self::Request(_) | Self::Cancel(_) | Self::Reject(_) => 12,
            Self::Piece { data, .. } => 8 + data.len(),
            Self::Port(_) => 2,
            Self::Extended { payload, .. } => 1 + payload.len(),
        }
    }

    /// Total encoded length including the 4-byte length prefix.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        4 + self.id().map_or(0, |_| 1 + self.payload_len())
    }

    /// Appends the framed encoding to `dst`.
    ///
    /// # Errors
    /// [`WireError::PayloadTooLarge`] when the body does not fit in a `u32`
    /// length prefix.
    pub fn encode(&self, dst: &mut BytesMut) -> Result<(), WireError> {
        let Some(id) = self.id() else {
            dst.reserve(4);
            dst.put_u32(0);
            return Ok(());
        };
        let body = self
            .payload_len()
            .checked_add(1)
            .ok_or(WireError::PayloadTooLarge)?;
        let len = u32::try_from(body).map_err(|_| WireError::PayloadTooLarge)?;
        dst.reserve(4 + body);
        dst.put_u32(len);
        dst.put_u8(id);
        match self {
            Self::KeepAlive
            | Self::Choke
            | Self::Unchoke
            | Self::Interested
            | Self::NotInterested => {}
            Self::Have { piece } => dst.put_u32(*piece),
            Self::Bitfield(b) | Self::Unknown { payload: b, .. } => dst.put_slice(b),
            Self::Request(r) | Self::Cancel(r) => {
                dst.put_u32(r.piece);
                dst.put_u32(r.offset);
                dst.put_u32(r.length);
            }
            Self::Piece {
                piece,
                offset,
                data,
            } => {
                dst.put_u32(*piece);
                dst.put_u32(*offset);
                dst.put_slice(data);
            }
            Self::Port(p) => dst.put_u16(*p),
            Self::Suggest { piece } | Self::AllowedFast { piece } => dst.put_u32(*piece),
            Self::HaveAll | Self::HaveNone => {}
            Self::Reject(r) => {
                dst.put_u32(r.piece);
                dst.put_u32(r.offset);
                dst.put_u32(r.length);
            }
            Self::Extended { id, payload } => {
                dst.put_u8(*id);
                dst.put_slice(payload);
            }
        }
        Ok(())
    }

    /// Decodes one frame body (id + payload, without the length prefix).
    /// An empty frame is a keep-alive.
    ///
    /// # Errors
    /// [`WireError::InvalidPayloadLength`] if the payload size is wrong for the id.
    pub fn decode_frame(mut frame: Bytes) -> Result<Self, WireError> {
        if frame.is_empty() {
            return Ok(Self::KeepAlive);
        }
        let id = frame.get_u8();
        let len = frame.len();
        let need = |n: usize| {
            if len == n {
                Ok(())
            } else {
                Err(WireError::InvalidPayloadLength { id, len })
            }
        };
        let Ok(known) = MessageId::try_from(id) else {
            return Ok(Self::Unknown { id, payload: frame });
        };
        Ok(match known {
            MessageId::Choke => need(0).map(|()| Self::Choke)?,
            MessageId::Unchoke => need(0).map(|()| Self::Unchoke)?,
            MessageId::Interested => need(0).map(|()| Self::Interested)?,
            MessageId::NotInterested => need(0).map(|()| Self::NotInterested)?,
            MessageId::Have => {
                need(4)?;
                Self::Have {
                    piece: frame.get_u32(),
                }
            }
            MessageId::Bitfield => Self::Bitfield(frame),
            MessageId::Request | MessageId::Cancel => {
                need(12)?;
                let r = BlockRequest {
                    piece: frame.get_u32(),
                    offset: frame.get_u32(),
                    length: frame.get_u32(),
                };
                if known == MessageId::Request {
                    Self::Request(r)
                } else {
                    Self::Cancel(r)
                }
            }
            MessageId::Piece => {
                if len < 8 {
                    return Err(WireError::InvalidPayloadLength { id, len });
                }
                let piece = frame.get_u32();
                let offset = frame.get_u32();
                Self::Piece {
                    piece,
                    offset,
                    data: frame,
                }
            }
            MessageId::Port => {
                need(2)?;
                Self::Port(frame.get_u16())
            }
            MessageId::Suggest => {
                need(4)?;
                Self::Suggest {
                    piece: frame.get_u32(),
                }
            }
            MessageId::HaveAll => need(0).map(|()| Self::HaveAll)?,
            MessageId::HaveNone => need(0).map(|()| Self::HaveNone)?,
            MessageId::Reject => {
                need(12)?;
                Self::Reject(BlockRequest {
                    piece: frame.get_u32(),
                    offset: frame.get_u32(),
                    length: frame.get_u32(),
                })
            }
            MessageId::AllowedFast => {
                need(4)?;
                Self::AllowedFast {
                    piece: frame.get_u32(),
                }
            }
            MessageId::Extended => {
                if len < 1 {
                    return Err(WireError::InvalidPayloadLength { id, len });
                }
                Self::Extended {
                    id: frame.get_u8(),
                    payload: frame,
                }
            }
        })
    }
}

/// Reads one bit. Bit 0 is the high bit of the first byte.
#[must_use]
pub fn bitfield_bit(bitfield: &[u8], index: u32) -> Option<bool> {
    let index = usize::try_from(index).ok()?;
    let byte = *bitfield.get(index / 8)?;
    let shift = 7 - (index % 8);
    Some((byte >> shift) & 1 == 1)
}

/// Checks that `bitfield` is `ceil(piece_count / 8)` bytes and that spare bits are clear.
///
/// # Errors
/// [`WireError::BadBitfield`] when the length or the unused low bits are wrong.
/// A torrent with no pieces requires an empty bitfield.
pub fn check_bitfield(bitfield: &[u8], piece_count: u32) -> Result<(), WireError> {
    let need = usize::try_from(piece_count.div_ceil(8)).unwrap_or(usize::MAX);
    if bitfield.len() != need {
        return Err(WireError::BadBitfield);
    }
    let spare = piece_count % 8;
    if spare != 0 {
        let mask = (1_u8 << (8 - spare)).wrapping_sub(1);
        let Some(last) = bitfield.last().copied() else {
            return Err(WireError::BadBitfield);
        };
        if last & mask != 0 {
            return Err(WireError::BadBitfield);
        }
    }
    Ok(())
}

/// Streaming decoder for length-prefixed messages.
///
/// Feed received bytes into a [`BytesMut`] and call [`decode`](Self::decode)
/// until it returns `Ok(None)`. Oversized frames are rejected from the
/// 4-byte prefix alone, before any payload is buffered.
///
/// ```
/// use bytes::BytesMut;
/// use dravity_wire::{Message, MessageDecoder};
///
/// let mut buf = BytesMut::new();
/// Message::Have { piece: 7 }.encode(&mut buf).unwrap();
/// Message::KeepAlive.encode(&mut buf).unwrap();
/// let mut dec = MessageDecoder::new();
/// assert_eq!(dec.decode(&mut buf).unwrap(), Some(Message::Have { piece: 7 }));
/// assert_eq!(dec.decode(&mut buf).unwrap(), Some(Message::KeepAlive));
/// assert_eq!(dec.decode(&mut buf).unwrap(), None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageDecoder {
    max_frame_len: u32,
}

impl Default for MessageDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl MessageDecoder {
    /// Decoder with [`DEFAULT_MAX_FRAME_LEN`].
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_frame_len: DEFAULT_MAX_FRAME_LEN,
        }
    }

    /// Decoder with a custom maximum frame length.
    #[must_use]
    pub const fn with_max_frame_len(max_frame_len: u32) -> Self {
        Self { max_frame_len }
    }

    /// The configured maximum frame length.
    #[must_use]
    pub const fn max_frame_len(&self) -> u32 {
        self.max_frame_len
    }

    /// Decodes the next message from the front of `src`, or returns `Ok(None)`
    /// if a complete frame has not arrived yet. On success the frame is
    /// removed from `src`.
    ///
    /// # Errors
    /// [`WireError::FrameTooLarge`] or [`WireError::InvalidPayloadLength`].
    pub fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Message>, WireError> {
        let Some(prefix) = src.get(..4) else {
            return Ok(None);
        };
        let mut prefix_bytes = [0u8; 4];
        prefix_bytes.copy_from_slice(prefix);
        let len = u32::from_be_bytes(prefix_bytes);
        if len > self.max_frame_len {
            return Err(WireError::FrameTooLarge {
                len,
                max: self.max_frame_len,
            });
        }
        let frame_len = usize::try_from(len).map_err(|_| WireError::FrameTooLarge {
            len,
            max: self.max_frame_len,
        })?;
        if src.len() - 4 < frame_len {
            src.reserve(frame_len + 4 - src.len());
            return Ok(None);
        }
        src.advance(4);
        let frame = src.split_to(frame_len).freeze();
        Message::decode_frame(frame).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(m: &Message) -> Vec<u8> {
        let mut b = BytesMut::new();
        m.encode(&mut b).unwrap();
        assert_eq!(b.len(), m.encoded_len());
        b.to_vec()
    }

    #[test]
    fn known_encodings() {
        assert_eq!(enc(&Message::KeepAlive), [0, 0, 0, 0]);
        assert_eq!(enc(&Message::Choke), [0, 0, 0, 1, 0]);
        assert_eq!(enc(&Message::NotInterested), [0, 0, 0, 1, 3]);
        assert_eq!(
            enc(&Message::Have { piece: 0x0102_0304 }),
            [0, 0, 0, 5, 4, 1, 2, 3, 4]
        );
        assert_eq!(
            enc(&Message::Request(BlockRequest {
                piece: 1,
                offset: 0x4000,
                length: 0x4000
            })),
            [0, 0, 0, 13, 6, 0, 0, 0, 1, 0, 0, 0x40, 0, 0, 0, 0x40, 0]
        );
        assert_eq!(
            enc(&Message::Piece {
                piece: 2,
                offset: 3,
                data: Bytes::from_static(b"xy")
            }),
            [0, 0, 0, 11, 7, 0, 0, 0, 2, 0, 0, 0, 3, b'x', b'y']
        );
        assert_eq!(enc(&Message::Port(6881)), [0, 0, 0, 3, 9, 0x1a, 0xe1]);
        assert_eq!(enc(&Message::HaveAll), [0, 0, 0, 1, 14]);
        assert_eq!(enc(&Message::HaveNone), [0, 0, 0, 1, 15]);
        assert_eq!(
            enc(&Message::Suggest { piece: 7 }),
            [0, 0, 0, 5, 13, 0, 0, 0, 7]
        );
        assert_eq!(
            enc(&Message::AllowedFast { piece: 4 }),
            [0, 0, 0, 5, 17, 0, 0, 0, 4]
        );
        assert_eq!(
            enc(&Message::Reject(BlockRequest {
                piece: 1,
                offset: 2,
                length: 3
            })),
            [0, 0, 0, 13, 16, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3]
        );
        assert_eq!(
            enc(&Message::Extended {
                id: 0,
                payload: Bytes::from_static(b"de")
            }),
            [0, 0, 0, 4, 20, 0, b'd', b'e']
        );
        assert_eq!(
            enc(&Message::Unknown {
                id: 21,
                payload: Bytes::from_static(b"\x00d")
            }),
            [0, 0, 0, 3, 21, 0, b'd']
        );
    }

    #[test]
    fn rejects_bad_lengths() {
        let cases: &[(&[u8], WireError)] = &[
            (
                &[0, 0, 0, 2, 0, 0],
                WireError::InvalidPayloadLength { id: 0, len: 1 },
            ),
            (
                &[0, 0, 0, 4, 4, 0, 0, 0],
                WireError::InvalidPayloadLength { id: 4, len: 3 },
            ),
            (
                &[0, 0, 0, 2, 6, 0],
                WireError::InvalidPayloadLength { id: 6, len: 1 },
            ),
            (
                &[0, 0, 0, 8, 7, 0, 0, 0, 0, 0, 0, 0],
                WireError::InvalidPayloadLength { id: 7, len: 7 },
            ),
            (
                &[0, 0, 0, 2, 9, 0],
                WireError::InvalidPayloadLength { id: 9, len: 1 },
            ),
            (
                &[0xff, 0xff, 0xff, 0xff],
                WireError::FrameTooLarge {
                    len: u32::MAX,
                    max: DEFAULT_MAX_FRAME_LEN,
                },
            ),
        ];
        for (input, err) in cases {
            let mut buf = BytesMut::from(*input);
            assert_eq!(
                MessageDecoder::new().decode(&mut buf),
                Err(err.clone()),
                "{input:?}"
            );
        }
    }

    #[test]
    fn piece_payload_is_zero_copy() {
        let mut buf = BytesMut::new();
        Message::Piece {
            piece: 0,
            offset: 0,
            data: Bytes::from(vec![7u8; 16384]),
        }
        .encode(&mut buf)
        .unwrap();
        let base = buf.as_ptr();
        let Some(Message::Piece { data, .. }) = MessageDecoder::new().decode(&mut buf).unwrap()
        else {
            panic!("expected piece")
        };
        assert_eq!(data.as_ptr(), base.wrapping_add(13));
    }

    #[test]
    fn partial_frames_wait_for_more() {
        let mut dec = MessageDecoder::new();
        let mut buf = BytesMut::from(&[0u8, 0, 0][..]);
        assert_eq!(dec.decode(&mut buf), Ok(None));
        buf.extend_from_slice(&[5, 4, 0, 0]);
        assert_eq!(dec.decode(&mut buf), Ok(None));
        assert_eq!(buf.len(), 7, "nothing consumed while incomplete");
        buf.extend_from_slice(&[0, 9, 1]);
        assert_eq!(dec.decode(&mut buf), Ok(Some(Message::Have { piece: 9 })));
        assert_eq!(&buf[..], &[1]);
    }

    #[test]
    fn bitfield_matches_piece_count() {
        assert!(check_bitfield(&[], 0).is_ok());
        assert_eq!(check_bitfield(&[0], 0), Err(WireError::BadBitfield));
        assert!(check_bitfield(&[0x80], 1).is_ok());
        assert_eq!(check_bitfield(&[0x81], 1), Err(WireError::BadBitfield));
        assert!(check_bitfield(&[0xff], 8).is_ok());
        assert!(check_bitfield(&[0xff, 0x80], 9).is_ok());
        assert_eq!(
            check_bitfield(&[0xff, 0x40], 9),
            Err(WireError::BadBitfield)
        );
        assert_eq!(bitfield_bit(&[0x80], 0), Some(true));
        assert_eq!(bitfield_bit(&[0x80], 1), Some(false));
        assert_eq!(bitfield_bit(&[0x80], 8), None);
    }

    #[test]
    fn message_id_roundtrip() {
        for id in 0u8..=255 {
            if let Ok(known) = MessageId::try_from(id) {
                assert_eq!(known as u8, id);
            }
        }
    }
}
