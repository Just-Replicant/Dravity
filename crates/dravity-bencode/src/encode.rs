//! Primitive encoders.

use std::fmt::Display;
use std::io::Write as _;

/// Appends the decimal representation of `n`.
fn decimal(out: &mut Vec<u8>, n: impl Display) {
    // Writing into a `Vec<u8>` is infallible.
    let _ = write!(out, "{n}");
}

/// Appends `i<n>e`.
pub(crate) fn int(out: &mut Vec<u8>, n: impl Display) {
    out.push(b'i');
    decimal(out, n);
    out.push(b'e');
}

/// Appends `<len>:<bytes>`.
pub(crate) fn bytes(out: &mut Vec<u8>, b: &[u8]) {
    decimal(out, b.len());
    out.push(b':');
    out.extend_from_slice(b);
}
