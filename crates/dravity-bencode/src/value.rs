//! A dynamically typed, zero-copy bencode value.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;

use crate::encode;
use crate::error::{Error, Result};
use crate::parser::{DecodeOptions, Parser};

/// Dictionary type used by [`Value::Dict`]. Keys are kept in sorted (canonical) order.
pub type Dict<'a> = BTreeMap<Cow<'a, [u8]>, Value<'a>>;

/// A bencode value that borrows byte strings from the input where possible.
///
/// ```
/// use dravity_bencode::Value;
///
/// let v = Value::decode(b"d3:cow3:moo4:spaml1:a1:bee").unwrap();
/// assert_eq!(v.get(b"cow").and_then(Value::as_str), Some("moo"));
/// assert_eq!(v.to_vec(), b"d3:cow3:moo4:spaml1:a1:bee");
/// ```
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Value<'a> {
    /// `i<n>e`
    Int(i64),
    /// `<len>:<bytes>`
    Bytes(Cow<'a, [u8]>),
    /// `l...e`
    List(Vec<Value<'a>>),
    /// `d...e`
    Dict(Dict<'a>),
}

impl<'a> Value<'a> {
    /// Decodes exactly one value from `input` with [`DecodeOptions::default`].
    ///
    /// # Errors
    /// Returns an [`Error`] for malformed input or trailing data.
    pub fn decode(input: &'a [u8]) -> Result<Self> {
        Self::decode_with(input, DecodeOptions::default())
    }

    /// Decodes exactly one value from `input` with custom options.
    ///
    /// # Errors
    /// Returns an [`Error`] for malformed input or trailing data.
    pub fn decode_with(input: &'a [u8], opts: DecodeOptions) -> Result<Self> {
        let mut p = Parser::new(input, opts);
        let v = Self::parse(&mut p)?;
        p.finish()?;
        Ok(v)
    }

    /// Decodes one value from the start of `input` and returns it with the
    /// number of bytes it used. Trailing bytes are allowed, e.g. the raw
    /// piece data after a BEP 9 `ut_metadata` dictionary.
    ///
    /// ```
    /// use dravity_bencode::Value;
    ///
    /// let (v, used) = Value::decode_prefix(b"i42eRAW").unwrap();
    /// assert_eq!((v, used), (Value::Int(42), 4));
    /// ```
    ///
    /// # Errors
    /// Returns an [`Error`] for malformed input.
    pub fn decode_prefix(input: &'a [u8]) -> Result<(Self, usize)> {
        let mut p = Parser::new(input, DecodeOptions::default());
        let v = Self::parse(&mut p)?;
        Ok((v, p.pos()))
    }

    pub(crate) fn parse(p: &mut Parser<'a>) -> Result<Self> {
        match p.peek()? {
            b'i' => p.int_i64().map(Value::Int),
            b'0'..=b'9' => p.byte_string().map(|(b, _)| Value::Bytes(Cow::Borrowed(b))),
            b'l' => {
                p.enter(b'l')?;
                let mut items = Vec::new();
                while !p.eat_end()? {
                    p.check_items(items.len())?;
                    items.push(Self::parse(p)?);
                }
                p.leave();
                Ok(Value::List(items))
            }
            b'd' => {
                p.enter(b'd')?;
                let mut dict = Dict::new();
                let mut prev: Option<&[u8]> = None;
                let mut seen = 0usize;
                while !p.eat_end()? {
                    p.check_items(seen)?;
                    seen += 1;
                    let (key, offset) = p.dict_key()?;
                    p.check_key(prev, key, offset)?;
                    prev = Some(key);
                    let value = Self::parse(p)?;
                    if dict.insert(Cow::Borrowed(key), value).is_some() {
                        return Err(Error::DuplicateKey { offset });
                    }
                }
                p.leave();
                Ok(Value::Dict(dict))
            }
            byte => Err(Error::UnexpectedByte {
                byte,
                offset: p.pos(),
            }),
        }
    }

    /// Appends the canonical encoding of this value to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Value::Int(i) => encode::int(out, *i),
            Value::Bytes(b) => encode::bytes(out, b),
            Value::List(items) => {
                out.push(b'l');
                for item in items {
                    item.encode(out);
                }
                out.push(b'e');
            }
            Value::Dict(dict) => {
                out.push(b'd');
                for (k, v) in dict {
                    encode::bytes(out, k);
                    v.encode(out);
                }
                out.push(b'e');
            }
        }
    }

    /// Returns the canonical encoding of this value.
    #[must_use]
    pub fn to_vec(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode(&mut out);
        out
    }

    /// Converts into a value that owns all of its data.
    #[must_use]
    pub fn into_owned(self) -> Value<'static> {
        match self {
            Value::Int(i) => Value::Int(i),
            Value::Bytes(b) => Value::Bytes(Cow::Owned(b.into_owned())),
            Value::List(items) => Value::List(items.into_iter().map(Value::into_owned).collect()),
            Value::Dict(dict) => Value::Dict(
                dict.into_iter()
                    .map(|(k, v)| (Cow::Owned(k.into_owned()), v.into_owned()))
                    .collect(),
            ),
        }
    }

    /// Returns the integer if this is [`Value::Int`].
    #[must_use]
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// Returns the bytes if this is [`Value::Bytes`].
    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    /// Returns the bytes as UTF-8 text if this is [`Value::Bytes`] holding valid UTF-8.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        self.as_bytes().and_then(|b| std::str::from_utf8(b).ok())
    }

    /// Returns the items if this is [`Value::List`].
    #[must_use]
    pub fn as_list(&self) -> Option<&[Value<'a>]> {
        match self {
            Value::List(items) => Some(items),
            _ => None,
        }
    }

    /// Returns the dictionary if this is [`Value::Dict`].
    #[must_use]
    pub fn as_dict(&self) -> Option<&Dict<'a>> {
        match self {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }

    /// Looks up `key` if this is a dictionary.
    #[must_use]
    pub fn get(&self, key: &[u8]) -> Option<&Value<'a>> {
        self.as_dict().and_then(|d| d.get(key))
    }
}

impl From<i64> for Value<'_> {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}

impl<'a> From<&'a [u8]> for Value<'a> {
    fn from(b: &'a [u8]) -> Self {
        Value::Bytes(Cow::Borrowed(b))
    }
}

impl<'a> From<&'a str> for Value<'a> {
    fn from(s: &'a str) -> Self {
        Value::Bytes(Cow::Borrowed(s.as_bytes()))
    }
}

impl From<Vec<u8>> for Value<'_> {
    fn from(b: Vec<u8>) -> Self {
        Value::Bytes(Cow::Owned(b))
    }
}

impl From<String> for Value<'_> {
    fn from(s: String) -> Self {
        Value::Bytes(Cow::Owned(s.into_bytes()))
    }
}

impl<'a> From<Vec<Value<'a>>> for Value<'a> {
    fn from(items: Vec<Value<'a>>) -> Self {
        Value::List(items)
    }
}

impl<'a> From<Dict<'a>> for Value<'a> {
    fn from(d: Dict<'a>) -> Self {
        Value::Dict(d)
    }
}

/// Debug output renders byte strings as escaped ASCII, which is far more
/// readable than a list of numbers.
impl fmt::Debug for Value<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct Bstr<'b>(&'b [u8]);
        impl fmt::Debug for Bstr<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "b\"{}\"", self.0.escape_ascii())
            }
        }
        match self {
            Value::Int(i) => write!(f, "{i}"),
            Value::Bytes(b) => Bstr(b).fmt(f),
            Value::List(items) => f.debug_list().entries(items).finish(),
            Value::Dict(d) => f
                .debug_map()
                .entries(d.iter().map(|(k, v)| (Bstr(k), v)))
                .finish(),
        }
    }
}

mod serde_impls {
    use std::borrow::Cow;
    use std::fmt;
    use std::marker::PhantomData;

    use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
    use serde::ser::{Serialize, Serializer};

    use super::{Dict, Value};

    pub(crate) struct ByteSlice<'b>(pub(crate) &'b [u8]);

    impl Serialize for ByteSlice<'_> {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_bytes(self.0)
        }
    }

    impl Serialize for Value<'_> {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            match self {
                Value::Int(i) => s.serialize_i64(*i),
                Value::Bytes(b) => s.serialize_bytes(b),
                Value::List(items) => s.collect_seq(items),
                Value::Dict(d) => s.collect_map(d.iter().map(|(k, v)| (ByteSlice(k), v))),
            }
        }
    }

    struct CowBytes<'a>(Cow<'a, [u8]>);

    impl<'de: 'a, 'a> Deserialize<'de> for CowBytes<'a> {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct V<'a>(PhantomData<&'a ()>);
            impl<'de: 'a, 'a> Visitor<'de> for V<'a> {
                type Value = CowBytes<'a>;
                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("a byte string")
                }
                fn visit_borrowed_bytes<E>(self, v: &'de [u8]) -> Result<Self::Value, E> {
                    Ok(CowBytes(Cow::Borrowed(v)))
                }
                fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Self::Value, E> {
                    Ok(CowBytes(Cow::Borrowed(v.as_bytes())))
                }
                fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E> {
                    Ok(CowBytes(Cow::Owned(v.to_vec())))
                }
                fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
                    Ok(CowBytes(Cow::Owned(v.as_bytes().to_vec())))
                }
            }
            d.deserialize_bytes(V(PhantomData))
        }
    }

    impl<'de: 'a, 'a> Deserialize<'de> for Value<'a> {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct V<'a>(PhantomData<&'a ()>);
            impl<'de: 'a, 'a> Visitor<'de> for V<'a> {
                type Value = Value<'a>;
                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("a bencode value")
                }
                fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
                    Ok(Value::Int(i64::from(v)))
                }
                fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
                    Ok(Value::Int(v))
                }
                fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                    i64::try_from(v)
                        .map(Value::Int)
                        .map_err(|_| E::invalid_value(de::Unexpected::Unsigned(v), &self))
                }
                fn visit_borrowed_bytes<E>(self, v: &'de [u8]) -> Result<Self::Value, E> {
                    Ok(Value::Bytes(Cow::Borrowed(v)))
                }
                fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Self::Value, E> {
                    Ok(Value::Bytes(Cow::Borrowed(v.as_bytes())))
                }
                fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E> {
                    Ok(Value::Bytes(Cow::Owned(v.to_vec())))
                }
                fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
                    Ok(Value::Bytes(Cow::Owned(v.as_bytes().to_vec())))
                }
                fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                    let mut items = Vec::new();
                    while let Some(item) = seq.next_element()? {
                        items.push(item);
                    }
                    Ok(Value::List(items))
                }
                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                    let mut dict = Dict::new();
                    while let Some((CowBytes(k), v)) = map.next_entry()? {
                        if dict.insert(k, v).is_some() {
                            return Err(de::Error::custom("duplicate dictionary key"));
                        }
                    }
                    Ok(Value::Dict(dict))
                }
            }
            d.deserialize_any(V(PhantomData))
        }
    }
}

pub(crate) use serde_impls::ByteSlice;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_spec_examples() {
        assert_eq!(Value::decode(b"4:spam").unwrap(), Value::from("spam"));
        assert_eq!(Value::decode(b"0:").unwrap(), Value::from(""));
        assert_eq!(Value::decode(b"i3e").unwrap(), Value::Int(3));
        assert_eq!(Value::decode(b"i-3e").unwrap(), Value::Int(-3));
        assert_eq!(Value::decode(b"i0e").unwrap(), Value::Int(0));
        assert_eq!(
            Value::decode(b"l4:spam4:eggse").unwrap(),
            Value::List(vec!["spam".into(), "eggs".into()])
        );
        let d = Value::decode(b"d4:spaml1:a1:bee").unwrap();
        assert_eq!(d.get(b"spam").unwrap().as_list().unwrap().len(), 2);
    }

    #[test]
    fn decode_is_zero_copy() {
        let input = b"l5:helloe";
        let v = Value::decode(input).unwrap();
        let Value::List(items) = &v else {
            panic!("not a list")
        };
        let Value::Bytes(Cow::Borrowed(b)) = &items[0] else {
            panic!("copied")
        };
        assert!(std::ptr::eq(b.as_ptr(), input[3..].as_ptr()));
    }

    #[test]
    fn rejects_malformed() {
        let cases: &[(&[u8], Error)] = &[
            (b"", Error::UnexpectedEof { offset: 0 }),
            (b"i-0e", Error::InvalidInteger { offset: 0 }),
            (b"i03e", Error::InvalidInteger { offset: 0 }),
            (b"ie", Error::InvalidInteger { offset: 0 }),
            (b"i1", Error::UnexpectedEof { offset: 2 }),
            (
                b"i9223372036854775808e",
                Error::IntegerOverflow { offset: 0 },
            ),
            (b"l", Error::UnexpectedEof { offset: 1 }),
            (
                b"x",
                Error::UnexpectedByte {
                    byte: b'x',
                    offset: 0,
                },
            ),
            (b"i1ei2e", Error::TrailingData { offset: 3 }),
            (b"di1ei2ee", Error::KeyNotBytes { offset: 1 }),
            (b"d1:a0:1:a0:e", Error::DuplicateKey { offset: 6 }),
            (b"d1:b0:1:c0:1:b0:e", Error::DuplicateKey { offset: 11 }),
        ];
        for (input, err) in cases {
            assert_eq!(
                Value::decode(input).unwrap_err(),
                *err,
                "{}",
                input.escape_ascii()
            );
        }
    }

    #[test]
    fn key_order_strictness() {
        let unsorted = b"d1:b0:1:a0:e";
        assert!(Value::decode(unsorted).is_ok());
        assert_eq!(
            Value::decode_with(unsorted, DecodeOptions::strict()).unwrap_err(),
            Error::UnsortedKeys { offset: 6 }
        );
        // Re-encoding canonicalises.
        assert_eq!(Value::decode(unsorted).unwrap().to_vec(), b"d1:a0:1:b0:e");
    }

    #[test]
    fn debug_is_readable() {
        let v = Value::decode(b"d1:ai1e1:bl3:x\x00yee").unwrap();
        assert_eq!(format!("{v:?}"), r#"{b"a": 1, b"b": [b"x\x00y"]}"#);
    }

    #[test]
    fn item_limit_bounds_lists_and_dicts() {
        let list = b"li1ei2ei3ee";
        let tight = DecodeOptions::new().max_items(2);
        assert!(matches!(
            Value::decode_with(list, tight),
            Err(Error::TooManyItems { max: 2, .. })
        ));
        assert!(Value::decode_with(list, DecodeOptions::new().max_items(3)).is_ok());
        assert!(matches!(
            crate::from_bytes_with::<Value<'_>>(list, tight),
            Err(Error::TooManyItems { max: 2, .. })
        ));
        assert!(matches!(
            crate::from_bytes_with::<serde::de::IgnoredAny>(list, tight),
            Err(Error::TooManyItems { max: 2, .. })
        ));
        let dict = b"d1:ai1e1:bi2e1:ci3ee";
        assert!(matches!(
            Value::decode_with(dict, tight),
            Err(Error::TooManyItems { max: 2, .. })
        ));
        assert!(Value::decode(list).is_ok());
    }

    #[test]
    fn into_owned_outlives_input() {
        let owned = {
            let input = b"d1:kl1:vee".to_vec();
            Value::decode(&input).unwrap().into_owned()
        };
        assert_eq!(owned.to_vec(), b"d1:kl1:vee");
    }
}
