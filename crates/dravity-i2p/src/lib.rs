//! I2P SAM v3 client: session create, naming lookup, and stream
//! connect/accept, for talking to a SAM bridge (and, in tests, a fake
//! one).
//!
//! A [`Session`] holds the control connection the bridge requires to
//! stay open. [`Session::lookup`] uses that connection; [`Session::connect`]
//! and [`Session::accept`] each open a new one, then become a raw
//! [`Stream`]. Peers are not yet wired into the BitTorrent session.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

/// Default SAM bridge address (the Java I2P router's SAM v3 port).
pub const DEFAULT_BRIDGE: &str = "127.0.0.1:7656";

/// Largest reply or destination line we will read.
const MAX_LINE: usize = 16 * 1024;

/// How to reach a SAM v3 bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SamConfig {
    /// The bridge's `host:port`.
    pub bridge: String,
    /// Lowest SAM version we accept (`MIN=`).
    pub min_version: String,
    /// Highest SAM version we offer (`MAX=`).
    pub max_version: String,
}

impl Default for SamConfig {
    fn default() -> Self {
        Self {
            bridge: DEFAULT_BRIDGE.into(),
            min_version: "3.0".into(),
            max_version: "3.3".into(),
        }
    }
}

impl SamConfig {
    /// A configuration that talks to `bridge` (`host:port`).
    #[must_use]
    pub fn new(bridge: impl Into<String>) -> Self {
        Self {
            bridge: bridge.into(),
            ..Self::default()
        }
    }
}

/// The destination a session is created with.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DestinationSpec {
    /// The bridge generates a new destination (`DESTINATION=TRANSIENT`).
    Transient,
    /// A private key previously issued by a SAM bridge.
    PrivateKey(String),
}

/// An error talking to a SAM bridge.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SamError {
    /// The TCP connection failed.
    #[error("I2P SAM I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The bridge refused the command (`RESULT` is not `OK`).
    #[error("I2P SAM refused: {result}{}", message.as_deref().map(|m| format!(": {m}")).unwrap_or_default())]
    Refused {
        /// The `RESULT=` code, e.g. `KEY_NOT_FOUND`, `CANT_REACH_PEER`.
        result: String,
        /// The `MESSAGE=` text, if any.
        message: Option<String>,
    },
    /// A reply was missing, truncated, or not valid SAM.
    #[error("I2P SAM protocol error: {0}")]
    Protocol(String),
}

/// A SAM v3 session. Cloning shares the control connection; the session
/// ends when the last clone is dropped.
#[derive(Debug, Clone)]
pub struct Session {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    id: String,
    destination: String,
    control: Mutex<TcpStream>,
    bridge: String,
}

/// A connected SAM stream (after [`Session::connect`] or
/// [`Session::accept`]). Implements [`AsyncRead`] and [`AsyncWrite`].
#[derive(Debug)]
pub struct Stream {
    inner: TcpStream,
}

impl Stream {
    /// The underlying TCP connection to the SAM bridge.
    #[must_use]
    pub fn into_inner(self) -> TcpStream {
        self.inner
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl Session {
    /// Creates a STREAM session on `config`'s bridge.
    ///
    /// # Errors
    /// If the bridge cannot be reached, refuses the session, or sends a
    /// malformed reply.
    pub async fn create(config: &SamConfig, dest: DestinationSpec) -> Result<Self, SamError> {
        let mut control = TcpStream::connect(config.bridge.as_str()).await?;
        hello(&mut control, config).await?;
        let id = unique_id();
        let destination = match dest {
            DestinationSpec::Transient => "TRANSIENT".to_owned(),
            DestinationSpec::PrivateKey(key) => key,
        };
        write_line(
            &mut control,
            &command(
                "SESSION CREATE",
                &[
                    ("STYLE", "STREAM"),
                    ("ID", &id),
                    ("DESTINATION", &destination),
                ],
            ),
        )
        .await?;
        let reply = read_fields(&mut control).await?;
        expect_ok(&reply)?;
        let destination = field(&reply, "DESTINATION")?.to_owned();
        if destination.is_empty() {
            return Err(SamError::Protocol("empty DESTINATION".into()));
        }
        Ok(Self {
            inner: Arc::new(Inner {
                id,
                destination,
                control: Mutex::new(control),
                bridge: config.bridge.clone(),
            }),
        })
    }

    /// The session nickname sent to the bridge.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.inner.id
    }

    /// Our public destination (the `DESTINATION` the bridge returned).
    #[must_use]
    pub fn destination(&self) -> &str {
        &self.inner.destination
    }

    /// Resolves an I2P name (`example.i2p`, or `ME` for our destination)
    /// to a base64 destination.
    ///
    /// # Errors
    /// [`SamError::Refused`] with `KEY_NOT_FOUND` or `INVALID_KEY` if the
    /// bridge does not know the name; I/O and protocol errors otherwise.
    pub async fn lookup(&self, name: &str) -> Result<String, SamError> {
        let mut control = self.inner.control.lock().await;
        write_line(&mut control, &command("NAMING LOOKUP", &[("NAME", name)])).await?;
        let reply = read_fields(&mut control).await?;
        expect_ok(&reply)?;
        Ok(field(&reply, "VALUE")?.to_owned())
    }

    /// Opens an outbound stream to `destination` (a base64 destination
    /// from a lookup, or a session's [`Self::destination`]).
    ///
    /// # Errors
    /// [`SamError::Refused`] with `CANT_REACH_PEER` if the peer cannot
    /// be reached; I/O and protocol errors otherwise.
    pub async fn connect(&self, destination: &str) -> Result<Stream, SamError> {
        let mut stream = self.new_channel().await?;
        write_line(
            &mut stream,
            &command(
                "STREAM CONNECT",
                &[
                    ("ID", &self.inner.id),
                    ("DESTINATION", destination),
                    ("SILENT", "false"),
                ],
            ),
        )
        .await?;
        let reply = read_fields(&mut stream).await?;
        expect_ok(&reply)?;
        Ok(Stream { inner: stream })
    }

    /// Waits for an inbound stream. Returns the stream and the peer's
    /// destination (`SILENT=false`).
    ///
    /// # Errors
    /// If the bridge refuses, the session is gone, or the reply is
    /// malformed.
    pub async fn accept(&self) -> Result<(Stream, String), SamError> {
        let mut stream = self.new_channel().await?;
        write_line(
            &mut stream,
            &command(
                "STREAM ACCEPT",
                &[("ID", &self.inner.id), ("SILENT", "false")],
            ),
        )
        .await?;
        let reply = read_fields(&mut stream).await?;
        expect_ok(&reply)?;
        let from = read_line(&mut stream).await?;
        if from.is_empty() {
            return Err(SamError::Protocol("empty inbound destination".into()));
        }
        Ok((Stream { inner: stream }, from))
    }

    async fn new_channel(&self) -> Result<TcpStream, SamError> {
        let mut stream = TcpStream::connect(self.inner.bridge.as_str()).await?;
        hello(&mut stream, &SamConfig::new(self.inner.bridge.clone())).await?;
        Ok(stream)
    }
}

async fn hello(stream: &mut TcpStream, config: &SamConfig) -> Result<(), SamError> {
    write_line(
        stream,
        &command(
            "HELLO VERSION",
            &[("MIN", &config.min_version), ("MAX", &config.max_version)],
        ),
    )
    .await?;
    let reply = read_fields(stream).await?;
    expect_ok(&reply)?;
    Ok(())
}

fn unique_id() -> String {
    format!("dravity-{:08x}", fastrand::u32(..))
}

async fn write_line(stream: &mut TcpStream, line: &str) -> Result<(), SamError> {
    stream.write_all(line.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    Ok(())
}

async fn read_line(stream: &mut TcpStream) -> Result<String, SamError> {
    let mut buf = Vec::with_capacity(128);
    let mut one = [0u8; 1];
    loop {
        stream.read_exact(&mut one).await?;
        match one.first().copied() {
            Some(b'\n') => break,
            Some(b'\r') | None => {}
            Some(b) => {
                buf.push(b);
                if buf.len() > MAX_LINE {
                    return Err(SamError::Protocol("line too long".into()));
                }
            }
        }
    }
    String::from_utf8(buf).map_err(|_| SamError::Protocol("line is not UTF-8".into()))
}

async fn read_fields(stream: &mut TcpStream) -> Result<HashMap<String, String>, SamError> {
    let line = read_line(stream).await?;
    if line.is_empty() {
        return Err(SamError::Protocol("empty reply".into()));
    }
    Ok(parse_fields(&line))
}

fn expect_ok(fields: &HashMap<String, String>) -> Result<(), SamError> {
    let result = fields
        .get("RESULT")
        .ok_or_else(|| SamError::Protocol("missing RESULT".into()))?;
    if result == "OK" {
        Ok(())
    } else {
        Err(SamError::Refused {
            result: result.clone(),
            message: fields.get("MESSAGE").cloned(),
        })
    }
}

fn field<'a>(fields: &'a HashMap<String, String>, key: &str) -> Result<&'a str, SamError> {
    fields
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| SamError::Protocol(format!("missing {key}")))
}

/// Quotes a SAM value if it contains whitespace or `"`.
fn escape(value: &str) -> String {
    if value
        .chars()
        .any(|c| c.is_whitespace() || c == '"' || c == '\\')
    {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        value.to_owned()
    }
}

/// Builds a SAM command line (`VERB KEY=value …`) without the trailing
/// newline.
fn command(verb: &str, args: &[(&str, &str)]) -> String {
    let mut out = verb.to_owned();
    for (key, value) in args {
        out.push(' ');
        out.push_str(key);
        out.push('=');
        out.push_str(&escape(value));
    }
    out
}

/// Parses `VERB … KEY=value KEY="quoted"` into a map of keys.
///
/// ```
/// let f = dravity_i2p::parse_fields("NAMING REPLY RESULT=OK NAME=a.i2p VALUE=dest");
/// assert_eq!(f.get("RESULT").map(String::as_str), Some("OK"));
/// assert_eq!(f.get("NAME").map(String::as_str), Some("a.i2p"));
/// assert_eq!(f.get("VALUE").map(String::as_str), Some("dest"));
/// ```
#[must_use]
pub fn parse_fields(line: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut s = line.trim();
    while !s.is_empty() {
        let Some(eq) = s.find('=') else {
            break;
        };
        let head = s.get(..eq).unwrap_or_default();
        let key = head.rsplit_once(' ').map_or(head, |(_, key)| key).trim();
        if key.is_empty() {
            break;
        }
        let after = s.get(eq + 1..).unwrap_or_default();
        let (value, rest) = if after.starts_with('"') {
            unquote(after.get(1..).unwrap_or_default())
        } else {
            match after.split_once(' ') {
                Some((value, rest)) => (value.to_owned(), rest),
                None => (after.to_owned(), ""),
            }
        };
        map.insert(key.to_owned(), value);
        s = rest.trim_start();
    }
    map
}

fn unquote(s: &str) -> (String, &str) {
    let mut out = String::new();
    let mut chars = s.char_indices().peekable();
    let mut escaped = false;
    while let Some((_, c)) = chars.next() {
        if escaped {
            out.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => {
                let rest = chars.peek().map_or("", |(j, _)| s.get(*j..).unwrap_or(""));
                return (out, rest);
            }
            _ => out.push(c),
        }
    }
    (out, "")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn parse_fields_reads_keys_and_quoted_values() {
        let f = parse_fields("HELLO REPLY RESULT=OK VERSION=3.1");
        assert_eq!(f.get("RESULT").unwrap(), "OK");
        assert_eq!(f.get("VERSION").unwrap(), "3.1");
        let f = parse_fields(r#"SESSION STATUS RESULT=OK DESTINATION="a b" MESSAGE="say \"hi\"""#);
        assert_eq!(f.get("DESTINATION").unwrap(), "a b");
        assert_eq!(f.get("MESSAGE").unwrap(), r#"say "hi""#);
        assert!(parse_fields("NO EQUALS AT ALL").is_empty());
    }

    #[test]
    fn command_quotes_values_with_spaces() {
        assert_eq!(
            command("NAMING LOOKUP", &[("NAME", "a.i2p")]),
            "NAMING LOOKUP NAME=a.i2p"
        );
        assert_eq!(command("X", &[("M", "a b")]), "X M=\"a b\"");
    }

    #[test]
    fn refused_display_includes_the_code() {
        let e = SamError::Refused {
            result: "KEY_NOT_FOUND".into(),
            message: Some("no such name".into()),
        };
        assert!(e.to_string().contains("KEY_NOT_FOUND"));
        assert!(e.to_string().contains("no such name"));
    }
}
