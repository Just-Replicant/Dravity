//! Message Stream Encryption (MSE, also called PE or protocol encryption)
//! for BitTorrent peer connections.
//!
//! MSE wraps a peer connection in a Diffie-Hellman key exchange (the
//! 768-bit prime of the specification, generator 2, 160-bit private keys)
//! followed by an RC4 stream keyed with SHA-1 of the shared secret and the
//! torrent's info-hash (the first 1024 bytes of keystream are discarded).
//! The peers then agree on the payload method: RC4 for the whole stream, or
//! plaintext after the encrypted header. Random padding hides the lengths of
//! the key exchange.
//!
//! MSE is obfuscation against passive traffic shaping, not authenticated
//! transport security: RC4 and 768-bit DH are weak by today's standards and
//! nothing authenticates the peer. It only proves that both sides know the
//! info-hash.
//!
//! The crate has two layers:
//! - [`Initiator`] and [`Responder`]: the handshake as sans-IO state machines
//!   (bytes in, bytes out), fuzzed and testable without sockets;
//! - with the default `stream` feature, [`initiate`], [`respond`] and
//!   [`MseStream`], which run the handshake over Tokio I/O and then encrypt
//!   and decrypt transparently.
//!
//! ```
//! # #[cfg(feature = "stream")]
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> Result<(), dravity_mse::MseError> {
//! use dravity_mse::{CryptoSet, Method, initiate, respond, req2_hash};
//! use tokio::io::{AsyncReadExt, AsyncWriteExt};
//!
//! let info_hash = [7u8; 20];
//! let (a, b) = tokio::io::duplex(4096);
//! let incoming = tokio::spawn(async move {
//!     // Finds the torrent from the obfuscated hash the initiator sent.
//!     let lookup = |hash: &[u8; 20]| (*hash == req2_hash(&info_hash)).then_some(info_hash);
//!     respond(b, &[], CryptoSet::ALL, lookup).await
//! });
//! let mut a = initiate(a, &info_hash, CryptoSet::ALL).await?;
//! let (mut b, skey) = incoming.await.expect("task")?;
//! assert_eq!(skey, info_hash);
//! assert_eq!(a.method(), Some(Method::Rc4));
//!
//! a.write_all(b"hello").await?;
//! a.flush().await?;
//! let mut got = [0; 5];
//! b.read_exact(&mut got).await?;
//! assert_eq!(&got, b"hello");
//! # Ok(())
//! # }
//! # #[cfg(not(feature = "stream"))]
//! # fn main() {}
//! ```

mod crypto;
mod handshake;
#[cfg(feature = "stream")]
mod stream;

pub use crypto::req2_hash;
pub use handshake::{
    CryptoSet, Established, Incoming, Initiator, MAX_IA, MAX_PAD, Method, Responder,
};
#[cfg(feature = "stream")]
pub use stream::{MseStream, initiate, respond};

/// How a client uses MSE.
///
/// The three modes of the specification's notes for BitTorrent clients.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Policy {
    /// Never encrypt: outgoing connections start with the plain BitTorrent
    /// handshake and incoming MSE handshakes are refused.
    Disabled,
    /// Prefer encryption: outgoing connections try MSE first (offering RC4
    /// and plaintext, RC4 preferred) and are redialed in plaintext if that
    /// fails; incoming connections may use either.
    #[default]
    Enabled,
    /// Only RC4-encrypted MSE connections, in both directions.
    Required,
}

impl Policy {
    /// Payload methods offered on outgoing and accepted on incoming MSE
    /// connections; empty when disabled.
    #[must_use]
    pub const fn methods(self) -> CryptoSet {
        match self {
            Self::Disabled => CryptoSet::NONE,
            Self::Enabled => CryptoSet::ALL,
            Self::Required => CryptoSet::RC4,
        }
    }

    /// Whether plain BitTorrent connections are allowed.
    #[must_use]
    pub const fn allows_plaintext_handshake(self) -> bool {
        !matches!(self, Self::Required)
    }

    /// Short name (`disabled`, `enabled`, `required`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Enabled => "enabled",
            Self::Required => "required",
        }
    }
}

impl std::fmt::Display for Policy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether `prefix` (the first bytes of an incoming connection) starts like
/// a plain BitTorrent handshake (`\x13BitTorrent protocol`) rather than an
/// MSE public key. `None` until enough bytes arrived to tell.
#[must_use]
pub fn is_plaintext_handshake(prefix: &[u8]) -> Option<bool> {
    const PSTR: &[u8; 20] = b"\x13BitTorrent protocol";
    let n = prefix.len().min(PSTR.len());
    let (head, pstr) = (prefix.get(..n)?, PSTR.get(..n)?);
    if head != pstr {
        Some(false)
    } else if n == PSTR.len() {
        Some(true)
    } else {
        None
    }
}

/// Why an MSE handshake failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MseError {
    /// The operating system had no randomness for a key or padding.
    #[error("no system randomness: {0}")]
    Random(String),
    /// The peer's Diffie-Hellman public key is out of range.
    #[error("invalid Diffie-Hellman public key")]
    InvalidPublicKey,
    /// The synchronisation pattern did not appear within the padding limit.
    #[error("handshake synchronisation pattern not found")]
    NoSync,
    /// The verification constant did not decrypt to zeros.
    #[error("bad verification constant")]
    BadVerification,
    /// No torrent matches the peer's obfuscated info-hash.
    #[error("unknown torrent")]
    UnknownTorrent,
    /// The peer offered no method we accept, or selected one we did not offer.
    #[error("no common encryption method")]
    NoCommonMethod,
    /// A padding length above [`MAX_PAD`].
    #[error("padding too long")]
    PaddingTooLong,
    /// The handshake already finished.
    #[error("handshake already finished")]
    Finished,
    /// The peer answered with a plain BitTorrent handshake: it does not
    /// speak MSE.
    #[error("peer answered with a plain BitTorrent handshake")]
    PlaintextPeer,
    /// The connection closed during the handshake.
    #[error("connection closed during the encryption handshake")]
    Closed,
    /// I/O error on the underlying connection.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<getrandom::Error> for MseError {
    fn from(e: getrandom::Error) -> Self {
        Self::Random(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_detection() {
        assert_eq!(is_plaintext_handshake(b""), None);
        assert_eq!(is_plaintext_handshake(b"\x13BitTor"), None);
        assert_eq!(
            is_plaintext_handshake(b"\x13BitTorrent protocol"),
            Some(true)
        );
        assert_eq!(
            is_plaintext_handshake(b"\x13BitTorrent protocol\0\0"),
            Some(true)
        );
        assert_eq!(is_plaintext_handshake(b"\x13BitTorX"), Some(false));
        assert_eq!(is_plaintext_handshake(&[0xff; 96]), Some(false));
    }

    #[test]
    fn policies() {
        assert_eq!(Policy::default(), Policy::Enabled);
        assert!(Policy::Disabled.methods().is_empty());
        assert_eq!(Policy::Required.methods(), CryptoSet::RC4);
        assert!(!Policy::Required.allows_plaintext_handshake());
        assert!(Policy::Enabled.allows_plaintext_handshake());
        assert_eq!(Policy::Required.to_string(), "required");
    }
}
