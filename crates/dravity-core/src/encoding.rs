//! Small, dependency-free hex and base32 helpers.

/// Lower-case hex encoding.
pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        for nibble in [b >> 4, b & 0x0f] {
            s.push(char::from_digit(u32::from(nibble), 16).unwrap_or('0'));
        }
    }
    s
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Decodes case-insensitive hex into exactly `N` bytes.
pub(crate) fn hex_decode<const N: usize>(s: &str) -> Option<[u8; N]> {
    let s = s.as_bytes();
    if s.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (o, pair) in out.iter_mut().zip(s.chunks_exact(2)) {
        let [hi, lo] = pair else { return None };
        *o = (hex_val(*hi)? << 4) | hex_val(*lo)?;
    }
    Some(out)
}

/// Decodes RFC 4648 base32 (case-insensitive, no padding) of exactly 32
/// characters into 20 bytes, as used by legacy magnet links.
pub(crate) fn base32_decode_20(s: &str) -> Option<[u8; 20]> {
    let s = s.as_bytes();
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 20];
    let mut acc: u64 = 0;
    let mut bits = 0u32;
    let mut i = 0usize;
    for &c in s {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a',
            b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        acc = (acc << 5) | u64::from(v);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            *out.get_mut(i)? = u8::try_from((acc >> bits) & 0xff).ok()?;
            i += 1;
        }
    }
    (i == 20).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let b = [0x00, 0x7f, 0xff, 0xa5];
        assert_eq!(hex_encode(&b), "007fffa5");
        assert_eq!(hex_decode::<4>("007FfFa5"), Some(b));
        assert_eq!(hex_decode::<4>("007fffa"), None);
        assert_eq!(hex_decode::<4>("007fffag"), None);
    }

    #[test]
    fn base32_known_vector() {
        // 20 bytes of 0x00..0x13 in base32.
        let bytes: [u8; 20] = core::array::from_fn(|i| u8::try_from(i).unwrap());
        assert_eq!(
            base32_decode_20("AAAQEAYEAUDAOCAJBIFQYDIOB4IBCEQT"),
            Some(bytes)
        );
        assert_eq!(
            base32_decode_20("aaaqeayeaudaocajbifqydiob4ibceqt"),
            Some(bytes)
        );
        assert_eq!(base32_decode_20("AAAQEAYEAUDAOCAJBIFQYDIOB4IBCEQ1"), None);
        assert_eq!(base32_decode_20("AAAQ"), None);
    }
}
