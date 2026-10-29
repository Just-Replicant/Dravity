//! `Range` request headers from HTTP clients.
#![no_main]

use dravity_http::range;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: (u64, &str)| {
    let (len, header) = input;
    if let Ok(Some(r)) = range::parse(header, len) {
        assert!(r.start <= r.end && r.end < len);
    }
});
