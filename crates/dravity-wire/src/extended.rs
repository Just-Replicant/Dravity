//! BEP 10 extended handshake.
//!
//! The handshake is an extended message with id 0. Its payload is one bencode
//! dictionary. Unknown keys are ignored. The `m` map is always written, even
//! when it is empty. The frame itself stays a zero-copy [`crate::Message::Extended`];
//! parsing the handshake copies only these small fields.

use std::borrow::Cow;

use dravity_bencode::{Dict, Value};

/// Errors parsing or encoding an extended handshake.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ExtendedError {
    /// The payload is not valid bencode.
    #[error(transparent)]
    Bencode(#[from] dravity_bencode::Error),
    /// The payload is not a dictionary.
    #[error("extended handshake is not a dictionary")]
    NotADict,
    /// A known field has the wrong bencode type.
    #[error("extended handshake field `{field}` has the wrong type")]
    BadType {
        /// Field name.
        field: &'static str,
    },
    /// An extension name is not valid UTF-8.
    #[error("extended handshake field `{field}` is not valid UTF-8")]
    BadText {
        /// Field name.
        field: &'static str,
    },
    /// An integer field does not fit the expected range.
    #[error("extended handshake field `{field}` is out of range")]
    OutOfRange {
        /// Field name.
        field: &'static str,
    },
    /// A `ut_metadata` message has an unknown `msg_type`.
    #[error("unknown ut_metadata msg_type {0}")]
    UnknownMessageType(i64),
    /// An entry in `m` is not a single byte.
    #[error("extension id for {name:?} does not fit in a byte")]
    BadExtensionId {
        /// Extension name.
        name: String,
    },
}

/// Extended handshake fields used by later metadata and peer exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtendedHandshake {
    /// Extension name to message id, in byte order of the name.
    pub extensions: Vec<(String, u8)>,
    /// `p`, the local TCP listen port.
    pub listen_port: Option<u16>,
    /// `v`, the client version. Not necessarily UTF-8.
    pub client: Option<Vec<u8>>,
    /// `yourip`, the receiver's address as seen by the sender.
    pub your_ip: Option<Vec<u8>>,
    /// `reqq`, the sender's request queue size.
    pub request_queue: Option<u32>,
    /// `metadata_size`, the ut_metadata info-dict size.
    pub metadata_size: Option<u64>,
    /// Optional compact IPv4 endpoint.
    pub ipv4: Option<Vec<u8>>,
    /// Optional compact IPv6 endpoint.
    pub ipv6: Option<Vec<u8>>,
}

impl ExtendedHandshake {
    /// Decode the bencode payload of extended message 0.
    ///
    /// # Errors
    /// Returns [`ExtendedError`] when the payload is not a handshake dictionary
    /// or a known field has the wrong type or range.
    pub fn decode(payload: &[u8]) -> Result<Self, ExtendedError> {
        let value = Value::decode(payload)?;
        let Some(dict) = value.as_dict() else {
            return Err(ExtendedError::NotADict);
        };
        let extensions = match dict.get(b"m".as_slice()) {
            None => Vec::new(),
            Some(value) => extension_map(value)?,
        };
        Ok(Self {
            extensions,
            listen_port: optional_u16(dict, b"p", "p")?,
            client: optional_bytes(dict, b"v", "v")?,
            your_ip: optional_bytes(dict, b"yourip", "yourip")?,
            request_queue: optional_u32(dict, b"reqq", "reqq")?,
            metadata_size: optional_u64(dict, b"metadata_size", "metadata_size")?,
            ipv4: optional_bytes(dict, b"ipv4", "ipv4")?,
            ipv6: optional_bytes(dict, b"ipv6", "ipv6")?,
        })
    }

    /// Id registered for `name`, if the peer offered it.
    #[must_use]
    pub fn extension_id(&self, name: &str) -> Option<u8> {
        self.extensions
            .iter()
            .find(|(extension, _)| extension == name)
            .map(|(_, id)| *id)
    }

    /// Canonical bencode dictionary.
    ///
    /// # Errors
    /// [`ExtendedError::OutOfRange`] when `metadata_size` does not fit in an
    /// `i64`, which is the largest integer bencode writes.
    pub fn encode(&self) -> Result<Vec<u8>, ExtendedError> {
        let mut entries = Dict::new();
        let mut extensions = Dict::new();
        for (name, id) in &self.extensions {
            extensions.insert(
                Cow::Owned(name.as_bytes().to_vec()),
                Value::Int(i64::from(*id)),
            );
        }
        entries.insert(Cow::Borrowed(b"m"), Value::Dict(extensions));
        if let Some(bytes) = &self.ipv4 {
            entries.insert(
                Cow::Borrowed(b"ipv4"),
                Value::Bytes(Cow::Owned(bytes.clone())),
            );
        }
        if let Some(bytes) = &self.ipv6 {
            entries.insert(
                Cow::Borrowed(b"ipv6"),
                Value::Bytes(Cow::Owned(bytes.clone())),
            );
        }
        if let Some(size) = self.metadata_size {
            let size = i64::try_from(size).map_err(|_| ExtendedError::OutOfRange {
                field: "metadata_size",
            })?;
            entries.insert(Cow::Borrowed(b"metadata_size"), Value::Int(size));
        }
        if let Some(port) = self.listen_port {
            entries.insert(Cow::Borrowed(b"p"), Value::Int(i64::from(port)));
        }
        if let Some(queue) = self.request_queue {
            entries.insert(Cow::Borrowed(b"reqq"), Value::Int(i64::from(queue)));
        }
        if let Some(client) = &self.client {
            entries.insert(
                Cow::Borrowed(b"v"),
                Value::Bytes(Cow::Owned(client.clone())),
            );
        }
        if let Some(ip) = &self.your_ip {
            entries.insert(
                Cow::Borrowed(b"yourip"),
                Value::Bytes(Cow::Owned(ip.clone())),
            );
        }
        Ok(Value::Dict(entries).to_vec())
    }
}

fn extension_map(value: &Value<'_>) -> Result<Vec<(String, u8)>, ExtendedError> {
    let Some(dict) = value.as_dict() else {
        return Err(ExtendedError::BadType { field: "m" });
    };
    let mut extensions = Vec::with_capacity(dict.len());
    for (key, value) in dict {
        let name = std::str::from_utf8(key)
            .map_err(|_| ExtendedError::BadText { field: "m" })?
            .to_owned();
        let id = value
            .as_int()
            .ok_or(ExtendedError::BadType { field: "m" })?;
        let id =
            u8::try_from(id).map_err(|_| ExtendedError::BadExtensionId { name: name.clone() })?;
        extensions.push((name, id));
    }
    Ok(extensions)
}

fn optional_bytes(
    dict: &Dict<'_>,
    key: &[u8],
    field: &'static str,
) -> Result<Option<Vec<u8>>, ExtendedError> {
    match dict.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_bytes()
            .map(|bytes| Some(bytes.to_vec()))
            .ok_or(ExtendedError::BadType { field }),
    }
}

fn optional_u16(
    dict: &Dict<'_>,
    key: &[u8],
    field: &'static str,
) -> Result<Option<u16>, ExtendedError> {
    match optional_int(dict, key, field)? {
        None => Ok(None),
        Some(value) => u16::try_from(value)
            .map(Some)
            .map_err(|_| ExtendedError::OutOfRange { field }),
    }
}

fn optional_u32(
    dict: &Dict<'_>,
    key: &[u8],
    field: &'static str,
) -> Result<Option<u32>, ExtendedError> {
    match optional_int(dict, key, field)? {
        None => Ok(None),
        Some(value) => u32::try_from(value)
            .map(Some)
            .map_err(|_| ExtendedError::OutOfRange { field }),
    }
}

fn optional_u64(
    dict: &Dict<'_>,
    key: &[u8],
    field: &'static str,
) -> Result<Option<u64>, ExtendedError> {
    match optional_int(dict, key, field)? {
        None => Ok(None),
        Some(value) => u64::try_from(value)
            .map(Some)
            .map_err(|_| ExtendedError::OutOfRange { field }),
    }
}

fn optional_int(
    dict: &Dict<'_>,
    key: &[u8],
    field: &'static str,
) -> Result<Option<i64>, ExtendedError> {
    match dict.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_int()
            .map(Some)
            .ok_or(ExtendedError::BadType { field }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_roundtrip() {
        let hs = ExtendedHandshake {
            extensions: vec![("ut_metadata".to_owned(), 1), ("ut_pex".to_owned(), 2)],
            listen_port: Some(6881),
            client: Some(b"dravity".to_vec()),
            your_ip: Some(vec![127, 0, 0, 1]),
            request_queue: Some(250),
            metadata_size: Some(100),
            ipv4: Some(vec![1, 2, 3, 4, 0x1a, 0xe1]),
            ipv6: None,
        };
        let encoded = hs.encode().unwrap();
        let decoded = ExtendedHandshake::decode(&encoded).unwrap();
        assert_eq!(decoded, hs);
        assert_eq!(decoded.extension_id("ut_metadata"), Some(1));
        assert_eq!(decoded.extension_id("missing"), None);
        let empty = ExtendedHandshake::decode(b"d1:mdee").unwrap();
        assert!(empty.extensions.is_empty());
        assert_eq!(empty.listen_port, None);
    }

    #[test]
    fn rejects_bad_handshake() {
        assert!(matches!(
            ExtendedHandshake::decode(b"i1e"),
            Err(ExtendedError::NotADict)
        ));
        assert!(matches!(
            ExtendedHandshake::decode(b"d1:mi1ee"),
            Err(ExtendedError::BadType { field: "m" })
        ));
        assert!(matches!(
            ExtendedHandshake::decode(b"d1:md11:ut_metadatai256eee"),
            Err(ExtendedError::BadExtensionId { .. })
        ));
        assert!(matches!(
            ExtendedHandshake::decode(b"d1:mde1:pi-1ee"),
            Err(ExtendedError::OutOfRange { field: "p" })
        ));
    }
}
