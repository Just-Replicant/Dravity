//! Arbitrary bytes through every bencode decoding entry point.
#![no_main]

use dravity_bencode::{DecodeOptions, RawValue, Value, from_bytes};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let lenient = Value::decode(data);
    if let Ok(v) = Value::decode_with(data, DecodeOptions::strict()) {
        // Strictly valid input is canonical: re-encoding reproduces it exactly.
        assert_eq!(v.to_vec(), data);
        assert!(lenient.is_ok());
    }
    let _ = from_bytes::<serde::de::IgnoredAny>(data);
    let _ = from_bytes::<Value<'_>>(data);
    if let Ok(raw) = RawValue::from_bytes(data) {
        assert_eq!(raw.as_bytes(), data);
    }
});
