//! The little HTTP/1.1 UPnP needs: one request per connection to a device
//! on the LAN, `Connection: close`, body read to the end (identity,
//! `Content-Length` or chunked).

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Most a response may be; device descriptions are a few KiB.
pub(crate) const MAX_RESPONSE: usize = 256 * 1024;

/// An `http://host:port/path` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    /// Host and port.
    pub addr: SocketAddr,
    /// Path with query, starting with `/`.
    pub path: String,
}

impl Url {
    /// Parses an `http://` URL whose host is an IP address (as UPnP
    /// devices advertise). Returns `None` otherwise.
    #[must_use]
    pub fn parse(url: &str) -> Option<Self> {
        let rest = url.trim().strip_prefix("http://").or_else(|| {
            url.trim()
                .get(..7)
                .filter(|p| p.eq_ignore_ascii_case("http://"))
                .and_then(|_| url.trim().get(7..))
        })?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (rest.get(..i)?, rest.get(i..)?),
            None => (rest, "/"),
        };
        let addr = authority
            .parse::<SocketAddr>()
            .ok()
            .or_else(|| format!("{authority}:80").parse().ok())?;
        Some(Self {
            addr,
            path: path.to_owned(),
        })
    }

    /// Resolves `reference` (absolute URL, absolute path or relative path)
    /// against this URL.
    #[must_use]
    pub fn join(&self, reference: &str) -> Option<Self> {
        let reference = reference.trim();
        if reference.to_ascii_lowercase().starts_with("http://") {
            return Self::parse(reference);
        }
        let path = if reference.starts_with('/') {
            reference.to_owned()
        } else {
            let dir = self
                .path
                .rfind('/')
                .map_or("/", |i| self.path.get(..=i).unwrap_or("/"));
            format!("{dir}{reference}")
        };
        Some(Self {
            addr: self.addr,
            path,
        })
    }
}

/// A complete response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
}

/// Why a response could not be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ParseError {
    #[error("malformed HTTP response")]
    Malformed,
    #[error("HTTP response too large")]
    TooLarge,
}

/// Parses a whole response (the connection was closed after it).
pub(crate) fn parse_response(data: &[u8]) -> Result<Response, ParseError> {
    let end = data
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or(ParseError::Malformed)?;
    let head = std::str::from_utf8(data.get(..end).ok_or(ParseError::Malformed)?)
        .map_err(|_| ParseError::Malformed)?;
    let body = data.get(end + 4..).unwrap_or_default();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().ok_or(ParseError::Malformed)?;
    let mut parts = status_line.split_whitespace();
    if !parts.next().is_some_and(|v| v.starts_with("HTTP/1.")) {
        return Err(ParseError::Malformed);
    }
    let status = parts
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or(ParseError::Malformed)?;
    let mut length = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            length = Some(value.parse::<usize>().map_err(|_| ParseError::Malformed)?);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.to_ascii_lowercase().contains("chunked");
        }
    }
    let body = if chunked {
        dechunk(body)?
    } else if let Some(len) = length {
        body.get(..len).ok_or(ParseError::Malformed)?.to_vec()
    } else {
        body.to_vec()
    };
    Ok(Response { status, body })
}

fn dechunk(mut data: &[u8]) -> Result<Vec<u8>, ParseError> {
    let mut out = Vec::new();
    loop {
        let line_end = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or(ParseError::Malformed)?;
        let line = std::str::from_utf8(data.get(..line_end).ok_or(ParseError::Malformed)?)
            .map_err(|_| ParseError::Malformed)?;
        let size_hex = line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_hex, 16).map_err(|_| ParseError::Malformed)?;
        data = data.get(line_end + 2..).ok_or(ParseError::Malformed)?;
        if size == 0 {
            return Ok(out);
        }
        if out.len() + size > MAX_RESPONSE {
            return Err(ParseError::TooLarge);
        }
        out.extend_from_slice(data.get(..size).ok_or(ParseError::Malformed)?);
        data = data.get(size..).ok_or(ParseError::Malformed)?;
        data = data.strip_prefix(b"\r\n").ok_or(ParseError::Malformed)?;
    }
}

/// Sends one request and reads the response, all within `timeout`.
pub(crate) async fn request(
    url: &Url,
    method: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout: Duration,
) -> io::Result<Response> {
    let exchange = async {
        let mut stream = TcpStream::connect(url.addr).await?;
        let mut head = format!(
            "{method} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n",
            url.path,
            url.addr,
            body.len()
        );
        for (name, value) in headers {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(body).await?;
        let mut data = Vec::new();
        let mut limited = (&mut stream).take(u64::try_from(MAX_RESPONSE).unwrap_or(u64::MAX));
        limited.read_to_end(&mut data).await?;
        parse_response(&data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    };
    tokio::time::timeout(timeout, exchange)
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_parse_and_join() {
        let base = Url::parse("http://192.168.1.1:5000/rootDesc.xml").unwrap();
        assert_eq!(base.addr, "192.168.1.1:5000".parse().unwrap());
        assert_eq!(base.path, "/rootDesc.xml");
        assert_eq!(base.join("/ctl/IPConn").unwrap().path, "/ctl/IPConn");
        assert_eq!(base.join("ctl").unwrap().path, "/ctl");
        let other = base.join("http://192.168.1.1:80/x").unwrap();
        assert_eq!(other.addr.port(), 80);
        assert_eq!(Url::parse("HTTP://10.0.0.1").unwrap().path, "/");
        assert_eq!(Url::parse("http://10.0.0.1").unwrap().addr.port(), 80);
        assert!(Url::parse("https://10.0.0.1/").is_none());
        assert!(Url::parse("http://router.local/").is_none());
    }

    #[test]
    fn responses_with_length_chunks_or_eof() {
        let r = parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabcdef").unwrap();
        assert_eq!((r.status, &r.body[..]), (200, &b"abc"[..]));
        let r = parse_response(
            b"HTTP/1.1 500 Internal\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!((r.status, &r.body[..]), (500, &b"abcde"[..]));
        let r = parse_response(b"HTTP/1.0 200 OK\r\n\r\nall of it").unwrap();
        assert_eq!(&r.body[..], b"all of it");
        assert_eq!(parse_response(b"garbage"), Err(ParseError::Malformed));
        assert_eq!(
            parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nabc"),
            Err(ParseError::Malformed)
        );
        assert_eq!(
            parse_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n"),
            Err(ParseError::Malformed)
        );
    }
}
