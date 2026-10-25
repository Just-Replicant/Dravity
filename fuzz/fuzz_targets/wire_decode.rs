//! Streaming handshake + message decoding of arbitrary peer input, with the
//! frame limit chosen by the fuzzer.
#![no_main]

use bytes::BytesMut;
use dravity_wire::{Handshake, MessageDecoder};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&limit, rest)) = data.split_first() else {
        return;
    };
    let mut buf = BytesMut::from(rest);
    if let Ok(Some(_)) = Handshake::decode(&mut buf) {
        // Peer sent a valid handshake: continue with messages.
    } else {
        buf = BytesMut::from(rest);
    }
    let mut dec = MessageDecoder::with_max_frame_len(u32::from(limit) * 64);
    while let Ok(Some(_)) = dec.decode(&mut buf) {}
});
