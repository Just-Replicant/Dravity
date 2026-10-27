//! A fixed-length piece bitmap in wire order (bit 0 is the high bit of byte 0).

use bytes::Bytes;
use dravity_wire::{WireError, check_bitfield};

/// A set of piece indexes with a fixed length, stored in BEP 3 wire order.
///
/// ```
/// use dravity_engine::Bitfield;
///
/// let mut b = Bitfield::new(10);
/// assert!(b.set(3));
/// assert!(!b.set(3));
/// assert!(b.get(3));
/// assert_eq!(b.count(), 1);
/// assert_eq!(b.as_bytes(), &[0b0001_0000, 0]);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitfield {
    bytes: Vec<u8>,
    len: u32,
    count: u32,
}

impl Bitfield {
    /// An empty bitfield for `len` pieces.
    #[must_use]
    pub fn new(len: u32) -> Self {
        Self {
            bytes: vec![0; byte_len(len)],
            len,
            count: 0,
        }
    }

    /// A bitfield with every piece set.
    #[must_use]
    pub fn full(len: u32) -> Self {
        let mut b = Self::new(len);
        for i in 0..len {
            b.set(i);
        }
        b
    }

    /// Parses a peer's `bitfield` payload for a torrent with `len` pieces.
    ///
    /// # Errors
    /// [`WireError::BadBitfield`] when the length or spare bits are wrong.
    pub fn from_wire(bytes: &[u8], len: u32) -> Result<Self, WireError> {
        check_bitfield(bytes, len)?;
        let count = bytes.iter().map(|b| b.count_ones()).sum();
        Ok(Self {
            bytes: bytes.to_vec(),
            len,
            count,
        })
    }

    /// Number of pieces this bitfield covers.
    #[must_use]
    pub const fn len(&self) -> u32 {
        self.len
    }

    /// Whether the torrent has no pieces.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Number of set bits.
    #[must_use]
    pub const fn count(&self) -> u32 {
        self.count
    }

    /// Whether every piece is set (true for a zero-piece torrent).
    #[must_use]
    pub const fn is_full(&self) -> bool {
        self.count == self.len
    }

    /// Whether piece `index` is set. Out-of-range indexes are never set.
    #[must_use]
    pub fn get(&self, index: u32) -> bool {
        index < self.len && dravity_wire::bitfield_bit(&self.bytes, index).unwrap_or(false)
    }

    /// Sets piece `index`. Returns `true` if it was newly set, `false` if it
    /// was already set or out of range.
    pub fn set(&mut self, index: u32) -> bool {
        self.write(index, true)
    }

    /// Clears piece `index`. Returns `true` if it was previously set.
    pub fn clear(&mut self, index: u32) -> bool {
        self.write(index, false)
    }

    fn write(&mut self, index: u32, on: bool) -> bool {
        if index >= self.len || self.get(index) == on {
            return false;
        }
        let Some(byte) = usize::try_from(index / 8)
            .ok()
            .and_then(|i| self.bytes.get_mut(i))
        else {
            return false;
        };
        let mask = 0x80_u8 >> (index % 8);
        if on {
            *byte |= mask;
            self.count += 1;
        } else {
            *byte &= !mask;
            self.count -= 1;
        }
        true
    }

    /// Whether `self` has any piece that `other` lacks. Both must cover the
    /// same number of pieces; otherwise this returns `false`.
    #[must_use]
    pub fn has_any_missing_from(&self, other: &Self) -> bool {
        self.len == other.len
            && self
                .bytes
                .iter()
                .zip(&other.bytes)
                .any(|(mine, theirs)| mine & !theirs != 0)
    }

    /// Iterates over the set piece indexes in ascending order.
    pub fn ones(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.len).filter(|&i| self.get(i))
    }

    /// The wire representation.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The wire representation as [`Bytes`], for a `bitfield` message.
    #[must_use]
    pub fn to_bytes(&self) -> Bytes {
        Bytes::copy_from_slice(&self.bytes)
    }
}

fn byte_len(len: u32) -> usize {
    usize::try_from(len.div_ceil(8)).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn full_and_empty() {
        let b = Bitfield::full(9);
        assert!(b.is_full());
        assert_eq!(b.as_bytes(), &[0xff, 0x80]);
        let z = Bitfield::new(0);
        assert!(z.is_full() && z.is_empty());
        assert!(z.as_bytes().is_empty());
    }

    #[test]
    fn out_of_range_is_ignored() {
        let mut b = Bitfield::new(3);
        assert!(!b.set(3));
        assert!(!b.get(100));
        assert!(!b.clear(7));
        assert_eq!(b.count(), 0);
    }

    #[test]
    fn from_wire_validates() {
        assert!(Bitfield::from_wire(&[0xff], 7).is_err());
        assert!(Bitfield::from_wire(&[0xfe], 7).is_ok());
        assert!(Bitfield::from_wire(&[0xfe, 0], 7).is_err());
        assert_eq!(Bitfield::from_wire(&[0xfe], 7).map(|b| b.count()), Ok(7));
    }

    #[test]
    fn missing_from() {
        let mut a = Bitfield::new(12);
        let mut b = Bitfield::new(12);
        a.set(11);
        assert!(a.has_any_missing_from(&b));
        b.set(11);
        assert!(!a.has_any_missing_from(&b));
        assert!(!a.has_any_missing_from(&Bitfield::new(3)));
    }

    proptest! {
        #[test]
        fn set_clear_roundtrip(len in 0u32..200, ops in proptest::collection::vec((any::<bool>(), 0u32..220), 0..300)) {
            let mut b = Bitfield::new(len);
            let mut model = std::collections::BTreeSet::new();
            for (on, i) in ops {
                let changed = if on { b.set(i) } else { b.clear(i) };
                let model_changed = i < len && if on { model.insert(i) } else { model.remove(&i) };
                prop_assert_eq!(changed, model_changed);
            }
            prop_assert_eq!(b.count() as usize, model.len());
            prop_assert_eq!(b.ones().collect::<Vec<_>>(), model.into_iter().collect::<Vec<_>>());
            prop_assert!(Bitfield::from_wire(b.as_bytes(), len).is_ok());
        }
    }
}
