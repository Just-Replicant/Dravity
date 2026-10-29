//! `Range: bytes=…` request headers (RFC 9110, section 14) for file
//! streaming.
//!
//! Only a single range is honored. Anything this parser does not
//! understand (other units, several ranges, bad syntax) is ignored and the
//! whole file is served, which RFC 9110 allows. A well-formed range that
//! lies outside the file is [`Unsatisfiable`] (`416`).

/// An inclusive byte range within a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    /// First byte.
    pub start: u64,
    /// Last byte (inclusive).
    pub end: u64,
}

impl ByteRange {
    /// Number of bytes in the range.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.end - self.start + 1
    }

    /// Never true: a range holds at least one byte.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// The `Content-Range` header value for a file of `total` bytes.
    #[must_use]
    pub fn content_range(&self, total: u64) -> String {
        format!("bytes {}-{}/{total}", self.start, self.end)
    }
}

/// The requested range lies outside the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("range not satisfiable for {len} bytes")]
pub struct Unsatisfiable {
    /// File length.
    pub len: u64,
}

/// Parses a `Range` header for a file of `len` bytes.
///
/// `Ok(None)`: serve the whole file. `Ok(Some(range))`: serve `range`
/// (clamped to the file). `Err`: answer `416 Range Not Satisfiable`.
///
/// # Errors
/// [`Unsatisfiable`] when the range starts at or after the end of the file,
/// asks for a zero-length suffix, or the file is empty.
pub fn parse(header: &str, len: u64) -> Result<Option<ByteRange>, Unsatisfiable> {
    let Some(spec) = strip_unit(header.trim()) else {
        return Ok(None);
    };
    if spec.contains(',') {
        return Ok(None);
    }
    let Some((first, last)) = spec.trim().split_once('-') else {
        return Ok(None);
    };
    let (first, last) = (first.trim(), last.trim());
    let unsatisfiable = Err(Unsatisfiable { len });
    if first.is_empty() {
        // Suffix range: the last `n` bytes.
        let Some(n) = number(last) else {
            return Ok(None);
        };
        if n == 0 || len == 0 {
            return unsatisfiable;
        }
        return Ok(Some(ByteRange {
            start: len.saturating_sub(n),
            end: len - 1,
        }));
    }
    let Some(start) = number(first) else {
        return Ok(None);
    };
    let end = if last.is_empty() {
        None
    } else {
        match number(last) {
            Some(end) if end >= start => Some(end),
            // Last before first: invalid, so ignored.
            _ => return Ok(None),
        }
    };
    if start >= len {
        return unsatisfiable;
    }
    let end = end.map_or(len - 1, |end| end.min(len - 1));
    Ok(Some(ByteRange { start, end }))
}

/// The range set after a case-insensitive `bytes=`.
fn strip_unit(header: &str) -> Option<&str> {
    let (unit, spec) = header.split_once('=')?;
    unit.trim().eq_ignore_ascii_case("bytes").then_some(spec)
}

/// A decimal number of digits only (no sign, no spaces).
fn number(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{ByteRange, Unsatisfiable, parse};

    fn range(start: u64, end: u64) -> Result<Option<ByteRange>, Unsatisfiable> {
        Ok(Some(ByteRange { start, end }))
    }

    #[test]
    fn single_ranges_are_clamped_to_the_file() {
        assert_eq!(parse("bytes=0-99", 1000), range(0, 99));
        assert_eq!(parse("bytes=500-", 1000), range(500, 999));
        assert_eq!(parse("bytes=900-5000", 1000), range(900, 999));
        assert_eq!(parse("bytes=-100", 1000), range(900, 999));
        assert_eq!(parse("bytes=-5000", 1000), range(0, 999));
        assert_eq!(parse(" Bytes = 1 - 2 ", 1000), range(1, 2));
        assert_eq!(parse("bytes=0-0", 1), range(0, 0));
    }

    #[test]
    fn ranges_past_the_end_are_unsatisfiable() {
        let err = Err(Unsatisfiable { len: 1000 });
        assert_eq!(parse("bytes=1000-", 1000), err);
        assert_eq!(parse("bytes=1000-2000", 1000), err);
        assert_eq!(parse("bytes=-0", 1000), err);
        assert_eq!(parse("bytes=0-", 0), Err(Unsatisfiable { len: 0 }));
        assert_eq!(parse("bytes=-1", 0), Err(Unsatisfiable { len: 0 }));
    }

    #[test]
    fn what_is_not_understood_is_ignored() {
        for header in [
            "",
            "bytes",
            "items=0-1",
            "bytes=0-1,5-6",
            "bytes=5-1",
            "bytes=a-b",
            "bytes=+1-2",
            "bytes=-",
            "bytes=1",
            "bytes=99999999999999999999999-",
        ] {
            assert_eq!(parse(header, 1000), Ok(None), "{header:?}");
        }
    }

    #[test]
    fn the_content_range_names_the_bytes_and_the_total() {
        let range = ByteRange { start: 10, end: 19 };
        assert_eq!(range.len(), 10);
        assert_eq!(range.content_range(100), "bytes 10-19/100");
    }

    proptest! {
        #[test]
        fn a_served_range_is_inside_the_file(header in ".{0,40}", len in 0u64..10_000) {
            if let Ok(Some(range)) = parse(&header, len) {
                prop_assert!(range.start <= range.end);
                prop_assert!(range.end < len);
            }
        }

        #[test]
        fn explicit_ranges_parse(start in 0u64..1000, extra in 0u64..1000, len in 1u64..2000) {
            let header = format!("bytes={start}-{}", start + extra);
            let parsed = parse(&header, len);
            if start >= len {
                prop_assert_eq!(parsed, Err(Unsatisfiable { len }));
            } else {
                prop_assert_eq!(parsed, range(start, (start + extra).min(len - 1)));
            }
        }
    }
}
