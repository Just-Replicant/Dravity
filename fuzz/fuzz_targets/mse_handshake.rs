//! MSE handshakes fed arbitrary bytes: raw, and as tampered versions of a
//! genuine exchange, so the encrypted header parsers (crypto fields, padding
//! and IA lengths) see fuzzed values.
#![no_main]

use dravity_mse::{CryptoSet, Initiator, Responder, req2_hash};
use libfuzzer_sys::fuzz_target;

const SKEY: [u8; 20] = [0x5c; 20];

fn lookup(hash: &[u8; 20]) -> Option<[u8; 20]> {
    (*hash == req2_hash(&SKEY)).then_some(SKEY)
}

/// `genuine` with `noise` XORed over it from `from` on; extra noise is
/// appended.
fn tamper(genuine: &[u8], from: usize, noise: &[u8]) -> Vec<u8> {
    let mut out = genuine.to_vec();
    let mut extra = Vec::new();
    for (i, n) in noise.iter().enumerate() {
        match out.get_mut(from + i) {
            Some(b) => *b ^= n,
            None => extra.push(*n),
        }
    }
    out.extend_from_slice(&extra);
    out
}

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    let chunk = usize::from(mode >> 2).max(1);
    let (Ok(mut a), Ok(mut b)) = (
        Initiator::with_private_key(&[1; 20], &[7; 3], &SKEY, CryptoSet::ALL),
        Responder::with_private_key(&[2; 20], &[9; 5], CryptoSet::ALL),
    ) else {
        return;
    };
    match mode & 3 {
        0 => {
            for part in rest.chunks(chunk) {
                if !matches!(b.receive(part, lookup), Ok(None)) {
                    break;
                }
            }
        }
        1 => {
            for part in rest.chunks(chunk) {
                if !matches!(a.receive(part), Ok(None)) {
                    break;
                }
            }
        }
        2 => {
            // A genuine step 1 and 2, then a tampered step 3 (past the two
            // hashes) at the responder.
            let step1 = a.take_output();
            if !matches!(b.receive(&step1, lookup), Ok(None)) {
                return;
            }
            if !matches!(a.receive(&b.take_output()), Ok(None)) {
                return;
            }
            let step3 = tamper(&a.take_output(), 40, rest);
            for part in step3.chunks(chunk) {
                match b.receive(part, lookup) {
                    Ok(None) => {}
                    Ok(Some(incoming)) => {
                        assert_eq!(incoming.skey, SKEY);
                        break;
                    }
                    Err(_) => break,
                }
            }
        }
        _ => {
            // A genuine exchange up to step 4, tampered at the initiator.
            let step1 = a.take_output();
            if !matches!(b.receive(&step1, lookup), Ok(None)) {
                return;
            }
            let step2 = b.take_output();
            if !matches!(a.receive(&step2), Ok(None)) {
                return;
            }
            if !matches!(b.receive(&a.take_output(), lookup), Ok(Some(_))) {
                return;
            }
            let step4 = tamper(&b.take_output(), 0, rest);
            for part in step4.chunks(chunk) {
                if !matches!(a.receive(part), Ok(None)) {
                    break;
                }
            }
        }
    }
});
