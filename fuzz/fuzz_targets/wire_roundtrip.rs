//! decode -> encode -> decode must reproduce the same messages and bytes.
#![no_main]

use bytes::BytesMut;
use dravity_wire::MessageDecoder;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut buf = BytesMut::from(data);
    let mut dec = MessageDecoder::new();
    let mut consumed = 0;
    while let Ok(Some(msg)) = dec.decode(&mut buf) {
        let mut out = BytesMut::new();
        msg.encode(&mut out);
        assert_eq!(out.len(), msg.encoded_len());
        // Canonical framing: the encoding equals the input frame.
        let frame = data
            .get(consumed..consumed + out.len())
            .expect("frame in bounds");
        assert_eq!(&out[..], frame);
        consumed += out.len();
        let again = MessageDecoder::new()
            .decode(&mut out)
            .expect("re-decode")
            .expect("complete");
        assert_eq!(again, msg);
    }
});
