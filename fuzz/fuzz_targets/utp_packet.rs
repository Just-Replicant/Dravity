//! Arbitrary datagrams through the uTP packet decoder and a connection.
#![no_main]

use std::time::{Duration, Instant};

use dravity_utp::{Config, Connection, Packet, PacketType};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(packet) = Packet::decode(data) else {
        return;
    };
    // Re-encoding is stable (SACK bytes are padded once).
    let once = packet.encode();
    let again = Packet::decode(&once).expect("own encoding decodes");
    assert_eq!(again.encode(), once);

    // Any packet, in any state, must not panic the state machine.
    let now = Instant::now();
    let mut syn = Packet::new(PacketType::Syn, packet.conn_id);
    syn.seq_nr = packet.ack_nr;
    let mut accepted = Connection::accept(Config::default(), now, &syn);
    let mut dialed = Connection::connect(Config::default(), now, packet.conn_id);
    for conn in [&mut accepted, &mut dialed] {
        while conn.poll_transmit(now).is_some() {}
        conn.on_packet(now, &packet);
        let mut buf = [0u8; 2048];
        let _ = conn.read(&mut buf);
        let _ = conn.write(&packet.payload);
        let later = now + Duration::from_secs(2);
        conn.on_timeout(later);
        while conn.poll_transmit(later).is_some() {}
    }
});
