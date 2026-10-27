//! serde deserialization from borrowed bencode.

use serde::de::value::{BorrowedStrDeserializer, SeqDeserializer};
use serde::de::{self, DeserializeSeed, EnumAccess, MapAccess, SeqAccess, VariantAccess, Visitor};
use serde::forward_to_deserialize_any;

use crate::error::{Error, Result};
use crate::parser::{DecodeOptions, Parser, parse_i64, parse_u64};
use crate::raw::RAW_VALUE_TOKEN;

/// Deserializes a `T` from exactly one bencode value in `input`.
///
/// Byte strings are borrowed from `input`, so `&'a [u8]` and `&'a str` fields
/// are zero-copy. Use `#[serde(with = "serde_bytes")]`-style wrappers, `&[u8]`,
/// or [`RawValue`](crate::RawValue) for binary data; `Vec<u8>` also accepts a
/// byte string.
///
/// # Errors
/// Returns an [`Error`] for malformed input, a type mismatch, or trailing data.
pub fn from_bytes<'a, T: de::Deserialize<'a>>(input: &'a [u8]) -> Result<T> {
    from_bytes_with(input, DecodeOptions::default())
}

/// Like [`from_bytes`] with custom [`DecodeOptions`].
///
/// # Errors
/// Returns an [`Error`] for malformed input, a type mismatch, or trailing data.
pub fn from_bytes_with<'a, T: de::Deserialize<'a>>(
    input: &'a [u8],
    opts: DecodeOptions,
) -> Result<T> {
    let mut de = Deserializer::with_options(input, opts);
    let value = T::deserialize(&mut de)?;
    de.end()?;
    Ok(value)
}

/// A serde [`Deserializer`](de::Deserializer) over borrowed bencode input.
#[derive(Debug)]
pub struct Deserializer<'de> {
    p: Parser<'de>,
}

impl<'de> Deserializer<'de> {
    /// Creates a deserializer with default options.
    #[must_use]
    pub fn from_bytes(input: &'de [u8]) -> Self {
        Self::with_options(input, DecodeOptions::default())
    }

    /// Creates a deserializer with custom options.
    #[must_use]
    pub fn with_options(input: &'de [u8], opts: DecodeOptions) -> Self {
        Self {
            p: Parser::new(input, opts),
        }
    }

    /// Checks that the whole input has been consumed.
    ///
    /// # Errors
    /// Returns [`Error::TrailingData`] if bytes remain.
    pub fn end(&self) -> Result<()> {
        self.p.finish()
    }

    /// Current byte offset into the input.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.p.pos()
    }

    fn str(&mut self) -> Result<&'de str> {
        let (b, offset) = self.p.byte_string()?;
        std::str::from_utf8(b).map_err(|_| Error::InvalidUtf8 { offset })
    }

    fn next_is_bytes(&self) -> Result<bool> {
        Ok(self.p.peek()?.is_ascii_digit())
    }
}

impl<'de> de::Deserializer<'de> for &mut Deserializer<'de> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.p.peek()? {
            b'i' => {
                let (digits, offset) = self.p.int_digits()?;
                if digits.first() == Some(&b'-') {
                    visitor.visit_i64(parse_i64(digits).ok_or(Error::IntegerOverflow { offset })?)
                } else {
                    visitor.visit_u64(parse_u64(digits).ok_or(Error::IntegerOverflow { offset })?)
                }
            }
            b'0'..=b'9' => visitor.visit_borrowed_bytes(self.p.byte_string()?.0),
            b'l' => self.deserialize_seq(visitor),
            b'd' => self.deserialize_map(visitor),
            byte => Err(Error::UnexpectedByte {
                byte,
                offset: self.p.pos(),
            }),
        }
    }

    forward_to_deserialize_any! {
        i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 tuple_struct
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        if self.p.peek()? != b'i' {
            return self.deserialize_any(visitor);
        }
        match self.p.int_i64()? {
            0 => visitor.visit_bool(false),
            1 => visitor.visit_bool(true),
            n => Err(de::Error::invalid_value(
                de::Unexpected::Signed(n),
                &"0 or 1",
            )),
        }
    }

    fn deserialize_f32<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
        Err(Error::Unsupported("floating point"))
    }

    fn deserialize_f64<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
        Err(Error::Unsupported("floating point"))
    }

    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_str(visitor)
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        if self.next_is_bytes()? {
            visitor.visit_borrowed_str(self.str()?)
        } else {
            self.deserialize_any(visitor)
        }
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_str(visitor)
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_any(visitor)
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_any(visitor)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        // Bencode has no null: absent dictionary keys map to `None` via serde's
        // missing-field handling; anything present is `Some`.
        visitor.visit_some(self)
    }

    fn deserialize_unit<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
        Err(Error::Unsupported("unit"))
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _visitor: V,
    ) -> Result<V::Value> {
        Err(Error::Unsupported("unit struct"))
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        if name == RAW_VALUE_TOKEN {
            let raw = self.p.skip_value()?;
            visitor.visit_borrowed_bytes(raw)
        } else {
            visitor.visit_newtype_struct(self)
        }
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        if self.next_is_bytes()? {
            // Allow `Vec<u8>` / `[u8; N]` to be read from a byte string.
            let (b, _) = self.p.byte_string()?;
            let mut seq = SeqDeserializer::<_, Error>::new(b.iter().copied());
            let value = visitor.visit_seq(&mut seq)?;
            seq.end()?;
            return Ok(value);
        }
        if self.p.peek()? != b'l' {
            return self.deserialize_any(visitor);
        }
        self.p.enter(b'l')?;
        let mut access = ListAccess {
            de: self,
            seen: 0,
            done: false,
        };
        let value = visitor.visit_seq(&mut access)?;
        if !access.done && !access.de.p.eat_end()? {
            return Err(Error::Custom("list has more elements than expected".into()));
        }
        access.de.p.leave();
        Ok(value)
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        if self.p.peek()? != b'd' {
            return self.deserialize_any(visitor);
        }
        self.p.enter(b'd')?;
        let mut access = DictAccess {
            de: self,
            prev: None,
            seen: 0,
            done: false,
        };
        let value = visitor.visit_map(&mut access)?;
        if !access.done {
            return Err(Error::Custom(
                "dictionary has more entries than expected".into(),
            ));
        }
        access.de.p.leave();
        Ok(value)
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        match self.p.peek()? {
            b'0'..=b'9' => {
                let variant = self.str()?;
                visitor.visit_enum(BorrowedStrDeserializer::<Error>::new(variant))
            }
            b'd' => {
                self.p.enter(b'd')?;
                let (key, offset) = self.p.dict_key()?;
                let variant =
                    std::str::from_utf8(key).map_err(|_| Error::InvalidUtf8 { offset })?;
                let value = visitor.visit_enum(Enum {
                    de: &mut *self,
                    variant,
                })?;
                self.p.expect(b'e')?;
                self.p.leave();
                Ok(value)
            }
            byte => Err(Error::UnexpectedByte {
                byte,
                offset: self.p.pos(),
            }),
        }
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_str(visitor)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.p.skip_value()?;
        visitor.visit_unit()
    }

    fn is_human_readable(&self) -> bool {
        false
    }
}

struct ListAccess<'a, 'de> {
    de: &'a mut Deserializer<'de>,
    seen: usize,
    done: bool,
}

impl<'de> SeqAccess<'de> for ListAccess<'_, 'de> {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>> {
        if self.done || self.de.p.eat_end()? {
            self.done = true;
            return Ok(None);
        }
        self.de.p.check_items(self.seen)?;
        self.seen += 1;
        seed.deserialize(&mut *self.de).map(Some)
    }
}

struct DictAccess<'a, 'de> {
    de: &'a mut Deserializer<'de>,
    prev: Option<&'de [u8]>,
    seen: usize,
    done: bool,
}

impl<'de> MapAccess<'de> for DictAccess<'_, 'de> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>> {
        if self.done || self.de.p.eat_end()? {
            self.done = true;
            return Ok(None);
        }
        self.de.p.check_items(self.seen)?;
        self.seen += 1;
        let (key, offset) = self.de.p.dict_key()?;
        self.de.p.check_key(self.prev, key, offset)?;
        self.prev = Some(key);
        seed.deserialize(KeyDeserializer { key, offset }).map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value> {
        seed.deserialize(&mut *self.de)
    }
}

struct Enum<'a, 'de> {
    de: &'a mut Deserializer<'de>,
    variant: &'de str,
}

impl<'de> EnumAccess<'de> for Enum<'_, 'de> {
    type Error = Error;
    type Variant = Self;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Self)> {
        let v = seed.deserialize(BorrowedStrDeserializer::<Error>::new(self.variant))?;
        Ok((v, self))
    }
}

impl<'de> VariantAccess<'de> for Enum<'_, 'de> {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        Err(Error::Custom(
            "unit variant must be encoded as a byte string".into(),
        ))
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value> {
        seed.deserialize(&mut *self.de)
    }

    fn tuple_variant<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value> {
        de::Deserializer::deserialize_seq(&mut *self.de, visitor)
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        de::Deserializer::deserialize_map(&mut *self.de, visitor)
    }
}

/// Deserializer for dictionary keys, which are always byte strings.
struct KeyDeserializer<'de> {
    key: &'de [u8],
    offset: usize,
}

impl<'de> KeyDeserializer<'de> {
    fn str(&self) -> Result<&'de str> {
        let key: &'de [u8] = self.key;
        std::str::from_utf8(key).map_err(|_| Error::InvalidUtf8 {
            offset: self.offset,
        })
    }
}

impl<'de> de::Deserializer<'de> for KeyDeserializer<'de> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_borrowed_bytes(self.key)
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_borrowed_str(self.str()?)
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_str(visitor)
    }

    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        self.deserialize_str(visitor)
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        // Field identifiers accept bytes, so non-UTF-8 unknown keys are simply ignored.
        visitor.visit_borrowed_bytes(self.key)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_some(self)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        let s = self.str()?;
        visitor.visit_enum(BorrowedStrDeserializer::<Error>::new(s))
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 bytes byte_buf unit unit_struct
        seq tuple tuple_struct map struct ignored_any
    }
}
