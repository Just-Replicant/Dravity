//! Property tests for the wire codec: roundtrips under arbitrary fragmentation
//! and robustness on arbitrary input.

use bytes::{Bytes, BytesMut};
use dravity_core::{InfoHash, PeerId};
use dravity_wire::{
    BlockRequest, Handshake, Message, MessageDecoder, MessageId, MetadataMessage, PexMessage,
    PexPeer, Reserved, WireError,
};
use proptest::prelude::*;

fn arb_bytes(max: usize) -> impl Strategy<Value = Bytes> {
    proptest::collection::vec(any::<u8>(), 0..max).prop_map(Bytes::from)
}

fn arb_request() -> impl Strategy<Value = BlockRequest> {
    (any::<u32>(), any::<u32>(), any::<u32>()).prop_map(|(piece, offset, length)| BlockRequest {
        piece,
        offset,
        length,
    })
}

fn arb_message() -> impl Strategy<Value = Message> {
    prop_oneof![
        Just(Message::KeepAlive),
        Just(Message::Choke),
        Just(Message::Unchoke),
        Just(Message::Interested),
        Just(Message::NotInterested),
        any::<u32>().prop_map(|piece| Message::Have { piece }),
        arb_bytes(64).prop_map(Message::Bitfield),
        arb_request().prop_map(Message::Request),
        arb_request().prop_map(Message::Cancel),
        (any::<u32>(), any::<u32>(), arb_bytes(256)).prop_map(|(piece, offset, data)| {
            Message::Piece {
                piece,
                offset,
                data,
            }
        }),
        any::<u16>().prop_map(Message::Port),
        any::<u32>().prop_map(|piece| Message::Suggest { piece }),
        Just(Message::HaveAll),
        Just(Message::HaveNone),
        arb_request().prop_map(Message::Reject),
        any::<u32>().prop_map(|piece| Message::AllowedFast { piece }),
        (any::<u8>(), arb_bytes(32)).prop_map(|(id, payload)| Message::Extended { id, payload }),
        (
            any::<u8>().prop_filter("unknown id", |id| MessageId::try_from(*id).is_err()),
            arb_bytes(32)
        )
            .prop_map(|(id, payload)| Message::Unknown { id, payload }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn message_roundtrip_fragmented(
        msgs in proptest::collection::vec(arb_message(), 0..16),
        chunk in 1usize..64,
    ) {
        let mut wire = BytesMut::new();
        for m in &msgs {
            m.encode(&mut wire).unwrap();
        }
        let expected_len: usize = msgs.iter().map(Message::encoded_len).sum();
        prop_assert_eq!(wire.len(), expected_len);

        let mut dec = MessageDecoder::new();
        let mut buf = BytesMut::new();
        let mut out = Vec::new();
        for piece in wire.chunks(chunk) {
            buf.extend_from_slice(piece);
            while let Some(m) = dec.decode(&mut buf).unwrap() {
                out.push(m);
            }
        }
        prop_assert!(buf.is_empty());
        prop_assert_eq!(out, msgs);
    }

    #[test]
    fn handshake_roundtrip(r in any::<[u8; 8]>(), ih in any::<[u8; 20]>(), id in any::<[u8; 20]>()) {
        let hs = Handshake::new(InfoHash::new(ih), PeerId::new(id)).with_reserved(Reserved::from_bytes(r));
        let mut buf = BytesMut::from(&hs.to_bytes()[..]);
        buf.extend_from_slice(b"rest");
        prop_assert_eq!(Handshake::decode(&mut buf).unwrap(), Some(hs));
        prop_assert_eq!(&buf[..], b"rest");
    }

    #[test]
    fn decode_never_panics(input in proptest::collection::vec(any::<u8>(), 0..512), max in 0u32..1024) {
        let mut buf = BytesMut::from(&input[..]);
        let mut dec = MessageDecoder::with_max_frame_len(max);
        loop {
            match dec.decode(&mut buf) {
                Ok(Some(m)) => {
                    // Anything decoded re-encodes to a decodable frame.
                    let mut re = BytesMut::new();
                    m.encode(&mut re).unwrap();
                    prop_assert_eq!(MessageDecoder::new().decode(&mut re).unwrap(), Some(m));
                }
                Ok(None) | Err(WireError::FrameTooLarge { .. } | WireError::InvalidPayloadLength { .. }) => break,
                Err(e) => prop_assert!(false, "unexpected error {e:?}"),
            }
        }
        let mut buf = BytesMut::from(&input[..]);
        let _ = Handshake::decode(&mut buf);
    }
}

fn arb_addr() -> impl Strategy<Value = std::net::SocketAddr> {
    prop_oneof![
        (any::<[u8; 4]>(), any::<u16>()).prop_map(|(ip, p)| std::net::SocketAddr::from((ip, p))),
        (any::<[u8; 16]>(), any::<u16>()).prop_map(|(ip, p)| std::net::SocketAddr::from((ip, p))),
    ]
}

proptest! {
    #[test]
    fn metadata_roundtrip(kind in 0u8..3, piece in any::<u32>(), size in 0u64..i64::MAX as u64, data in arb_bytes(512)) {
        let msg = match kind {
            0 => MetadataMessage::Request { piece },
            1 => MetadataMessage::Data { piece, total_size: size, data },
            _ => MetadataMessage::Reject { piece },
        };
        prop_assert_eq!(MetadataMessage::decode(&msg.encode().unwrap()).unwrap(), msg);
    }

    #[test]
    fn pex_roundtrip(
        added in proptest::collection::vec((arb_addr(), any::<u8>()), 0..60),
        dropped in proptest::collection::vec(arb_addr(), 0..60),
    ) {
        // Decoding yields IPv4 entries before IPv6 ones.
        let order = |a: &std::net::SocketAddr| a.is_ipv6();
        let mut added: Vec<PexPeer> = added.into_iter().map(|(addr, flags)| PexPeer { addr, flags }).collect();
        added.sort_by_key(|p| order(&p.addr));
        let mut dropped = dropped;
        dropped.sort_by_key(order);
        let msg = PexMessage { added, dropped };
        prop_assert_eq!(PexMessage::decode(&msg.encode()).unwrap(), msg);
    }

    #[test]
    fn extension_payloads_never_panic(input in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = PexMessage::decode(&input);
        let _ = MetadataMessage::decode(&Bytes::from(input));
    }
}
