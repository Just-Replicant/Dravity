//! The primitives: Diffie-Hellman over the MSE prime, SHA-1 key derivation
//! and RC4 with the first 1024 bytes of keystream dropped.

use std::fmt;
use std::sync::LazyLock;

use num_bigint::BigUint;
use rc4::{KeyInit, Rc4, StreamCipher};
use sha1::{Digest, Sha1};

use crate::MseError;

/// Length of a public key and of the shared secret (768 bits).
pub(crate) const KEY_LEN: usize = 96;

/// Length of a private key (160 bits, as the specification recommends).
pub(crate) const PRIVATE_LEN: usize = 20;

/// The 768-bit safe prime of the specification.
const PRIME_HEX: &str = "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74\
                         020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F1437\
                         4FE1356D6D51C245E485B576625E7EC6F44C42E9A63A36210000000000090563";

pub(crate) static PRIME: LazyLock<BigUint> =
    LazyLock::new(|| BigUint::parse_bytes(PRIME_HEX.as_bytes(), 16).unwrap_or_default());

static GENERATOR: LazyLock<BigUint> = LazyLock::new(|| BigUint::from(2u8));

/// Bytes of RC4 keystream discarded before use.
const DISCARD: usize = 1024;

/// One side's Diffie-Hellman key pair.
pub(crate) struct KeyPair {
    private: BigUint,
    public: [u8; KEY_LEN],
}

impl fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyPair").finish_non_exhaustive()
    }
}

impl KeyPair {
    /// A fresh key pair from the operating system's randomness.
    pub(crate) fn generate() -> Result<Self, MseError> {
        let mut private = [0u8; PRIVATE_LEN];
        getrandom::fill(&mut private)?;
        Ok(Self::from_private(&private))
    }

    /// The key pair for a given private key (big-endian).
    pub(crate) fn from_private(private: &[u8; PRIVATE_LEN]) -> Self {
        let private = BigUint::from_bytes_be(private);
        let public = to_key_bytes(&GENERATOR.modpow(&private, &PRIME));
        Self { private, public }
    }

    pub(crate) const fn public(&self) -> &[u8; KEY_LEN] {
        &self.public
    }

    /// The shared secret with the peer's public key `theirs`.
    ///
    /// # Errors
    /// [`MseError::InvalidPublicKey`] unless `1 < theirs < P - 1`: the other
    /// values give a secret an eavesdropper can guess.
    pub(crate) fn shared(&self, theirs: &[u8]) -> Result<[u8; KEY_LEN], MseError> {
        let y = BigUint::from_bytes_be(theirs);
        let one = BigUint::from(1u8);
        if y <= one || y >= &*PRIME - &one {
            return Err(MseError::InvalidPublicKey);
        }
        Ok(to_key_bytes(&y.modpow(&self.private, &PRIME)))
    }
}

/// `n` as 96 big-endian bytes, left-padded with zeros.
fn to_key_bytes(n: &BigUint) -> [u8; KEY_LEN] {
    let bytes = n.to_bytes_be();
    let mut out = [0u8; KEY_LEN];
    let start = KEY_LEN.saturating_sub(bytes.len());
    if let (Some(dst), Some(src)) = (
        out.get_mut(start..),
        bytes.get(bytes.len().saturating_sub(KEY_LEN)..),
    ) {
        dst.copy_from_slice(src);
    }
    out
}

/// SHA-1 over the concatenation of `parts`.
pub(crate) fn hash(parts: &[&[u8]]) -> [u8; 20] {
    let mut h = Sha1::new();
    for part in parts {
        h.update(part);
    }
    h.finalize().into()
}

/// `HASH('req2', SKEY)`: what an incoming handshake carries (obfuscated) to
/// name its torrent. A responder's lookup compares it with this value for
/// each info-hash it serves.
#[must_use]
pub fn req2_hash(skey: &[u8; 20]) -> [u8; 20] {
    hash(&[b"req2", skey])
}

/// An RC4 stream for one direction, keyed `HASH(key_name, S, SKEY)`, with
/// the first 1024 bytes of keystream dropped.
pub(crate) struct Cipher(Rc4);

impl fmt::Debug for Cipher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Cipher(RC4)")
    }
}

impl Cipher {
    pub(crate) fn new(key_name: &[u8], secret: &[u8; KEY_LEN], skey: &[u8; 20]) -> Self {
        let key = hash(&[key_name, secret, skey]);
        Self::from_key(&key)
    }

    pub(crate) fn from_key(key: &[u8; 20]) -> Self {
        // A 20-byte key is always within RC4's 1..=256 byte range.
        let mut rc4 = match Rc4::new_from_slice(key) {
            Ok(rc4) => rc4,
            Err(_) => unreachable_key(),
        };
        let mut discard = [0u8; DISCARD];
        rc4.apply_keystream(&mut discard);
        Self(rc4)
    }

    /// Encrypts or decrypts `buf` in place, continuing the stream.
    pub(crate) fn apply(&mut self, buf: &mut [u8]) {
        self.0.apply_keystream(buf);
    }
}

#[cold]
#[expect(clippy::panic, reason = "a 20-byte RC4 key is always valid")]
fn unreachable_key() -> ! {
    panic!("RC4 rejected a 20-byte key")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    /// RFC 6229 test vector: key 0x0102030405, keystream at offset 0.
    #[test]
    fn rc4_matches_rfc_6229() {
        let mut rc4 = Rc4::new_from_slice(&[1, 2, 3, 4, 5]).unwrap();
        let mut out = [0u8; 16];
        rc4.apply_keystream(&mut out);
        assert_eq!(
            out,
            [
                0xb2, 0x39, 0x63, 0x05, 0xf0, 0x3d, 0xc0, 0x27, 0xcc, 0xc3, 0x52, 0x4a, 0x0a, 0x11,
                0x18, 0xa8
            ]
        );
    }

    #[test]
    fn cipher_drops_the_first_1024_bytes() {
        let key = [9u8; 20];
        let mut raw = Rc4::new_from_slice(&key).unwrap();
        let mut stream = vec![0u8; DISCARD + 64];
        raw.apply_keystream(&mut stream);
        let mut ours = [0u8; 64];
        Cipher::from_key(&key).apply(&mut ours);
        assert_eq!(&ours[..], &stream[DISCARD..]);
    }

    /// The constant is the prime the specification prints: 768 bits, and
    /// Fermat-probable prime for several bases.
    #[test]
    fn prime_is_the_specified_768_bit_prime() {
        assert_eq!(PRIME.bits(), 768);
        let p_minus_one = &*PRIME - 1u8;
        for base in [2u8, 3, 5, 7, 11] {
            assert_eq!(
                BigUint::from(base).modpow(&p_minus_one, &PRIME),
                BigUint::from(1u8)
            );
        }
        // Safe prime: (P - 1) / 2 is prime too.
        let q: BigUint = &p_minus_one >> 1;
        let q_minus_one = &q - 1u8;
        assert_eq!(
            BigUint::from(2u8).modpow(&q_minus_one, &q),
            BigUint::from(1u8)
        );
    }

    #[test]
    fn diffie_hellman_agrees() {
        let a = KeyPair::from_private(&[0x11; 20]);
        let b = KeyPair::from_private(&[0x22; 20]);
        let s1 = a.shared(b.public()).unwrap();
        let s2 = b.shared(a.public()).unwrap();
        assert_eq!(s1, s2);
        assert_ne!(s1, [0; KEY_LEN]);
        // Private key 1 gives the generator itself.
        let mut one = [0u8; 20];
        one[19] = 1;
        let g = KeyPair::from_private(&one);
        assert_eq!(g.public()[KEY_LEN - 1], 2);
        assert!(g.public()[..KEY_LEN - 1].iter().all(|&b| b == 0));
    }

    #[test]
    fn weak_public_keys_are_rejected() {
        let a = KeyPair::from_private(&[0x11; 20]);
        let mut p_minus_one = to_key_bytes(&(&*PRIME - 1u8));
        assert!(matches!(
            a.shared(&p_minus_one),
            Err(MseError::InvalidPublicKey)
        ));
        p_minus_one = [0; KEY_LEN];
        assert!(matches!(
            a.shared(&p_minus_one),
            Err(MseError::InvalidPublicKey)
        ));
        let mut one = [0u8; KEY_LEN];
        one[KEY_LEN - 1] = 1;
        assert!(matches!(a.shared(&one), Err(MseError::InvalidPublicKey)));
        assert!(matches!(
            a.shared(&[0xff; KEY_LEN]),
            Err(MseError::InvalidPublicKey)
        ));
    }
}
