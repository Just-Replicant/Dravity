//! Sans-IO web seed planning: which URL holds each file, and which HTTP
//! ranges cover a range of the content stream.

use std::ops::Range;

use dravity_core::Info;

use crate::WebSeedError;

/// One part of a content-stream range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Download `length` bytes at `offset` of the file at `url`.
    Fetch {
        /// The file's URL.
        url: String,
        /// First byte within the file.
        offset: u64,
        /// Number of bytes.
        length: u64,
        /// The whole file's length, checked against the `Content-Range`
        /// total or the length of a `200` body.
        file_length: u64,
    },
    /// `length` bytes of a BEP 47 padding file: zeros, never fetched.
    Zeros {
        /// Number of bytes.
        length: u64,
    },
}

impl Segment {
    /// Bytes this segment contributes.
    #[must_use]
    pub const fn len(&self) -> u64 {
        match self {
            Self::Fetch { length, .. } | Self::Zeros { length } => *length,
        }
    }

    /// Whether the segment is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The URL of file `index` on the web seed `base` (BEP 19).
///
/// A multi-file torrent's files live under `base/<name>/<path…>` (a `/` is
/// added to `base` when missing). A single-file torrent's file is `base`
/// itself, or `base<name>` when `base` ends with `/`. Path components are
/// percent-encoded. `None` for padding files and out-of-range indices.
///
/// ```
/// use dravity_core::Metainfo;
/// use dravity_webseed::file_url;
///
/// let t = b"d4:infod6:lengthi3e4:name7:a b.txt12:piece lengthi16384e6:pieces20:aaaaaaaaaaaaaaaaaaaaee";
/// let m = Metainfo::from_bytes(t).unwrap();
/// assert_eq!(file_url("http://h/f.txt", m.info(), 0).unwrap(), "http://h/f.txt");
/// assert_eq!(file_url("http://h/d/", m.info(), 0).unwrap(), "http://h/d/a%20b.txt");
/// ```
#[must_use]
pub fn file_url(base: &str, info: &Info, index: usize) -> Option<String> {
    let file = info.files().get(index)?;
    if file.is_padding() {
        return None;
    }
    if !info.is_multi_file() {
        return Some(if base.ends_with('/') {
            format!("{base}{}", encode(info.name()))
        } else {
            base.to_owned()
        });
    }
    let mut url = base.to_owned();
    if !url.ends_with('/') {
        url.push('/');
    }
    url.push_str(&encode(info.name()));
    for c in file.path().components() {
        url.push('/');
        url.push_str(&encode(c));
    }
    Some(url)
}

/// The segments, in order, that make up `range` of the content stream on
/// the web seed `base`. Empty files are skipped; padding becomes
/// [`Segment::Zeros`].
///
/// # Errors
/// [`WebSeedError::OutOfRange`] when `range` is empty or reaches past the
/// content.
pub fn plan_range(
    base: &str,
    info: &Info,
    range: Range<u64>,
) -> Result<Vec<Segment>, WebSeedError> {
    if range.start >= range.end || range.end > info.total_length() {
        return Err(WebSeedError::OutOfRange {
            start: range.start,
            end: range.end,
        });
    }
    let mut out = Vec::new();
    for (index, file) in info.files().iter().enumerate() {
        let start = file.offset();
        let end = start.saturating_add(file.length());
        if end <= range.start || start >= range.end || file.length() == 0 {
            continue;
        }
        let from = range.start.max(start);
        let to = range.end.min(end);
        let length = to - from;
        match file_url(base, info, index) {
            Some(url) => out.push(Segment::Fetch {
                url,
                offset: from - start,
                length,
                file_length: file.length(),
            }),
            None => out.push(Segment::Zeros { length }),
        }
    }
    Ok(out)
}

/// The host and port a web seed URL connects to (`http` defaults to 80,
/// `https` to 443). `None` for other schemes or a URL without a host.
///
/// ```
/// use dravity_webseed::plan::host_port;
///
/// assert_eq!(host_port("https://user@Example.org/x"), Some(("Example.org".into(), 443)));
/// assert_eq!(host_port("http://[::1]:8080/a?b"), Some(("::1".into(), 8080)));
/// assert_eq!(host_port("ftp://h/"), None);
/// ```
#[must_use]
pub fn host_port(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let default = match scheme.to_ascii_lowercase().as_str() {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port) = if let Some(v6) = host_port.strip_prefix('[') {
        let (host, after) = v6.split_once(']')?;
        (host, after.strip_prefix(':'))
    } else {
        match host_port.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (host_port, None),
        }
    };
    if host.is_empty() {
        return None;
    }
    let port = match port {
        None | Some("") => default,
        Some(p) => p.parse().ok()?,
    };
    Some((host.to_owned(), port))
}

/// Percent-encodes everything but RFC 3986 unreserved characters.
fn encode(component: &str) -> String {
    let mut out = String::with_capacity(component.len());
    for b in component.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_and_port() {
        assert_eq!(host_port("http://h"), Some(("h".into(), 80)));
        assert_eq!(host_port("HTTP://h:81"), Some(("h".into(), 81)));
        assert_eq!(host_port("http://h:/x"), Some(("h".into(), 80)));
        assert_eq!(host_port("http://:80/"), None);
        assert_eq!(host_port("http://h:99999/"), None);
        assert_eq!(host_port("http://[::1/"), None);
        assert_eq!(host_port("no scheme"), None);
    }

    #[test]
    fn encodes_reserved_bytes() {
        assert_eq!(encode("a b/ü?#%"), "a%20b%2F%C3%BC%3F%23%25");
        assert_eq!(encode("Az09-._~"), "Az09-._~");
    }
}
