//! The handshake over Tokio I/O, and the encrypted stream after it.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};

use crate::MseError;
use crate::crypto::Cipher;
use crate::handshake::{CryptoSet, Established, Initiator, Method, Responder};

/// Read size while handshaking. Small reads keep payload that follows the
/// handshake mostly in the socket, but anything read is kept.
const READ_CHUNK: usize = 1024;

/// A connection after the MSE handshake (or a plain one): reads return
/// decrypted payload, writes are encrypted when the method is RC4.
///
/// Bytes that arrived with the handshake are returned first.
pub struct MseStream<S> {
    inner: S,
    method: Option<Method>,
    /// (encrypt, decrypt) for RC4; boxed to keep plain streams small.
    ciphers: Option<Box<(Cipher, Cipher)>>,
    /// Decrypted bytes to return before reading `inner`.
    prefix: Vec<u8>,
    prefix_pos: usize,
    /// Encrypted bytes accepted from the caller but not yet written.
    pending: Vec<u8>,
    pending_pos: usize,
}

impl<S: std::fmt::Debug> std::fmt::Debug for MseStream<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MseStream")
            .field("inner", &self.inner)
            .field("method", &self.method)
            .field("buffered", &(self.prefix.len() - self.prefix_pos))
            .field("pending", &(self.pending.len() - self.pending_pos))
            .finish()
    }
}

impl<S> MseStream<S> {
    /// A connection without MSE: bytes pass through unchanged.
    pub const fn plain(inner: S) -> Self {
        Self {
            inner,
            method: None,
            ciphers: None,
            prefix: Vec::new(),
            prefix_pos: 0,
            pending: Vec::new(),
            pending_pos: 0,
        }
    }

    /// A connection whose handshake finished with `established`.
    #[must_use]
    pub fn established(inner: S, established: Established) -> Self {
        let (method, ciphers, leftover) = established.into_parts();
        Self {
            method: Some(method),
            ciphers: ciphers.map(Box::new),
            prefix: leftover,
            ..Self::plain(inner)
        }
    }

    /// Returns `bytes` (already decrypted) from reads before anything else,
    /// e.g. bytes read to tell a plain handshake from an MSE one.
    #[must_use]
    pub fn with_prefix(mut self, mut bytes: Vec<u8>) -> Self {
        let rest = self.prefix.split_off(self.prefix_pos);
        bytes.extend_from_slice(&rest);
        self.prefix = bytes;
        self.prefix_pos = 0;
        self
    }

    /// The negotiated method, or `None` for a connection without MSE.
    #[must_use]
    pub const fn method(&self) -> Option<Method> {
        self.method
    }

    /// Whether payload is RC4-encrypted on the wire.
    #[must_use]
    pub const fn is_encrypted(&self) -> bool {
        matches!(self.method, Some(Method::Rc4))
    }

    /// The underlying connection.
    pub const fn get_ref(&self) -> &S {
        &self.inner
    }

    /// The underlying connection. Reading or writing it directly corrupts an
    /// encrypted stream.
    pub const fn get_mut(&mut self) -> &mut S {
        &mut self.inner
    }
}

impl<S: AsyncWrite + Unpin> MseStream<S> {
    /// Writes out encrypted bytes accepted earlier.
    fn poll_pending(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while let Some(rest) = self
            .pending
            .get(self.pending_pos..)
            .filter(|r| !r.is_empty())
        {
            let n = ready!(Pin::new(&mut self.inner).poll_write(cx, rest))?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.pending_pos += n;
        }
        self.pending.clear();
        self.pending_pos = 0;
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for MseStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Some(rest) = this.prefix.get(this.prefix_pos..).filter(|r| !r.is_empty()) {
            let n = rest.len().min(buf.remaining());
            buf.put_slice(rest.get(..n).unwrap_or_default());
            this.prefix_pos += n;
            if this.prefix_pos == this.prefix.len() {
                this.prefix = Vec::new();
                this.prefix_pos = 0;
            }
            return Poll::Ready(Ok(()));
        }
        let before = buf.filled().len();
        ready!(Pin::new(&mut this.inner).poll_read(cx, buf))?;
        if let (Some(ciphers), Some(new)) = (&mut this.ciphers, buf.filled_mut().get_mut(before..))
        {
            ciphers.1.apply(new);
        }
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for MseStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        ready!(this.poll_pending(cx))?;
        let Some(ciphers) = &mut this.ciphers else {
            return Pin::new(&mut this.inner).poll_write(cx, buf);
        };
        // The keystream advances as bytes are encrypted, so accept the whole
        // buffer now and write it out over this and later calls.
        this.pending.extend_from_slice(buf);
        if let Some(fresh) = this.pending.get_mut(this.pending_pos..) {
            ciphers.0.apply(fresh);
        }
        match this.poll_pending(cx) {
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(())) | Poll::Pending => Poll::Ready(Ok(buf.len())),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.ciphers.is_none() && this.pending.is_empty() {
            return Pin::new(&mut this.inner).poll_write_vectored(cx, bufs);
        }
        let buf = bufs
            .iter()
            .find(|b| !b.is_empty())
            .map_or(&[][..], |b| &**b);
        Pin::new(this).poll_write(cx, buf)
    }

    fn is_write_vectored(&self) -> bool {
        self.ciphers.is_none() && self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_pending(cx))?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        ready!(this.poll_pending(cx))?;
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

/// Bytes the handshake still has to send.
#[derive(Debug, Default)]
struct Outbox {
    bytes: Vec<u8>,
    sent: usize,
}

impl Outbox {
    fn push(&mut self, bytes: &[u8]) {
        if self.sent == self.bytes.len() {
            self.bytes.clear();
            self.sent = 0;
        }
        self.bytes.extend_from_slice(bytes);
    }

    /// Sends what is left, then flushes.
    async fn finish<S: AsyncWrite + Unpin>(&mut self, stream: &mut S) -> io::Result<()> {
        if let Some(rest) = self.bytes.get(self.sent..) {
            stream.write_all(rest).await?;
        }
        self.sent = self.bytes.len();
        stream.flush().await
    }
}

/// Writes queued handshake bytes while waiting for the peer's, so neither
/// side blocks on a full send buffer while the other does the same (each
/// handshake step can be over 600 bytes). Returns the bytes read (0: the
/// peer closed).
async fn exchange<S>(stream: &mut S, outbox: &mut Outbox, buf: &mut [u8]) -> io::Result<usize>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    std::future::poll_fn(|cx| {
        let mut wrote = false;
        while let Some(rest) = outbox.bytes.get(outbox.sent..).filter(|r| !r.is_empty()) {
            match Pin::new(&mut *stream).poll_write(cx, rest) {
                Poll::Ready(Ok(0)) => return Poll::Ready(Err(io::ErrorKind::WriteZero.into())),
                Poll::Ready(Ok(n)) => {
                    outbox.sent += n;
                    wrote = true;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => break,
            }
        }
        if wrote && let Poll::Ready(Err(e)) = Pin::new(&mut *stream).poll_flush(cx) {
            return Poll::Ready(Err(e));
        }
        let mut read = ReadBuf::new(buf);
        ready!(Pin::new(&mut *stream).poll_read(cx, &mut read))?;
        Poll::Ready(Ok(read.filled().len()))
    })
    .await
}

/// Runs the initiator side over `stream` for the torrent `skey`, offering
/// `provide`. Apply a timeout around it: a silent peer is waited for.
///
/// # Errors
/// [`MseError::Closed`] if the peer hangs up (typically a client without
/// MSE), or any handshake failure.
pub async fn initiate<S>(
    mut stream: S,
    skey: &[u8; 20],
    provide: CryptoSet,
) -> Result<MseStream<S>, MseError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut hs = Initiator::new(skey, provide)?;
    let mut outbox = Outbox::default();
    let mut buf = [0u8; READ_CHUNK];
    loop {
        outbox.push(&hs.take_output());
        let n = exchange(&mut stream, &mut outbox, &mut buf).await?;
        if n == 0 {
            return Err(MseError::Closed);
        }
        if let Some(established) = hs.receive(buf.get(..n).unwrap_or_default())? {
            outbox.push(&hs.take_output());
            outbox.finish(&mut stream).await?;
            return Ok(MseStream::established(stream, established));
        }
    }
}

/// Runs the responder side over `stream`, accepting the methods in
/// `allowed`. `received` holds bytes already read from the connection
/// (e.g. while telling MSE from a plain handshake). `lookup` maps
/// `HASH('req2', SKEY)` to a served torrent's SKEY (see
/// [`req2_hash`](crate::req2_hash)). Returns the stream and the SKEY.
/// Apply a timeout around it.
///
/// # Errors
/// [`MseError::UnknownTorrent`] for a torrent `lookup` does not know,
/// [`MseError::Closed`] if the peer hangs up, or any handshake failure.
pub async fn respond<S>(
    mut stream: S,
    received: &[u8],
    allowed: CryptoSet,
    lookup: impl Fn(&[u8; 20]) -> Option<[u8; 20]>,
) -> Result<(MseStream<S>, [u8; 20]), MseError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut hs = Responder::new(allowed)?;
    let mut outbox = Outbox::default();
    let mut done = hs.receive(received, &lookup)?;
    let mut buf = [0u8; READ_CHUNK];
    loop {
        outbox.push(&hs.take_output());
        if let Some(incoming) = done {
            outbox.finish(&mut stream).await?;
            return Ok((
                MseStream::established(stream, incoming.established),
                incoming.skey,
            ));
        }
        let n = exchange(&mut stream, &mut outbox, &mut buf).await?;
        if n == 0 {
            return Err(MseError::Closed);
        }
        done = hs.receive(buf.get(..n).unwrap_or_default(), &lookup)?;
    }
}
