//! Borrowed raw bencode, used to capture exact input bytes (e.g. the `info`
//! dictionary whose SHA-1 is the torrent's info-hash).

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, Visitor};
use serde::ser::{Serialize, Serializer};

use crate::error::Result;
use crate::parser::{DecodeOptions, Parser};
use crate::value::{ByteSlice, Value};

/// Magic newtype name recognised by this crate's (de)serializer.
pub(crate) const RAW_VALUE_TOKEN: &str = "$dravity_bencode::private::RawValue";

/// The exact, validated bytes of one bencode value, borrowed from the input.
///
/// Deserializing a field as `RawValue` with [`crate::from_bytes`] captures the
/// value's bytes verbatim, without re-encoding. This is how the v1 info-hash is
/// computed: re-encoding could change the bytes of a non-canonical torrent and
/// thus its hash.
///
/// `RawValue` only deserializes from this crate's [`Deserializer`](crate::Deserializer).
///
/// ```
/// use dravity_bencode::RawValue;
/// use serde::Deserialize;
///
/// #[derive(Deserialize)]
/// struct Torrent<'a> {
///     #[serde(borrow)]
///     info: RawValue<'a>,
/// }
///
/// let t: Torrent<'_> = dravity_bencode::from_bytes(b"d4:infod1:xi1eee").unwrap();
/// assert_eq!(t.info.as_bytes(), b"d1:xi1ee");
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct RawValue<'a>(&'a [u8]);

impl<'a> RawValue<'a> {
    /// Validates that `bytes` hold exactly one bencode value and wraps them.
    ///
    /// # Errors
    /// Returns an error if `bytes` is not exactly one valid value.
    pub fn from_bytes(bytes: &'a [u8]) -> Result<Self> {
        let mut p = Parser::new(bytes, DecodeOptions::default());
        p.skip_value()?;
        p.finish()?;
        Ok(Self(bytes))
    }

    /// The raw encoded bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &'a [u8] {
        self.0
    }

    /// Decodes the raw bytes into a [`Value`].
    ///
    /// # Errors
    /// Propagates decoding errors (none are expected for a validated value
    /// unless stricter options are used elsewhere).
    pub fn to_value(&self) -> Result<Value<'a>> {
        Value::decode(self.0)
    }

    /// Deserializes the raw bytes into `T`.
    ///
    /// # Errors
    /// Returns an error if the bytes do not match `T`.
    pub fn deserialize<T: Deserialize<'a>>(&self) -> Result<T> {
        crate::from_bytes(self.0)
    }
}

impl fmt::Debug for RawValue<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RawValue(b\"{}\")", self.0.escape_ascii())
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for RawValue<'a> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<'a>(std::marker::PhantomData<&'a ()>);
        impl<'de: 'a, 'a> Visitor<'de> for V<'a> {
            type Value = RawValue<'a>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a raw bencode value (requires dravity_bencode::Deserializer)")
            }
            fn visit_borrowed_bytes<E: de::Error>(self, v: &'de [u8]) -> Result<Self::Value, E> {
                Ok(RawValue(v))
            }
        }
        d.deserialize_newtype_struct(RAW_VALUE_TOKEN, V(std::marker::PhantomData))
    }
}

impl Serialize for RawValue<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_newtype_struct(RAW_VALUE_TOKEN, &ByteSlice(self.0))
    }
}
