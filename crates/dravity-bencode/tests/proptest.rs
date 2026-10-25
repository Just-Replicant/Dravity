//! Property tests: encode/decode roundtrips and robustness on arbitrary input.

use std::borrow::Cow;

use dravity_bencode::{DecodeOptions, Dict, Value, from_bytes, to_vec};
use proptest::prelude::*;

fn arb_value() -> impl Strategy<Value = Value<'static>> {
    let leaf = prop_oneof![
        any::<i64>().prop_map(Value::Int),
        proptest::collection::vec(any::<u8>(), 0..32).prop_map(|b| Value::Bytes(Cow::Owned(b))),
    ];
    leaf.prop_recursive(6, 128, 8, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..8).prop_map(Value::List),
            proptest::collection::btree_map(
                proptest::collection::vec(any::<u8>(), 0..12).prop_map(Cow::Owned),
                inner,
                0..8
            )
            .prop_map(|m: Dict<'static>| Value::Dict(m)),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn value_roundtrip(v in arb_value()) {
        let enc = v.to_vec();
        let strict = Value::decode_with(&enc, DecodeOptions::strict()).unwrap();
        prop_assert_eq!(&strict, &v);
        prop_assert_eq!(Value::decode(&enc).unwrap(), v.clone());
        // serde path produces identical bytes and values.
        prop_assert_eq!(to_vec(&v).unwrap(), enc.clone());
        let via_serde: Value<'_> = from_bytes(&enc).unwrap();
        prop_assert_eq!(via_serde, v);
    }

    #[test]
    fn decode_never_panics(input in proptest::collection::vec(any::<u8>(), 0..256)) {
        let lenient = Value::decode(&input);
        let strict = Value::decode_with(&input, DecodeOptions::strict());
        // Strict acceptance implies lenient acceptance and canonical re-encoding.
        if let Ok(v) = strict {
            prop_assert_eq!(v.to_vec(), input.clone());
            prop_assert!(lenient.is_ok());
        }
        let _ = from_bytes::<Value<'_>>(&input);
        let _ = from_bytes::<serde::de::IgnoredAny>(&input);
    }

    #[test]
    fn mutated_encodings_never_panic(v in arb_value(), idx in any::<prop::sample::Index>(), byte in any::<u8>()) {
        let mut enc = v.to_vec();
        let i = idx.index(enc.len());
        enc[i] = byte;
        if let Ok(decoded) = Value::decode(&enc) {
            // Whatever decodes must re-encode to something that decodes to the same value.
            let reenc = decoded.to_vec();
            prop_assert_eq!(Value::decode(&reenc).unwrap(), decoded);
        }
    }

    #[test]
    fn serde_struct_roundtrip(a in any::<u32>(), b in any::<Option<i64>>(), s in ".{0,16}", list in proptest::collection::vec(any::<u16>(), 0..8)) {
        #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
        struct S {
            a: u32,
            b: Option<i64>,
            s: String,
            list: Vec<u16>,
        }
        let v = S { a, b, s, list };
        let enc = to_vec(&v).unwrap();
        prop_assert_eq!(from_bytes::<S>(&enc).unwrap(), v);
    }
}
