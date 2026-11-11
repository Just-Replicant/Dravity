//! Interop-style tests: an independent, literal implementation of the MSE
//! specification's steps (written straight from the text, with nonzero
//! PadC/PadD and an initial payload, which Dravity itself never sends)
//! talks to Dravity's initiator and responder.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::cast_possible_truncation
)]

use dravity_mse::{CryptoSet, Initiator, MAX_PAD, Method, Responder, req2_hash};
use num_bigint::BigUint;
use rc4::{KeyInit, Rc4, StreamCipher};
use sha1::{Digest, Sha1};

const P: &str = "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9A63A36210000000000090563";
const SKEY: [u8; 20] = *b"0123456789abcdefghij";

fn p() -> BigUint {
    BigUint::parse_bytes(P.as_bytes(), 16).unwrap()
}

fn h(parts: &[&[u8]]) -> [u8; 20] {
    let mut s = Sha1::new();
    for part in parts {
        s.update(part);
    }
    s.finalize().into()
}

fn bytes96(n: &BigUint) -> Vec<u8> {
    let b = n.to_bytes_be();
    let mut out = vec![0u8; 96 - b.len()];
    out.extend_from_slice(&b);
    out
}

/// RC4 keyed HASH(name, S, SKEY), 1024 bytes discarded.
fn rc4(name: &[u8], s: &[u8], skey: &[u8]) -> Rc4 {
    let mut c = Rc4::new_from_slice(&h(&[name, s, skey])).unwrap();
    let mut discard = [0u8; 1024];
    c.apply_keystream(&mut discard);
    c
}

/// The reference peer's key pair: (private, Y).
fn keys(private: u8) -> (BigUint, Vec<u8>) {
    let x = BigUint::from_bytes_be(&[private; 20]);
    let y = BigUint::from(2u8).modpow(&x, &p());
    (x, bytes96(&y))
}

fn lookup(hash: &[u8; 20]) -> Option<[u8; 20]> {
    (*hash == req2_hash(&SKEY)).then_some(SKEY)
}

fn bt_handshake() -> Vec<u8> {
    let mut hs = b"\x13BitTorrent protocol".to_vec();
    hs.extend_from_slice(&[0; 8]);
    hs.extend_from_slice(&SKEY);
    hs.extend_from_slice(b"-XX0000-abcdefghijkl");
    hs
}

/// A reference initiator (padding, PadC and IA) against Dravity's responder.
#[test]
fn reference_initiator_to_dravity_responder() {
    for (provide, expected) in [(0x03u32, Method::Rc4), (0x01, Method::Plaintext)] {
        let (xa, ya) = keys(0x5a);
        let mut b = Responder::with_private_key(&[0x6b; 20], &[0xee; 33], CryptoSet::ALL).unwrap();

        // 1 A->B: Ya, PadA.
        let mut step1 = ya.clone();
        step1.extend_from_slice(&[0x11; 200]);
        assert!(b.receive(&step1, lookup).unwrap().is_none());

        // 2 B->A: Yb, PadB.
        let step2 = b.take_output();
        assert_eq!(step2.len(), 96 + 33);
        let yb = BigUint::from_bytes_be(&step2[..96]);
        let s = bytes96(&yb.modpow(&xa, &p()));

        // 3 A->B.
        let mut enc = rc4(b"keyA", &s, &SKEY);
        let mut dec = rc4(b"keyB", &s, &SKEY);
        let ia = bt_handshake();
        let mut step3 = h(&[b"req1", &s]).to_vec();
        let req2 = h(&[b"req2", &SKEY]);
        let req3 = h(&[b"req3", &s]);
        step3.extend(req2.iter().zip(req3).map(|(a, b)| a ^ b));
        let mut secret = vec![0u8; 8];
        secret.extend_from_slice(&provide.to_be_bytes());
        secret.extend_from_slice(&10u16.to_be_bytes());
        secret.extend_from_slice(&[0; 10]);
        secret.extend_from_slice(&(ia.len() as u16).to_be_bytes());
        secret.extend_from_slice(&ia);
        enc.apply_keystream(&mut secret);
        step3.extend_from_slice(&secret);
        let incoming = b.receive(&step3, lookup).unwrap().expect("established");
        assert_eq!(incoming.skey, SKEY);
        assert_eq!(incoming.established.method(), expected);
        assert_eq!(incoming.established.leftover(), &ia[..]);

        // 4 B->A: ENCRYPT(VC, crypto_select, len(PadD), PadD), payload.
        let mut step4 = b.take_output();
        let mut est = incoming.established;
        let mut reply = b"reply".to_vec();
        est.encrypt(&mut reply);
        step4.extend_from_slice(&reply);
        dec.apply_keystream(&mut step4[..14]);
        assert_eq!(&step4[..8], &[0; 8]);
        let selected = if expected == Method::Rc4 { 0x02 } else { 0x01 };
        assert_eq!(
            u32::from_be_bytes(step4[8..12].try_into().unwrap()),
            selected
        );
        let pad_d = usize::from(u16::from_be_bytes([step4[12], step4[13]]));
        assert!(pad_d <= MAX_PAD);
        let mut rest = step4[14..].to_vec();
        dec.apply_keystream(&mut rest[..pad_d]);
        let mut payload = rest[pad_d..].to_vec();
        if expected == Method::Rc4 {
            dec.apply_keystream(&mut payload);
        }
        assert_eq!(payload, b"reply");

        // 5 A->B: payload in the selected method.
        let mut more = b"more".to_vec();
        if expected == Method::Rc4 {
            enc.apply_keystream(&mut more);
        }
        est.decrypt(&mut more);
        assert_eq!(more, b"more");
    }
}

/// A reference responder (PadB, PadD) against Dravity's initiator.
#[test]
fn dravity_initiator_to_reference_responder() {
    for (select, expected) in [(0x02u32, Method::Rc4), (0x01, Method::Plaintext)] {
        let mut a =
            Initiator::with_private_key(&[0x7c; 20], &[0x22; 17], &SKEY, CryptoSet::ALL).unwrap();
        let (xb, yb) = keys(0x3d);

        // 1 A->B.
        let step1 = a.take_output();
        assert_eq!(step1.len(), 96 + 17);
        let ya = BigUint::from_bytes_be(&step1[..96]);
        let s = bytes96(&ya.modpow(&xb, &p()));

        // 2 B->A: Yb and 300 bytes of PadB, delivered in two pieces.
        let mut step2 = yb.clone();
        step2.extend_from_slice(&[0x33; 300]);
        assert!(a.receive(&step2[..50]).unwrap().is_none());
        assert!(a.take_output().is_empty(), "step 3 waits for all of Yb");
        assert!(a.receive(&step2[50..150]).unwrap().is_none());
        // 3 A->B, checked field by field.
        let step3 = a.take_output();
        assert!(a.receive(&step2[150..]).unwrap().is_none());
        assert!(a.take_output().is_empty());
        assert_eq!(&step3[..20], &h(&[b"req1", &s]));
        let req3 = h(&[b"req3", &s]);
        let req2: Vec<u8> = step3[20..40].iter().zip(req3).map(|(a, b)| a ^ b).collect();
        assert_eq!(req2, h(&[b"req2", &SKEY]));
        let mut dec = rc4(b"keyA", &s, &SKEY);
        let mut enc = rc4(b"keyB", &s, &SKEY);
        let mut header = step3[40..].to_vec();
        dec.apply_keystream(&mut header);
        assert_eq!(&header[..8], &[0; 8]);
        assert_eq!(u32::from_be_bytes(header[8..12].try_into().unwrap()), 0x03);
        let pad_c = usize::from(u16::from_be_bytes([header[12], header[13]]));
        let ia_len = usize::from(u16::from_be_bytes([header[14 + pad_c], header[15 + pad_c]]));
        assert_eq!(header.len(), 16 + pad_c + ia_len);

        // 4 B->A: VC, select, 7 bytes of PadD, then payload.
        let mut step4 = vec![0u8; 8];
        step4.extend_from_slice(&select.to_be_bytes());
        step4.extend_from_slice(&7u16.to_be_bytes());
        step4.extend_from_slice(&[0; 7]);
        enc.apply_keystream(&mut step4);
        let mut payload = bt_handshake();
        if expected == Method::Rc4 {
            enc.apply_keystream(&mut payload);
        }
        step4.extend_from_slice(&payload);
        let mut est = a.receive(&step4).unwrap().expect("established");
        assert_eq!(est.method(), expected);
        assert_eq!(est.leftover(), &bt_handshake()[..]);

        // 5 A->B.
        let mut out = b"bitfield".to_vec();
        est.encrypt(&mut out);
        if expected == Method::Rc4 {
            dec.apply_keystream(&mut out);
        }
        assert_eq!(out, b"bitfield");
    }
}

/// A responder selecting a method the initiator did not offer is refused.
#[test]
fn unoffered_selection_is_refused() {
    let mut a = Initiator::with_private_key(&[1; 20], &[], &SKEY, CryptoSet::RC4).unwrap();
    let (xb, yb) = keys(0x44);
    let step1 = a.take_output();
    let s = bytes96(&BigUint::from_bytes_be(&step1[..96]).modpow(&xb, &p()));
    assert!(a.receive(&yb).unwrap().is_none());
    let _ = a.take_output();
    let mut enc = rc4(b"keyB", &s, &SKEY);
    let mut step4 = vec![0u8; 8];
    step4.extend_from_slice(&1u32.to_be_bytes());
    step4.extend_from_slice(&0u16.to_be_bytes());
    enc.apply_keystream(&mut step4);
    assert!(matches!(
        a.receive(&step4),
        Err(dravity_mse::MseError::NoCommonMethod)
    ));
}
