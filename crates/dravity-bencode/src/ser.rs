//! serde serialization to canonical bencode.
//!
//! Dictionaries (maps and structs) are always emitted with keys sorted by raw
//! bytes, as BEP 3 requires. `None` values in maps/structs are omitted; `None`
//! inside a list, floats and unit types are rejected.

use serde::ser::{self, Serialize};

use crate::encode;
use crate::error::{Error, Result};
use crate::parser::{DecodeOptions, Parser};
use crate::raw::RAW_VALUE_TOKEN;

/// Serializes `value` to canonical bencode.
///
/// # Errors
/// Returns an [`Error`] if the value contains types bencode cannot represent.
pub fn to_vec<T: ?Sized + Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    value.serialize(&mut Serializer::new(&mut out))?;
    if out.is_empty() {
        return Err(Error::Unsupported("top-level None"));
    }
    Ok(out)
}

/// A serde [`Serializer`](ser::Serializer) that appends canonical bencode to a buffer.
#[derive(Debug)]
pub struct Serializer<'o> {
    out: &'o mut Vec<u8>,
}

impl<'o> Serializer<'o> {
    /// Creates a serializer appending to `out`.
    pub fn new(out: &'o mut Vec<u8>) -> Self {
        Self { out }
    }

    /// Serializes `value` into a fresh buffer; an empty result means `None`.
    fn nested<T: ?Sized + Serialize>(value: &T) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        value.serialize(&mut Serializer::new(&mut buf))?;
        Ok(buf)
    }

    /// Serializes a map key, which must produce a byte string, and returns its content.
    fn key<T: ?Sized + Serialize>(key: &T) -> Result<Vec<u8>> {
        let buf = Self::nested(key)?;
        if !buf.first().is_some_and(u8::is_ascii_digit) {
            return Err(Error::MapKeyNotBytes);
        }
        let mut p = Parser::new(&buf, DecodeOptions::default());
        let (content, _) = p.byte_string()?;
        p.finish()?;
        Ok(content.to_vec())
    }

    fn write_non_empty<T: ?Sized + Serialize>(
        &mut self,
        value: &T,
        what: &'static str,
    ) -> Result<()> {
        let before = self.out.len();
        value.serialize(&mut *self)?;
        if self.out.len() == before {
            return Err(Error::Unsupported(what));
        }
        Ok(())
    }
}

impl<'s, 'o> ser::Serializer for &'s mut Serializer<'o> {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = SeqSerializer<'s, 'o>;
    type SerializeTuple = SeqSerializer<'s, 'o>;
    type SerializeTupleStruct = SeqSerializer<'s, 'o>;
    type SerializeTupleVariant = SeqSerializer<'s, 'o>;
    type SerializeMap = DictSerializer<'s, 'o>;
    type SerializeStruct = DictSerializer<'s, 'o>;
    type SerializeStructVariant = DictSerializer<'s, 'o>;

    fn serialize_bool(self, v: bool) -> Result<()> {
        encode::int(self.out, u8::from(v));
        Ok(())
    }
    fn serialize_i8(self, v: i8) -> Result<()> {
        encode::int(self.out, v);
        Ok(())
    }
    fn serialize_i16(self, v: i16) -> Result<()> {
        encode::int(self.out, v);
        Ok(())
    }
    fn serialize_i32(self, v: i32) -> Result<()> {
        encode::int(self.out, v);
        Ok(())
    }
    fn serialize_i64(self, v: i64) -> Result<()> {
        encode::int(self.out, v);
        Ok(())
    }
    fn serialize_i128(self, v: i128) -> Result<()> {
        encode::int(self.out, v);
        Ok(())
    }
    fn serialize_u8(self, v: u8) -> Result<()> {
        encode::int(self.out, v);
        Ok(())
    }
    fn serialize_u16(self, v: u16) -> Result<()> {
        encode::int(self.out, v);
        Ok(())
    }
    fn serialize_u32(self, v: u32) -> Result<()> {
        encode::int(self.out, v);
        Ok(())
    }
    fn serialize_u64(self, v: u64) -> Result<()> {
        encode::int(self.out, v);
        Ok(())
    }
    fn serialize_u128(self, v: u128) -> Result<()> {
        encode::int(self.out, v);
        Ok(())
    }
    fn serialize_f32(self, _v: f32) -> Result<()> {
        Err(Error::Unsupported("floating point"))
    }
    fn serialize_f64(self, _v: f64) -> Result<()> {
        Err(Error::Unsupported("floating point"))
    }
    fn serialize_char(self, v: char) -> Result<()> {
        self.serialize_str(v.encode_utf8(&mut [0; 4]))
    }
    fn serialize_str(self, v: &str) -> Result<()> {
        encode::bytes(self.out, v.as_bytes());
        Ok(())
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<()> {
        encode::bytes(self.out, v);
        Ok(())
    }
    fn serialize_none(self) -> Result<()> {
        // Writes nothing; containers detect the empty output and omit the entry.
        Ok(())
    }
    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<()> {
        value.serialize(self)
    }
    fn serialize_unit(self) -> Result<()> {
        Err(Error::Unsupported("unit"))
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<()> {
        Err(Error::Unsupported("unit struct"))
    }
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<()> {
        self.serialize_str(variant)
    }
    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        name: &'static str,
        value: &T,
    ) -> Result<()> {
        if name == RAW_VALUE_TOKEN {
            // `RawValue` serializes as a byte string holding already-encoded
            // bencode: validate it and splice it in verbatim.
            let wrapped = Serializer::nested(value)?;
            let mut p = Parser::new(&wrapped, DecodeOptions::default());
            let (raw, _) = p.byte_string()?;
            let mut check = Parser::new(raw, DecodeOptions::default());
            check.skip_value()?;
            check.finish()?;
            self.out.extend_from_slice(raw);
            return Ok(());
        }
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<()> {
        self.out.push(b'd');
        encode::bytes(self.out, variant.as_bytes());
        self.write_non_empty(value, "None as enum payload")?;
        self.out.push(b'e');
        Ok(())
    }
    fn serialize_seq(self, _len: Option<usize>) -> Result<SeqSerializer<'s, 'o>> {
        self.out.push(b'l');
        Ok(SeqSerializer {
            ser: self,
            closers: 1,
        })
    }
    fn serialize_tuple(self, len: usize) -> Result<SeqSerializer<'s, 'o>> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<SeqSerializer<'s, 'o>> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<SeqSerializer<'s, 'o>> {
        self.out.push(b'd');
        encode::bytes(self.out, variant.as_bytes());
        self.out.push(b'l');
        Ok(SeqSerializer {
            ser: self,
            closers: 2,
        })
    }
    fn serialize_map(self, _len: Option<usize>) -> Result<DictSerializer<'s, 'o>> {
        Ok(DictSerializer {
            ser: self,
            entries: Vec::new(),
            pending_key: None,
            closers: 0,
        })
    }
    fn serialize_struct(self, _name: &'static str, len: usize) -> Result<DictSerializer<'s, 'o>> {
        self.serialize_map(Some(len))
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        _len: usize,
    ) -> Result<DictSerializer<'s, 'o>> {
        self.out.push(b'd');
        encode::bytes(self.out, variant.as_bytes());
        Ok(DictSerializer {
            ser: self,
            entries: Vec::new(),
            pending_key: None,
            closers: 1,
        })
    }

    fn is_human_readable(&self) -> bool {
        false
    }
}

/// Serializer state for lists, tuples and tuple variants.
#[derive(Debug)]
pub struct SeqSerializer<'s, 'o> {
    ser: &'s mut Serializer<'o>,
    closers: usize,
}

impl SeqSerializer<'_, '_> {
    fn element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<()> {
        self.ser.write_non_empty(value, "None inside a list")
    }

    fn finish(self) -> Result<()> {
        for _ in 0..self.closers {
            self.ser.out.push(b'e');
        }
        Ok(())
    }
}

impl ser::SerializeSeq for SeqSerializer<'_, '_> {
    type Ok = ();
    type Error = Error;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<()> {
        self.element(value)
    }
    fn end(self) -> Result<()> {
        self.finish()
    }
}

impl ser::SerializeTuple for SeqSerializer<'_, '_> {
    type Ok = ();
    type Error = Error;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<()> {
        self.element(value)
    }
    fn end(self) -> Result<()> {
        self.finish()
    }
}

impl ser::SerializeTupleStruct for SeqSerializer<'_, '_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<()> {
        self.element(value)
    }
    fn end(self) -> Result<()> {
        self.finish()
    }
}

impl ser::SerializeTupleVariant for SeqSerializer<'_, '_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<()> {
        self.element(value)
    }
    fn end(self) -> Result<()> {
        self.finish()
    }
}

/// Serializer state for maps, structs and struct variants.
///
/// Entries are buffered and sorted on [`end`](ser::SerializeMap::end) so the
/// output is canonical regardless of field or iteration order.
#[derive(Debug)]
pub struct DictSerializer<'s, 'o> {
    ser: &'s mut Serializer<'o>,
    entries: Vec<(Vec<u8>, Vec<u8>)>,
    pending_key: Option<Vec<u8>>,
    closers: usize,
}

impl DictSerializer<'_, '_> {
    fn entry<T: ?Sized + Serialize>(&mut self, key: Vec<u8>, value: &T) -> Result<()> {
        let value = Serializer::nested(value)?;
        if !value.is_empty() {
            self.entries.push((key, value));
        }
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        self.entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        if self
            .entries
            .windows(2)
            .any(|w| matches!(w, [a, b] if a.0 == b.0))
        {
            return Err(Error::DuplicateMapKey);
        }
        let out = &mut *self.ser.out;
        out.push(b'd');
        for (k, v) in &self.entries {
            encode::bytes(out, k);
            out.extend_from_slice(v);
        }
        out.push(b'e');
        for _ in 0..self.closers {
            out.push(b'e');
        }
        Ok(())
    }
}

impl ser::SerializeMap for DictSerializer<'_, '_> {
    type Ok = ();
    type Error = Error;
    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<()> {
        self.pending_key = Some(Serializer::key(key)?);
        Ok(())
    }
    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<()> {
        let key = self
            .pending_key
            .take()
            .ok_or_else(|| Error::Custom("serialize_value called before serialize_key".into()))?;
        self.entry(key, value)
    }
    fn end(self) -> Result<()> {
        self.finish()
    }
}

impl ser::SerializeStruct for DictSerializer<'_, '_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<()> {
        self.entry(key.as_bytes().to_vec(), value)
    }
    fn end(self) -> Result<()> {
        self.finish()
    }
}

impl ser::SerializeStructVariant for DictSerializer<'_, '_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<()> {
        self.entry(key.as_bytes().to_vec(), value)
    }
    fn end(self) -> Result<()> {
        self.finish()
    }
}
