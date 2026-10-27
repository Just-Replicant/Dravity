//! UDP tracker datagrams (BEP 15) from an untrusted tracker.
#![no_main]

use dravity_tracker::udp;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&flags, packet)) = data.split_first() else {
        return;
    };
    let tid = packet
        .get(4..8)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u32::from_be_bytes);
    let _ = udp::parse_connect_response(packet, tid);
    if let Ok(response) = udp::parse_announce_response(packet, tid, flags & 1 == 1) {
        let encoded = udp::encode_announce_response(
            tid,
            u32::try_from(response.interval.unwrap_or_default().as_secs()).unwrap_or(0),
            response.leechers.unwrap_or(0),
            response.seeders.unwrap_or(0),
            &response.peers,
        );
        assert_eq!(encoded, packet);
    }
});
