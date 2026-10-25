//! Property tests for the wire codec: roundtrips under arbitrary fragmentation
//! and robustness on arbitrary input.

use bytes::{Bytes, BytesMut};
use dravity_core::{InfoHash, PeerId};
use dravity_wire::{
    BlockRequest, Handshake, Message, MessageDecoder, MessageId, Reserved, WireError,
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
            m.encode(&mut wire);
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
                    m.encode(&mut re);
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
