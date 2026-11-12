//! A peer connection over TCP or uTP, possibly MSE-encrypted.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use dravity_mse::MseStream;
use dravity_utp::UtpStream;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

use crate::status::Transport;

/// The transport under a peer connection.
#[derive(Debug)]
pub(crate) enum RawStream {
    Tcp(TcpStream),
    Utp(UtpStream),
    /// A [`crate::transport::Network`] stream (reported as TCP).
    Boxed(crate::transport::BoxedStream),
}

impl RawStream {
    /// The transport.
    pub(crate) const fn kind(&self) -> Transport {
        match self {
            Self::Tcp(_) | Self::Boxed(_) => Transport::Tcp,
            Self::Utp(_) => Transport::Utp,
        }
    }
}

/// A peer connection: a transport, with or without MSE.
#[derive(Debug)]
pub(crate) struct PeerStream(MseStream<RawStream>);

impl PeerStream {
    /// A connection without MSE.
    pub(crate) const fn plain(raw: RawStream) -> Self {
        Self(MseStream::plain(raw))
    }

    /// A connection after an MSE handshake.
    pub(crate) const fn mse(stream: MseStream<RawStream>) -> Self {
        Self(stream)
    }

    /// Returns `bytes`, read before (already decrypted), first.
    pub(crate) fn with_prefix(self, bytes: Vec<u8>) -> Self {
        Self(self.0.with_prefix(bytes))
    }

    /// Short transport name for logs.
    pub(crate) const fn transport(&self) -> &'static str {
        self.kind().as_str()
    }

    /// The transport.
    pub(crate) const fn kind(&self) -> Transport {
        self.0.get_ref().kind()
    }

    /// Whether the connection began with an MSE handshake (RC4 or the
    /// plaintext method).
    pub(crate) const fn uses_mse(&self) -> bool {
        self.0.method().is_some()
    }

    /// Whether the payload is RC4-encrypted (MSE). MSE with the plaintext
    /// method only hides the handshake, and does not count.
    pub(crate) const fn encrypted(&self) -> bool {
        self.0.is_encrypted()
    }
}

impl AsyncRead for PeerStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl AsyncWrite for PeerStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().0).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.0.is_write_vectored()
    }
}

impl AsyncRead for RawStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_read(cx, buf),
            Self::Utp(s) => Pin::new(s).poll_read(cx, buf),
            Self::Boxed(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for RawStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_write(cx, buf),
            Self::Utp(s) => Pin::new(s).poll_write(cx, buf),
            Self::Boxed(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_flush(cx),
            Self::Utp(s) => Pin::new(s).poll_flush(cx),
            Self::Boxed(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_shutdown(cx),
            Self::Utp(s) => Pin::new(s).poll_shutdown(cx),
            Self::Boxed(s) => Pin::new(s).poll_shutdown(cx),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Tcp(s) => Pin::new(s).poll_write_vectored(cx, bufs),
            Self::Utp(s) => Pin::new(s).poll_write_vectored(cx, bufs),
            Self::Boxed(s) => Pin::new(s).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            Self::Tcp(s) => s.is_write_vectored(),
            Self::Utp(s) => s.is_write_vectored(),
            Self::Boxed(s) => s.is_write_vectored(),
        }
    }
}
