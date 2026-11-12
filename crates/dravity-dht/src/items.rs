//! BEP 44 items, sans-IO: immutable values stored under their SHA-1 and
//! mutable values signed with ed25519, stored under SHA-1(key ‖ salt).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha1::{Digest, Sha1};

/// Largest bencoded value (BEP 44).
pub const MAX_VALUE: usize = 1000;
/// Longest salt (BEP 44).
pub const MAX_SALT: usize = 64;
/// How long an item is kept without a new `put` (BEP 44: 2 hours).
pub const ITEM_TTL: Duration = Duration::from_secs(2 * 3600);

/// A stored item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// The bencoded value.
    pub value: Vec<u8>,
    /// Mutable items: key, signature, sequence number and salt.
    pub mutable: Option<Mutable>,
}

/// The signed part of a mutable item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mutable {
    /// ed25519 public key.
    pub key: [u8; 32],
    /// Signature over salt, seq and value.
    pub sig: [u8; 64],
    /// Sequence number.
    pub seq: i64,
    /// Salt (may be empty).
    pub salt: Vec<u8>,
}

/// Why a `put` was refused, with the BEP 44 error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PutError {
    /// 205: the value is over 1000 bytes.
    #[error("value too big")]
    TooBig,
    /// 206: the signature does not verify.
    #[error("invalid signature")]
    BadSignature,
    /// 207: the salt is over 64 bytes.
    #[error("salt too big")]
    SaltTooBig,
    /// 301: `cas` did not match the stored sequence number.
    #[error("CAS mismatch")]
    CasMismatch,
    /// 302: the sequence number is lower than the stored one.
    #[error("sequence number less than current")]
    OldSeq,
}

impl PutError {
    /// The KRPC error code.
    #[must_use]
    pub const fn code(self) -> i64 {
        match self {
            Self::TooBig => 205,
            Self::BadSignature => 206,
            Self::SaltTooBig => 207,
            Self::CasMismatch => 301,
            Self::OldSeq => 302,
        }
    }
}

/// The target of an immutable item: SHA-1 of its bencoded value.
#[must_use]
pub fn immutable_target(value: &[u8]) -> [u8; 20] {
    Sha1::digest(value).into()
}

/// The target of a mutable item: SHA-1 of the key and salt.
#[must_use]
pub fn mutable_target(key: &[u8; 32], salt: &[u8]) -> [u8; 20] {
    let mut h = Sha1::new();
    h.update(key);
    h.update(salt);
    h.finalize().into()
}

/// The bytes a mutable item's signature covers (BEP 44).
#[must_use]
pub fn signed_bytes(salt: &[u8], seq: i64, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    if !salt.is_empty() {
        out.extend_from_slice(format!("4:salt{}:", salt.len()).as_bytes());
        out.extend_from_slice(salt);
    }
    out.extend_from_slice(format!("3:seqi{seq}e1:v").as_bytes());
    out.extend_from_slice(value);
    out
}

/// Signs a mutable item.
#[must_use]
pub fn sign(key: &SigningKey, salt: &[u8], seq: i64, value: &[u8]) -> Mutable {
    Mutable {
        key: key.verifying_key().to_bytes(),
        sig: key.sign(&signed_bytes(salt, seq, value)).to_bytes(),
        seq,
        salt: salt.to_vec(),
    }
}

/// Whether `m` is a valid signature of `value`.
#[must_use]
pub fn verify(m: &Mutable, value: &[u8]) -> bool {
    VerifyingKey::from_bytes(&m.key).is_ok_and(|key| {
        key.verify(
            &signed_bytes(&m.salt, m.seq, value),
            &Signature::from_bytes(&m.sig),
        )
        .is_ok()
    })
}

/// Items stored for others.
#[derive(Debug, Clone)]
pub struct ItemStore {
    items: HashMap<[u8; 20], (Item, Instant)>,
    max_items: usize,
}

impl ItemStore {
    /// A store of at most `max_items`.
    #[must_use]
    pub fn new(max_items: usize) -> Self {
        Self {
            items: HashMap::new(),
            max_items,
        }
    }

    /// Stores `item`; returns its target.
    ///
    /// # Errors
    /// The [`PutError`] BEP 44 asks for.
    pub fn put(
        &mut self,
        item: Item,
        cas: Option<i64>,
        now: Instant,
    ) -> Result<[u8; 20], PutError> {
        if item.value.len() > MAX_VALUE {
            return Err(PutError::TooBig);
        }
        let target = match &item.mutable {
            None => immutable_target(&item.value),
            Some(m) => {
                if m.salt.len() > MAX_SALT {
                    return Err(PutError::SaltTooBig);
                }
                if !verify(m, &item.value) {
                    return Err(PutError::BadSignature);
                }
                let target = mutable_target(&m.key, &m.salt);
                if let Some((
                    Item {
                        mutable: Some(old), ..
                    },
                    _,
                )) = self.items.get(&target)
                {
                    if cas.is_some_and(|cas| cas != old.seq) {
                        return Err(PutError::CasMismatch);
                    }
                    if m.seq < old.seq {
                        return Err(PutError::OldSeq);
                    }
                }
                target
            }
        };
        self.items
            .retain(|_, (_, at)| now.saturating_duration_since(*at) < ITEM_TTL);
        if !self.items.contains_key(&target) && self.items.len() >= self.max_items {
            return Ok(target);
        }
        self.items.insert(target, (item, now));
        Ok(target)
    }

    /// The live item stored under `target`.
    #[must_use]
    pub fn get(&self, target: &[u8; 20], now: Instant) -> Option<&Item> {
        self.items
            .get(target)
            .filter(|(_, at)| now.saturating_duration_since(*at) < ITEM_TTL)
            .map(|(item, _)| item)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn bep44_vectors() {
        // BEP 44 test vectors: the immutable "12:Hello World!" and the
        // signing input of the mutable example with and without salt.
        assert_eq!(
            immutable_target(b"12:Hello World!"),
            *b"\xe5\xf9\x6f\x6f\x38\x32\x0f\x0f\x33\x95\x9c\xb4\xd3\xd6\x56\x45\x21\x17\xaa\xdb"
        );
        assert_eq!(
            signed_bytes(b"", 1, b"12:Hello World!"),
            b"3:seqi1e1:v12:Hello World!"
        );
        assert_eq!(
            signed_bytes(b"foobar", 1, b"12:Hello World!"),
            b"4:salt6:foobar3:seqi1e1:v12:Hello World!"
        );
    }

    #[test]
    fn mutable_items_are_signed_ordered_and_cas_checked() {
        let now = Instant::now();
        let key = SigningKey::from_bytes(&[7; 32]);
        let mut store = ItemStore::new(10);
        let v1 = b"5:hello".to_vec();
        let m1 = sign(&key, b"s", 1, &v1);
        let target = store
            .put(
                Item {
                    value: v1.clone(),
                    mutable: Some(m1.clone()),
                },
                None,
                now,
            )
            .unwrap();
        assert_eq!(target, mutable_target(&m1.key, b"s"));
        let mut forged = m1.clone();
        forged.seq = 9;
        assert_eq!(
            store.put(
                Item {
                    value: v1.clone(),
                    mutable: Some(forged)
                },
                None,
                now
            ),
            Err(PutError::BadSignature)
        );
        let v2 = b"5:world".to_vec();
        let m2 = sign(&key, b"s", 2, &v2);
        assert_eq!(
            store.put(
                Item {
                    value: v2.clone(),
                    mutable: Some(m2.clone())
                },
                Some(5),
                now
            ),
            Err(PutError::CasMismatch)
        );
        store
            .put(
                Item {
                    value: v2.clone(),
                    mutable: Some(m2),
                },
                Some(1),
                now,
            )
            .unwrap();
        assert_eq!(
            store.put(
                Item {
                    value: v1,
                    mutable: Some(m1)
                },
                None,
                now
            ),
            Err(PutError::OldSeq)
        );
        assert_eq!(store.get(&target, now).unwrap().value, v2);
        assert!(store.get(&target, now + ITEM_TTL).is_none());
        let big = vec![b'x'; MAX_VALUE + 1];
        assert_eq!(
            store.put(
                Item {
                    value: big,
                    mutable: None
                },
                None,
                now
            ),
            Err(PutError::TooBig)
        );
    }
}
