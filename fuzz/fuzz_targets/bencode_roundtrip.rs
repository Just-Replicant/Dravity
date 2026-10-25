//! decode -> encode -> decode must be stable and canonical.
#![no_main]

use dravity_bencode::{DecodeOptions, Value, to_vec};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(v) = Value::decode(data) else { return };
    let encoded = v.to_vec();
    let again = Value::decode_with(&encoded, DecodeOptions::strict())
        .expect("canonical encoding must decode strictly");
    assert_eq!(again, v);
    assert_eq!(to_vec(&v).expect("serde encoding"), encoded);
});
