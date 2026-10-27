//! BEP 11 `ut_pex` payloads from an untrusted peer.
#![no_main]

use dravity_wire::{MAX_PEX_ENTRIES, PexMessage};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(msg) = PexMessage::decode(data) {
        assert!(msg.added.len() <= 2 * MAX_PEX_ENTRIES);
        assert!(msg.dropped.len() <= 2 * MAX_PEX_ENTRIES);
        assert_eq!(PexMessage::decode(&msg.encode()).ok(), Some(msg));
    }
});
