//! BEP 11 `ut_pex` payloads from an untrusted peer.
#![no_main]

use dravity_wire::{MAX_PEX_ENTRIES, PexMessage};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(msg) = PexMessage::decode(data) {
        assert!(msg.added.len() <= 2 * MAX_PEX_ENTRIES);
        assert!(msg.dropped.len() <= 2 * MAX_PEX_ENTRIES);
        // Flags are unknown (`None`) only past the end of a short `added.f`
        // or `added6.f`, so re-encoding them as 0 never shifts a later flag.
        for family in [true, false] {
            let flags = msg.added.iter().filter(|p| p.addr.is_ipv4() == family);
            let known = flags.clone().take_while(|p| p.flags.is_some()).count();
            assert!(flags.skip(known).all(|p| p.flags.is_none()));
        }
        assert_eq!(PexMessage::decode(&msg.encode()).ok(), Some(msg));
    }
});
