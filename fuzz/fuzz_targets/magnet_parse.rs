//! Magnet parsing must never panic, and format -> parse must be lossless.
#![no_main]

use dravity_core::Magnet;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(m) = Magnet::parse(s) else { return };
    let reparsed = Magnet::parse(&m.to_uri()).expect("formatted magnet must parse");
    assert_eq!(reparsed, m);
});
