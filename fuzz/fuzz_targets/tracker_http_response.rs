//! HTTP tracker announce responses from an untrusted tracker.
#![no_main]

use dravity_bencode::Value;
use dravity_tracker::http::parse_announce_response;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(response) = parse_announce_response(data) else {
        return;
    };
    // Compact peer lists yield exactly one address per 6/18-byte record.
    let Ok(value) = Value::decode(data) else {
        return;
    };
    let compact = |key: &[u8], size: usize| match value.get(key) {
        Some(Value::Bytes(b)) => b.len() / size,
        _ => 0,
    };
    if matches!(value.get(b"peers"), Some(Value::Bytes(_)) | None) {
        assert_eq!(
            response.peers.len(),
            compact(b"peers", 6) + compact(b"peers6", 18)
        );
    }
});
