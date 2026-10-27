//! BEP 9 `ut_metadata` payloads from an untrusted peer.
#![no_main]

use bytes::Bytes;
use dravity_wire::MetadataMessage;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(msg) = MetadataMessage::decode(&Bytes::copy_from_slice(data)) {
        // Anything decoded re-encodes and decodes to the same message.
        let encoded = msg.encode().expect("decoded message re-encodes");
        assert_eq!(MetadataMessage::decode(&encoded).ok(), Some(msg));
    }
});
