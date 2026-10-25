//! serde integration tests.

use std::collections::{BTreeMap, HashMap};

use dravity_bencode::{DecodeOptions, Error, RawValue, Value, from_bytes, from_bytes_with, to_vec};
use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Announce<'a> {
    interval: u32,
    #[serde(rename = "min interval", skip_serializing_if = "Option::is_none")]
    min_interval: Option<u32>,
    #[serde(borrow, with = "bytes")]
    peers: &'a [u8],
    #[serde(rename = "tracker id")]
    tracker_id: Option<String>,
    complete: i64,
    flag: bool,
}

mod bytes {
    use serde::{Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer>(b: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(b)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<&'de [u8], D::Error> {
        <&[u8]>::deserialize(d)
    }

    use serde::Deserialize;
}

#[test]
fn struct_roundtrip_is_canonical_and_skips_none() {
    let a = Announce {
        interval: 1800,
        min_interval: None,
        peers: b"\x7f\x00\x00\x01\x1a\xe1",
        tracker_id: Some("abc".into()),
        complete: -3,
        flag: true,
    };
    let enc = to_vec(&a).unwrap();
    assert_eq!(
        enc,
        b"d8:completei-3e4:flagi1e8:intervali1800e5:peers6:\x7f\x00\x00\x01\x1a\xe110:tracker id3:abce"
    );
    let dec: Announce<'_> = from_bytes(&enc).unwrap();
    assert_eq!(dec, a);
    // Strict decoding accepts canonical output.
    let strict: Announce<'_> = from_bytes_with(&enc, DecodeOptions::strict()).unwrap();
    assert_eq!(strict, a);
}

#[test]
fn borrows_str_and_bytes() {
    #[derive(Deserialize)]
    struct S<'a> {
        name: &'a str,
        #[serde(borrow)]
        raw: &'a [u8],
    }
    let input = b"d4:name3:foo3:raw2:\xff\x00e";
    let s: S<'_> = from_bytes(input).unwrap();
    assert_eq!(s.name, "foo");
    assert_eq!(s.raw, b"\xff\x00");
    assert!(std::ptr::eq(s.name.as_ptr(), input[9..].as_ptr()));
}

#[test]
fn vec_u8_accepts_byte_string_and_list() {
    let v: Vec<u8> = from_bytes(b"3:abc").unwrap();
    assert_eq!(v, b"abc");
    let v: Vec<u8> = from_bytes(b"li1ei2ee").unwrap();
    assert_eq!(v, [1, 2]);
    let a: [u8; 2] = from_bytes(b"2:hi").unwrap();
    assert_eq!(&a, b"hi");
    assert!(from_bytes::<[u8; 2]>(b"3:hey").is_err());
}

#[test]
fn ignores_unknown_fields_including_non_utf8_keys() {
    #[derive(Deserialize, Debug, PartialEq)]
    struct S {
        a: i64,
    }
    let s: S = from_bytes(b"d1:ai1e2:\xff\xfeli1eee").unwrap();
    assert_eq!(s, S { a: 1 });
}

#[test]
fn missing_and_duplicate_fields() {
    #[derive(Deserialize, Debug)]
    #[allow(dead_code)]
    struct S {
        a: i64,
    }
    assert!(from_bytes::<S>(b"de").is_err());
    let err = from_bytes::<S>(b"d1:ai1e1:ai2ee").unwrap_err();
    assert_eq!(err, Error::DuplicateKey { offset: 7 });
}

#[test]
fn integers_range_checked() {
    assert_eq!(from_bytes::<u8>(b"i255e").unwrap(), 255);
    assert!(from_bytes::<u8>(b"i256e").is_err());
    assert!(from_bytes::<u32>(b"i-1e").is_err());
    assert_eq!(
        from_bytes::<u64>(b"i18446744073709551615e").unwrap(),
        u64::MAX
    );
    assert_eq!(
        from_bytes::<i64>(b"i-9223372036854775808e").unwrap(),
        i64::MIN
    );
    assert!(matches!(
        from_bytes::<u64>(b"i18446744073709551616e"),
        Err(Error::IntegerOverflow { offset: 0 })
    ));
    assert_eq!(to_vec(&u64::MAX).unwrap(), b"i18446744073709551615e");
    assert!(from_bytes::<bool>(b"i2e").is_err());
}

#[test]
fn rejects_unrepresentable_types() {
    assert_eq!(
        to_vec(&1.5f64).unwrap_err(),
        Error::Unsupported("floating point")
    );
    assert_eq!(to_vec(&()).unwrap_err(), Error::Unsupported("unit"));
    assert_eq!(
        to_vec(&None::<u8>).unwrap_err(),
        Error::Unsupported("top-level None")
    );
    assert_eq!(
        to_vec(&vec![Some(1), None]).unwrap_err(),
        Error::Unsupported("None inside a list")
    );
    let mut m = HashMap::new();
    m.insert(1u8, 2u8);
    assert_eq!(to_vec(&m).unwrap_err(), Error::MapKeyNotBytes);
}

#[test]
fn maps_are_sorted_by_raw_bytes() {
    let mut m = HashMap::new();
    for k in ["zeta", "alpha", "Beta", "a"] {
        m.insert(k.to_owned(), k.len());
    }
    assert_eq!(to_vec(&m).unwrap(), b"d4:Betai4e1:ai1e5:alphai5e4:zetai4ee");
    let back: BTreeMap<String, usize> = from_bytes(&to_vec(&m).unwrap()).unwrap();
    assert_eq!(back.len(), 4);
}

#[test]
fn enums() {
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    enum E {
        Unit,
        New(u8),
        Tuple(u8, String),
        Struct { x: i32 },
    }
    for (v, enc) in [
        (E::Unit, &b"4:Unit"[..]),
        (E::New(7), b"d3:Newi7ee"),
        (E::Tuple(1, "a".into()), b"d5:Tupleli1e1:aee"),
        (E::Struct { x: -1 }, b"d6:Structd1:xi-1eee"),
    ] {
        assert_eq!(to_vec(&v).unwrap(), enc);
        assert_eq!(from_bytes::<E>(enc).unwrap(), v);
    }
}

#[test]
fn raw_value_captures_exact_bytes() {
    #[derive(Deserialize, Serialize)]
    struct T<'a> {
        #[serde(borrow)]
        info: RawValue<'a>,
        other: i64,
    }
    // Inner dict deliberately non-canonical (unsorted keys).
    let input = b"d4:infod1:bi1e1:ai2ee5:otheri0ee";
    let t: T<'_> = from_bytes(input).unwrap();
    assert_eq!(t.info.as_bytes(), b"d1:bi1e1:ai2ee");
    // Serializing splices the raw bytes back verbatim.
    assert_eq!(to_vec(&t).unwrap(), input);
    assert_eq!(t.info.to_value().unwrap().get(b"a"), Some(&Value::Int(2)));

    // RawValue only works with this crate's deserializer types.
    assert!(RawValue::from_bytes(b"i1ei2e").is_err());
}

#[test]
fn value_via_serde_matches_native_decoder() {
    let input = b"d1:ad1:bli1e2:xyee1:ci-5ee";
    let a: Value<'_> = from_bytes(input).unwrap();
    let b = Value::decode(input).unwrap();
    assert_eq!(a, b);
    assert_eq!(to_vec(&a).unwrap(), input);
}

#[test]
fn trailing_data_and_depth() {
    assert_eq!(
        from_bytes::<i64>(b"i1ex").unwrap_err(),
        Error::TrailingData { offset: 3 }
    );
    let deep = "l".repeat(1000) + &"e".repeat(1000);
    assert!(matches!(
        from_bytes::<Value<'_>>(deep.as_bytes()),
        Err(Error::DepthLimitExceeded { .. })
    ));
    assert!(matches!(
        from_bytes::<serde::de::IgnoredAny>(deep.as_bytes()),
        Err(Error::DepthLimitExceeded { .. })
    ));
}
