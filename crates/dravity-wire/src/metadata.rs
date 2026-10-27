//! BEP 9 `ut_metadata`: exchanging the `info` dictionary for magnet links.
//!
//! A message is a bencode dictionary (`msg_type`, `piece`, and for data
//! `total_size`) carried in a BEP 10 extended frame. A data message is
//! followed by the raw metadata piece, outside the dictionary. Metadata is
//! split into [`METADATA_PIECE_LEN`] pieces; only the last may be shorter.

use std::borrow::Cow;

use bytes::{Bytes, BytesMut};
use dravity_bencode::{Dict, Value};

use crate::ExtendedError;

/// Extension name in the BEP 10 handshake `m` dictionary.
pub const UT_METADATA: &str = "ut_metadata";

/// Size of one metadata piece (16 KiB).
pub const METADATA_PIECE_LEN: u32 = 16 * 1024;

/// One `ut_metadata` message.
///
/// ```
/// use bytes::Bytes;
/// use dravity_wire::MetadataMessage;
///
/// let msg = MetadataMessage::Data { piece: 0, total_size: 3, data: Bytes::from_static(b"abc") };
/// let payload = msg.encode().unwrap();
/// assert_eq!(&payload[..], b"d8:msg_typei1e5:piecei0e10:total_sizei3eeabc");
/// assert_eq!(MetadataMessage::decode(&payload).unwrap(), msg);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MetadataMessage {
    /// Ask for one metadata piece (`msg_type` 0).
    Request {
        /// Piece index.
        piece: u32,
    },
    /// One metadata piece (`msg_type` 1).
    Data {
        /// Piece index.
        piece: u32,
        /// Size of the whole `info` dictionary.
        total_size: u64,
        /// Piece contents (at most [`METADATA_PIECE_LEN`] bytes).
        data: Bytes,
    },
    /// The sender will not serve this piece (`msg_type` 2).
    Reject {
        /// Piece index.
        piece: u32,
    },
}

impl MetadataMessage {
    /// Parses an extended-message payload (after the extended id).
    ///
    /// # Errors
    /// [`ExtendedError`] for invalid bencode, missing or mistyped fields, an
    /// unknown `msg_type`, or a data piece larger than [`METADATA_PIECE_LEN`].
    pub fn decode(payload: &Bytes) -> Result<Self, ExtendedError> {
        let (value, used) = Value::decode_prefix(payload)?;
        let Some(dict) = value.as_dict() else {
            return Err(ExtendedError::NotADict);
        };
        let int = |key: &'static str| -> Result<i64, ExtendedError> {
            dict.get(key.as_bytes())
                .ok_or(ExtendedError::BadType { field: key })?
                .as_int()
                .ok_or(ExtendedError::BadType { field: key })
        };
        let piece = u32::try_from(int("piece")?)
            .map_err(|_| ExtendedError::OutOfRange { field: "piece" })?;
        match int("msg_type")? {
            0 => Ok(Self::Request { piece }),
            1 => {
                let total_size =
                    u64::try_from(int("total_size")?).map_err(|_| ExtendedError::OutOfRange {
                        field: "total_size",
                    })?;
                let data = payload.slice(used.min(payload.len())..);
                if data.len() > METADATA_PIECE_LEN as usize {
                    return Err(ExtendedError::OutOfRange { field: "data" });
                }
                Ok(Self::Data {
                    piece,
                    total_size,
                    data,
                })
            }
            2 => Ok(Self::Reject { piece }),
            other => Err(ExtendedError::UnknownMessageType(other)),
        }
    }

    /// Encodes the extended-message payload.
    ///
    /// # Errors
    /// [`ExtendedError::OutOfRange`] if `total_size` does not fit bencode's
    /// `i64` or `data` is larger than [`METADATA_PIECE_LEN`].
    pub fn encode(&self) -> Result<Bytes, ExtendedError> {
        let mut dict = Dict::new();
        let (msg_type, piece) = match self {
            Self::Request { piece } => (0, piece),
            Self::Data { piece, .. } => (1, piece),
            Self::Reject { piece } => (2, piece),
        };
        dict.insert(Cow::Borrowed(b"msg_type"), Value::Int(msg_type));
        dict.insert(Cow::Borrowed(b"piece"), Value::Int(i64::from(*piece)));
        let mut out = BytesMut::new();
        if let Self::Data {
            total_size, data, ..
        } = self
        {
            let size = i64::try_from(*total_size).map_err(|_| ExtendedError::OutOfRange {
                field: "total_size",
            })?;
            if data.len() > METADATA_PIECE_LEN as usize {
                return Err(ExtendedError::OutOfRange { field: "data" });
            }
            dict.insert(Cow::Borrowed(b"total_size"), Value::Int(size));
            out.extend_from_slice(&Value::Dict(dict).to_vec());
            out.extend_from_slice(data);
        } else {
            out.extend_from_slice(&Value::Dict(dict).to_vec());
        }
        Ok(out.freeze())
    }
}

/// Number of [`METADATA_PIECE_LEN`] pieces for metadata of `size` bytes.
///
/// ```
/// assert_eq!(dravity_wire::metadata_piece_count(16_385), 2);
/// assert_eq!(dravity_wire::metadata_piece_count(0), 0);
/// ```
#[must_use]
pub const fn metadata_piece_count(size: u64) -> u64 {
    size.div_ceil(METADATA_PIECE_LEN as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_and_reject_roundtrip() {
        for msg in [
            MetadataMessage::Request { piece: 7 },
            MetadataMessage::Reject { piece: 0 },
        ] {
            let payload = msg.encode().unwrap();
            assert_eq!(MetadataMessage::decode(&payload).unwrap(), msg);
        }
        assert_eq!(
            &MetadataMessage::Request { piece: 1 }.encode().unwrap()[..],
            b"d8:msg_typei0e5:piecei1ee"
        );
    }

    #[test]
    fn rejects_malformed() {
        let decode = |b: &'static [u8]| MetadataMessage::decode(&Bytes::from_static(b));
        assert!(matches!(decode(b"le"), Err(ExtendedError::NotADict)));
        assert!(matches!(
            decode(b"d5:piecei0ee"),
            Err(ExtendedError::BadType { field: "msg_type" })
        ));
        assert!(matches!(
            decode(b"d8:msg_typei9e5:piecei0ee"),
            Err(ExtendedError::UnknownMessageType(9))
        ));
        assert!(matches!(
            decode(b"d8:msg_typei0e5:piecei-1ee"),
            Err(ExtendedError::OutOfRange { field: "piece" })
        ));
        assert!(matches!(
            decode(b"d8:msg_typei1e5:piecei0ee"),
            Err(ExtendedError::BadType {
                field: "total_size"
            })
        ));
        assert!(decode(b"d8:msg_type").is_err());
        let big = MetadataMessage::Data {
            piece: 0,
            total_size: 1,
            data: Bytes::from(vec![0; METADATA_PIECE_LEN as usize + 1]),
        };
        assert!(big.encode().is_err());
    }
}
