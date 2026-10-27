//! Low-level, allocation-free bencode tokenizer shared by [`Value`](crate::Value)
//! decoding and the serde [`Deserializer`](crate::Deserializer).

use std::cmp::Ordering;

use crate::error::{Error, Result};

/// Default maximum nesting depth of lists and dictionaries.
pub const DEFAULT_MAX_DEPTH: usize = 256;

/// Default maximum number of entries in one list or dictionary.
pub const DEFAULT_MAX_ITEMS: usize = 1_048_576;

/// Options controlling how strictly input is decoded.
///
/// The default is *lenient about key order* (many real-world `.torrent` files
/// have unsorted dictionaries) but always rejects non-canonical integers,
/// duplicate adjacent keys, trailing data, excessive nesting, and containers
/// with more than [`DEFAULT_MAX_ITEMS`] entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeOptions {
    max_depth: usize,
    max_items: usize,
    require_sorted_keys: bool,
}

impl Default for DecodeOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl DecodeOptions {
    /// Lenient defaults (see type-level docs).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_depth: DEFAULT_MAX_DEPTH,
            max_items: DEFAULT_MAX_ITEMS,
            require_sorted_keys: false,
        }
    }

    /// Strict, canonical-only decoding: dictionary keys must be strictly sorted.
    #[must_use]
    pub const fn strict() -> Self {
        Self {
            max_depth: DEFAULT_MAX_DEPTH,
            max_items: DEFAULT_MAX_ITEMS,
            require_sorted_keys: true,
        }
    }

    /// Sets the maximum nesting depth of lists and dictionaries.
    #[must_use]
    pub const fn max_depth(mut self, depth: usize) -> Self {
        self.max_depth = depth;
        self
    }

    /// Sets the maximum number of entries in a single list or dictionary.
    #[must_use]
    pub const fn max_items(mut self, items: usize) -> Self {
        self.max_items = items;
        self
    }

    /// Requires dictionary keys to be in strictly ascending byte order.
    #[must_use]
    pub const fn require_sorted_keys(mut self, yes: bool) -> Self {
        self.require_sorted_keys = yes;
        self
    }

    /// Returns the configured maximum nesting depth.
    #[must_use]
    pub const fn depth_limit(&self) -> usize {
        self.max_depth
    }

    /// Returns the configured maximum number of entries per container.
    #[must_use]
    pub const fn item_limit(&self) -> usize {
        self.max_items
    }

    /// Returns whether sorted dictionary keys are required.
    #[must_use]
    pub const fn requires_sorted_keys(&self) -> bool {
        self.require_sorted_keys
    }
}

/// A cursor over borrowed bencode input.
#[derive(Debug)]
pub(crate) struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
    depth: usize,
    opts: DecodeOptions,
}

impl<'a> Parser<'a> {
    pub(crate) fn new(input: &'a [u8], opts: DecodeOptions) -> Self {
        Self {
            input,
            pos: 0,
            depth: 0,
            opts,
        }
    }

    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    pub(crate) fn peek(&self) -> Result<u8> {
        self.input
            .get(self.pos)
            .copied()
            .ok_or(Error::UnexpectedEof { offset: self.pos })
    }

    /// Consumes `byte` or fails.
    pub(crate) fn expect(&mut self, byte: u8) -> Result<()> {
        let b = self.peek()?;
        if b == byte {
            self.pos += 1;
            Ok(())
        } else {
            Err(Error::UnexpectedByte {
                byte: b,
                offset: self.pos,
            })
        }
    }

    /// Consumes the end marker `e` if it is next. Returns whether it did.
    pub(crate) fn eat_end(&mut self) -> Result<bool> {
        if self.peek()? == b'e' {
            self.pos += 1;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub(crate) fn finish(&self) -> Result<()> {
        if self.pos == self.input.len() {
            Ok(())
        } else {
            Err(Error::TrailingData { offset: self.pos })
        }
    }

    /// Consumes a container start byte (`l` or `d`) and enters one nesting level.
    pub(crate) fn enter(&mut self, byte: u8) -> Result<()> {
        let offset = self.pos;
        self.expect(byte)?;
        if self.depth >= self.opts.max_depth {
            return Err(Error::DepthLimitExceeded {
                max: self.opts.max_depth,
                offset,
            });
        }
        self.depth += 1;
        Ok(())
    }

    pub(crate) fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// Rejects the next entry when `seen` has already reached the item cap.
    pub(crate) fn check_items(&self, seen: usize) -> Result<()> {
        if seen >= self.opts.max_items {
            Err(Error::TooManyItems {
                max: self.opts.max_items,
                offset: self.pos,
            })
        } else {
            Ok(())
        }
    }

    /// Parses `i<digits>e`, validating canonical form. Returns the digit slice
    /// (with optional leading `-`) and the offset of the `i`.
    pub(crate) fn int_digits(&mut self) -> Result<(&'a [u8], usize)> {
        let start = self.pos;
        self.expect(b'i')?;
        let rest = self.input.get(self.pos..).unwrap_or_default();
        let len = rest
            .iter()
            .position(|&b| b == b'e')
            .ok_or(Error::UnexpectedEof {
                offset: self.input.len(),
            })?;
        let digits = rest.get(..len).unwrap_or_default();
        if !is_canonical_int(digits) {
            return Err(Error::InvalidInteger { offset: start });
        }
        self.pos += len + 1;
        Ok((digits, start))
    }

    /// Parses an integer that must fit in `i64`.
    pub(crate) fn int_i64(&mut self) -> Result<i64> {
        let (digits, offset) = self.int_digits()?;
        parse_i64(digits).ok_or(Error::IntegerOverflow { offset })
    }

    /// Parses `<len>:<bytes>` and returns the borrowed bytes and the offset of the prefix.
    pub(crate) fn byte_string(&mut self) -> Result<(&'a [u8], usize)> {
        let start = self.pos;
        let first = self.peek()?;
        if !first.is_ascii_digit() {
            return Err(Error::UnexpectedByte {
                byte: first,
                offset: start,
            });
        }
        let mut len: usize = 0;
        let mut ndigits = 0usize;
        loop {
            let b = self.peek()?;
            if b == b':' {
                break;
            }
            if !b.is_ascii_digit() {
                return Err(Error::UnexpectedByte {
                    byte: b,
                    offset: self.pos,
                });
            }
            if ndigits == 1 && first == b'0' {
                // Leading zeros are not canonical.
                return Err(Error::InvalidLength { offset: start });
            }
            len = len
                .checked_mul(10)
                .and_then(|l| l.checked_add(usize::from(b - b'0')))
                .ok_or(Error::InvalidLength { offset: start })?;
            ndigits += 1;
            self.pos += 1;
        }
        self.pos += 1; // ':'
        let end = self
            .pos
            .checked_add(len)
            .filter(|&end| end <= self.input.len())
            .ok_or(Error::UnexpectedEof {
                offset: self.input.len(),
            })?;
        let bytes = self.input.get(self.pos..end).unwrap_or_default();
        self.pos = end;
        Ok((bytes, start))
    }

    /// Validates key ordering inside a dictionary.
    pub(crate) fn check_key(&self, prev: Option<&[u8]>, key: &[u8], offset: usize) -> Result<()> {
        if let Some(prev) = prev {
            match prev.cmp(key) {
                Ordering::Less => {}
                Ordering::Equal => return Err(Error::DuplicateKey { offset }),
                Ordering::Greater if self.opts.require_sorted_keys => {
                    return Err(Error::UnsortedKeys { offset });
                }
                Ordering::Greater => {}
            }
        }
        Ok(())
    }

    /// Skips one complete value, validating it, and returns its raw bytes.
    pub(crate) fn skip_value(&mut self) -> Result<&'a [u8]> {
        let start = self.pos;
        match self.peek()? {
            b'i' => {
                self.int_digits()?;
            }
            b'0'..=b'9' => {
                self.byte_string()?;
            }
            b'l' => {
                self.enter(b'l')?;
                let mut seen = 0usize;
                while !self.eat_end()? {
                    self.check_items(seen)?;
                    seen += 1;
                    self.skip_value()?;
                }
                self.leave();
            }
            b'd' => {
                self.enter(b'd')?;
                let mut prev: Option<&[u8]> = None;
                let mut seen = 0usize;
                while !self.eat_end()? {
                    self.check_items(seen)?;
                    seen += 1;
                    let key = self.dict_key()?;
                    self.check_key(prev, key.0, key.1)?;
                    prev = Some(key.0);
                    self.skip_value()?;
                }
                self.leave();
            }
            byte => {
                return Err(Error::UnexpectedByte {
                    byte,
                    offset: start,
                });
            }
        }
        Ok(self.input.get(start..self.pos).unwrap_or_default())
    }

    /// Reads a dictionary key (which must be a byte string).
    pub(crate) fn dict_key(&mut self) -> Result<(&'a [u8], usize)> {
        let offset = self.pos;
        if !self.peek()?.is_ascii_digit() {
            return Err(Error::KeyNotBytes { offset });
        }
        self.byte_string()
    }
}

/// `-?(0|[1-9][0-9]*)` excluding `-0`.
fn is_canonical_int(digits: &[u8]) -> bool {
    let unsigned = match digits {
        [b'-', rest @ ..] => {
            if rest.first() == Some(&b'0') {
                return false;
            }
            rest
        }
        _ => digits,
    };
    match unsigned {
        [] => false,
        [b'0'] => true,
        [b'0', ..] => false,
        _ => unsigned.iter().all(u8::is_ascii_digit),
    }
}

/// Parses validated canonical digits into an `i64`.
pub(crate) fn parse_i64(digits: &[u8]) -> Option<i64> {
    let (neg, body) = match digits {
        [b'-', rest @ ..] => (true, rest),
        _ => (false, digits),
    };
    let mut acc: i64 = 0;
    for &d in body {
        let d = i64::from(d.wrapping_sub(b'0'));
        acc = acc.checked_mul(10)?;
        acc = if neg {
            acc.checked_sub(d)?
        } else {
            acc.checked_add(d)?
        };
    }
    Some(acc)
}

/// Parses validated canonical, non-negative digits into a `u64`.
pub(crate) fn parse_u64(digits: &[u8]) -> Option<u64> {
    if digits.first() == Some(&b'-') {
        return None;
    }
    digits.iter().try_fold(0u64, |acc, &d| {
        acc.checked_mul(10)?
            .checked_add(u64::from(d.wrapping_sub(b'0')))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_ints() {
        for ok in ["0", "1", "-1", "42", "-9223372036854775808"] {
            assert!(is_canonical_int(ok.as_bytes()), "{ok}");
        }
        for bad in ["", "-", "-0", "00", "01", "-01", "1a", "+1", " 1"] {
            assert!(!is_canonical_int(bad.as_bytes()), "{bad}");
        }
    }

    #[test]
    fn int_ranges() {
        assert_eq!(parse_i64(b"-9223372036854775808"), Some(i64::MIN));
        assert_eq!(parse_i64(b"9223372036854775808"), None);
        assert_eq!(parse_u64(b"18446744073709551615"), Some(u64::MAX));
        assert_eq!(parse_u64(b"18446744073709551616"), None);
        assert_eq!(parse_u64(b"-1"), None);
    }

    #[test]
    fn byte_string_lengths() {
        let mut p = Parser::new(b"0:", DecodeOptions::new());
        assert_eq!(p.byte_string().unwrap().0, b"");
        let mut p = Parser::new(b"01:a", DecodeOptions::new());
        assert!(matches!(
            p.byte_string(),
            Err(Error::InvalidLength { offset: 0 })
        ));
        let mut p = Parser::new(b"99999999999999999999999:a", DecodeOptions::new());
        assert!(matches!(p.byte_string(), Err(Error::InvalidLength { .. })));
        let mut p = Parser::new(b"5:abc", DecodeOptions::new());
        assert!(matches!(
            p.byte_string(),
            Err(Error::UnexpectedEof { offset: 5 })
        ));
    }

    #[test]
    fn depth_limit() {
        let deep = "l".repeat(10) + &"e".repeat(10);
        let mut p = Parser::new(deep.as_bytes(), DecodeOptions::new().max_depth(9));
        assert!(matches!(
            p.skip_value(),
            Err(Error::DepthLimitExceeded { max: 9, offset: 9 })
        ));
        let mut p = Parser::new(deep.as_bytes(), DecodeOptions::new().max_depth(10));
        assert_eq!(p.skip_value().unwrap().len(), 20);
    }
}
