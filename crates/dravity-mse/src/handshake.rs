//! The MSE handshake as sans-IO state machines.
//!
//! ```text
//! 1 A->B: Ya, PadA
//! 2 B->A: Yb, PadB
//! 3 A->B: HASH('req1', S), HASH('req2', SKEY) xor HASH('req3', S),
//!         ENCRYPT(VC, crypto_provide, len(PadC), PadC, len(IA)), ENCRYPT(IA)
//! 4 B->A: ENCRYPT(VC, crypto_select, len(PadD), PadD), ENCRYPT2(payload)
//! 5 A->B: ENCRYPT2(payload)
//! ```
//!
//! Feed received bytes to [`Initiator::receive`] or [`Responder::receive`],
//! and send what `take_output` returns after each call. Both report
//! [`Established`] once the payload stream begins.

use std::mem;

use crate::MseError;
use crate::crypto::{Cipher, KEY_LEN, KeyPair, PRIVATE_LEN, hash};

/// Maximum length of each padding (PadA to PadD).
pub const MAX_PAD: usize = 512;

/// Maximum initial payload (IA) length: its length field is 16 bits.
pub const MAX_IA: usize = u16::MAX as usize;

/// The verification constant: 8 zero bytes.
const VC: [u8; 8] = [0; 8];

/// VC, crypto field and a padding length.
const HEADER_LEN: usize = 8 + 4 + 2;

/// Payload encryption methods (the `crypto_provide` and `crypto_select`
/// bit fields).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CryptoSet(u32);

impl CryptoSet {
    /// No method.
    pub const NONE: Self = Self(0);
    /// Plaintext after the encrypted header (`0x01`).
    pub const PLAINTEXT: Self = Self(0x01);
    /// RC4 for the whole stream (`0x02`).
    pub const RC4: Self = Self(0x02);
    /// Plaintext and RC4.
    pub const ALL: Self = Self(0x03);

    /// The set from a wire bit field; unknown bits are ignored, as the
    /// specification asks.
    #[must_use]
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits & Self::ALL.0)
    }

    /// The wire bit field.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Whether every method of `other` is in `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Methods in both sets.
    #[must_use]
    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Whether the set is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The method a responder selects: RC4 when both sides allow it,
    /// otherwise plaintext.
    #[must_use]
    pub const fn preferred(self) -> Option<Method> {
        if self.contains(Self::RC4) {
            Some(Method::Rc4)
        } else if self.contains(Self::PLAINTEXT) {
            Some(Method::Plaintext)
        } else {
            None
        }
    }
}

impl From<Method> for CryptoSet {
    fn from(method: Method) -> Self {
        match method {
            Method::Plaintext => Self::PLAINTEXT,
            Method::Rc4 => Self::RC4,
        }
    }
}

/// The negotiated payload method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    /// The payload is sent unencrypted after the handshake.
    Plaintext,
    /// The payload continues the handshake's RC4 streams.
    Rc4,
}

/// A finished handshake: the method and what the payload stream needs.
#[derive(Debug)]
pub struct Established {
    method: Method,
    /// (encrypt, decrypt) when the method is RC4.
    ciphers: Option<(Cipher, Cipher)>,
    /// Payload that already arrived, decrypted (for a responder, IA first).
    leftover: Vec<u8>,
}

impl Established {
    /// The negotiated payload method.
    #[must_use]
    pub const fn method(&self) -> Method {
        self.method
    }

    /// Payload bytes that arrived with the handshake, already decrypted.
    #[must_use]
    pub fn leftover(&self) -> &[u8] {
        &self.leftover
    }

    /// Encrypts outgoing payload in place (no-op for plaintext).
    pub fn encrypt(&mut self, buf: &mut [u8]) {
        if let Some((enc, _)) = &mut self.ciphers {
            enc.apply(buf);
        }
    }

    /// Decrypts incoming payload in place (no-op for plaintext).
    pub fn decrypt(&mut self, buf: &mut [u8]) {
        if let Some((_, dec)) = &mut self.ciphers {
            dec.apply(buf);
        }
    }

    #[cfg_attr(not(feature = "stream"), allow(dead_code))]
    pub(crate) fn into_parts(self) -> (Method, Option<(Cipher, Cipher)>, Vec<u8>) {
        (self.method, self.ciphers, self.leftover)
    }
}

/// A finished incoming handshake.
#[derive(Debug)]
pub struct Incoming {
    /// The payload stream.
    pub established: Established,
    /// The torrent (`SKEY`) the initiator asked for.
    pub skey: [u8; 20],
}

/// Random padding of 0 to [`MAX_PAD`] bytes.
fn random_pad() -> Result<Vec<u8>, MseError> {
    // MAX_PAD + 1 choices of length.
    const CHOICES: u32 = 513;
    let len = usize::try_from(getrandom::u32()? % CHOICES).unwrap_or(0);
    let mut pad = vec![0u8; len];
    getrandom::fill(&mut pad)?;
    Ok(pad)
}

fn xor20(a: &[u8; 20], b: &[u8; 20]) -> [u8; 20] {
    let mut out = *a;
    for (o, b) in out.iter_mut().zip(b) {
        *o ^= b;
    }
    out
}

/// Index of `needle` in the first `limit` bytes of `haystack`.
fn find(haystack: &[u8], needle: &[u8], limit: usize) -> Option<usize> {
    let window = haystack.get(..limit.min(haystack.len()))?;
    window.windows(needle.len()).position(|w| w == needle)
}

/// Splits the 14-byte decrypted header into (VC ok, crypto bits, pad len).
fn parse_header(header: &[u8; HEADER_LEN]) -> (bool, u32, usize) {
    let [v0, v1, v2, v3, v4, v5, v6, v7, c0, c1, c2, c3, p0, p1] = *header;
    (
        [v0, v1, v2, v3, v4, v5, v6, v7] == VC,
        u32::from_be_bytes([c0, c1, c2, c3]),
        usize::from(u16::from_be_bytes([p0, p1])),
    )
}

/// Removes and returns the first `N` bytes of `buf`, if there are that many.
fn take<const N: usize>(buf: &mut Vec<u8>) -> Option<[u8; N]> {
    let head: [u8; N] = buf.get(..N)?.try_into().ok()?;
    buf.drain(..N);
    Some(head)
}

/// Removes and returns the first `n` bytes of `buf`, if there are that many.
fn take_vec(buf: &mut Vec<u8>, n: usize) -> Option<Vec<u8>> {
    (buf.len() >= n).then(|| buf.drain(..n).collect())
}

#[derive(Debug)]
enum InitiatorState {
    PeerKey,
    Sync { pattern: [u8; 8] },
    Select,
    PadD { len: usize, method: Method },
    Done,
}

/// The connecting side (A) of a handshake.
#[derive(Debug)]
pub struct Initiator {
    keys: KeyPair,
    skey: [u8; 20],
    provide: CryptoSet,
    state: InitiatorState,
    enc: Option<Cipher>,
    dec: Option<Cipher>,
    buf: Vec<u8>,
    out: Vec<u8>,
}

impl Initiator {
    /// Starts a handshake for the torrent `skey` (its 20-byte info-hash, as
    /// in the BitTorrent handshake), offering the methods in `provide`. The
    /// public key and random padding are ready in [`take_output`](Self::take_output).
    ///
    /// # Errors
    /// [`MseError::Random`] if the OS has no randomness;
    /// [`MseError::NoCommonMethod`] if `provide` is empty.
    pub fn new(skey: &[u8; 20], provide: CryptoSet) -> Result<Self, MseError> {
        Self::with_key(KeyPair::generate()?, random_pad()?, skey, provide)
    }

    /// Like [`new`](Self::new) with a fixed private key and PadA, for
    /// reproducible tests and fuzzing. Never use a fixed key on a network.
    ///
    /// # Errors
    /// As [`new`](Self::new); [`MseError::PaddingTooLong`] for a PadA
    /// longer than [`MAX_PAD`].
    pub fn with_private_key(
        private: &[u8; PRIVATE_LEN],
        pad: &[u8],
        skey: &[u8; 20],
        provide: CryptoSet,
    ) -> Result<Self, MseError> {
        if pad.len() > MAX_PAD {
            return Err(MseError::PaddingTooLong);
        }
        Self::with_key(KeyPair::from_private(private), pad.to_vec(), skey, provide)
    }

    fn with_key(
        keys: KeyPair,
        pad: Vec<u8>,
        skey: &[u8; 20],
        provide: CryptoSet,
    ) -> Result<Self, MseError> {
        if provide.is_empty() {
            return Err(MseError::NoCommonMethod);
        }
        let mut out = Vec::with_capacity(KEY_LEN + pad.len());
        out.extend_from_slice(keys.public());
        out.extend_from_slice(&pad);
        Ok(Self {
            keys,
            skey: *skey,
            provide,
            state: InitiatorState::PeerKey,
            enc: None,
            dec: None,
            buf: Vec::new(),
            out,
        })
    }

    /// Bytes to send to the peer now.
    pub fn take_output(&mut self) -> Vec<u8> {
        mem::take(&mut self.out)
    }

    /// Handles bytes from the peer. Returns the established stream once the
    /// responder's header is complete; bytes after it are in
    /// [`Established::leftover`].
    ///
    /// # Errors
    /// Any handshake failure; the connection should then be closed.
    pub fn receive(&mut self, data: &[u8]) -> Result<Option<Established>, MseError> {
        if matches!(self.state, InitiatorState::Done) {
            return Err(MseError::Finished);
        }
        self.buf.extend_from_slice(data);
        loop {
            match self.state {
                InitiatorState::PeerKey => {
                    // A client without MSE may answer with its own plain
                    // handshake instead of hanging up: fail fast.
                    if crate::is_plaintext_handshake(&self.buf) == Some(true) {
                        return Err(MseError::PlaintextPeer);
                    }
                    let Some(theirs) = take::<KEY_LEN>(&mut self.buf) else {
                        return Ok(None);
                    };
                    self.send_step3(&theirs)?;
                }
                InitiatorState::Sync { pattern } => {
                    match find(&self.buf, &pattern, MAX_PAD + VC.len()) {
                        Some(at) => {
                            self.buf.drain(..at);
                            self.state = InitiatorState::Select;
                        }
                        None if self.buf.len() >= MAX_PAD + VC.len() => {
                            return Err(MseError::NoSync);
                        }
                        None => return Ok(None),
                    }
                }
                InitiatorState::Select => {
                    let Some(mut header) = take::<HEADER_LEN>(&mut self.buf) else {
                        return Ok(None);
                    };
                    self.decrypt(&mut header);
                    let (vc_ok, select, pad) = parse_header(&header);
                    if !vc_ok {
                        return Err(MseError::BadVerification);
                    }
                    let method = match select {
                        0x01 => Method::Plaintext,
                        0x02 => Method::Rc4,
                        _ => return Err(MseError::NoCommonMethod),
                    };
                    if !self.provide.contains(method.into()) {
                        return Err(MseError::NoCommonMethod);
                    }
                    if pad > MAX_PAD {
                        return Err(MseError::PaddingTooLong);
                    }
                    self.state = InitiatorState::PadD { len: pad, method };
                }
                InitiatorState::PadD { len, method } => {
                    let Some(mut pad) = take_vec(&mut self.buf, len) else {
                        return Ok(None);
                    };
                    self.decrypt(&mut pad);
                    self.state = InitiatorState::Done;
                    return Ok(Some(self.establish(method)));
                }
                InitiatorState::Done => return Err(MseError::Finished),
            }
        }
    }

    fn send_step3(&mut self, theirs: &[u8; KEY_LEN]) -> Result<(), MseError> {
        let secret = self.keys.shared(theirs)?;
        let mut enc = Cipher::new(b"keyA", &secret, &self.skey);
        let dec = Cipher::new(b"keyB", &secret, &self.skey);
        // The responder's header starts with ENCRYPT(VC): the first 8 bytes
        // of its keystream.
        let mut pattern = VC;
        Cipher::new(b"keyB", &secret, &self.skey).apply(&mut pattern);

        self.out.extend_from_slice(&hash(&[b"req1", &secret]));
        self.out.extend_from_slice(&xor20(
            &hash(&[b"req2", &self.skey]),
            &hash(&[b"req3", &secret]),
        ));
        // VC, crypto_provide, len(PadC) = 0, len(IA) = 0: the BitTorrent
        // handshake follows once the method is known.
        let mut header = Vec::with_capacity(HEADER_LEN + 2);
        header.extend_from_slice(&VC);
        header.extend_from_slice(&self.provide.bits().to_be_bytes());
        header.extend_from_slice(&0u16.to_be_bytes());
        header.extend_from_slice(&0u16.to_be_bytes());
        enc.apply(&mut header);
        self.out.extend_from_slice(&header);

        self.enc = Some(enc);
        self.dec = Some(dec);
        self.state = InitiatorState::Sync { pattern };
        Ok(())
    }

    fn decrypt(&mut self, buf: &mut [u8]) {
        if let Some(dec) = &mut self.dec {
            dec.apply(buf);
        }
    }

    fn establish(&mut self, method: Method) -> Established {
        let mut leftover = mem::take(&mut self.buf);
        let ciphers = match (method, self.enc.take(), self.dec.take()) {
            (Method::Rc4, Some(enc), Some(mut dec)) => {
                dec.apply(&mut leftover);
                Some((enc, dec))
            }
            _ => None,
        };
        Established {
            method,
            ciphers,
            leftover,
        }
    }
}

#[derive(Debug)]
enum ResponderState {
    PeerKey,
    Sync {
        secret: Box<[u8; KEY_LEN]>,
        req1: [u8; 20],
    },
    Skey {
        secret: Box<[u8; KEY_LEN]>,
    },
    Provide,
    PadC {
        len: usize,
        provide: CryptoSet,
    },
    Ia {
        len: usize,
        provide: CryptoSet,
    },
    Done,
}

/// The accepting side (B) of a handshake.
#[derive(Debug)]
pub struct Responder {
    keys: KeyPair,
    pad: Vec<u8>,
    allowed: CryptoSet,
    state: ResponderState,
    skey: [u8; 20],
    enc: Option<Cipher>,
    dec: Option<Cipher>,
    buf: Vec<u8>,
    out: Vec<u8>,
}

impl Responder {
    /// Waits for an initiator, accepting the methods in `allowed`.
    ///
    /// # Errors
    /// [`MseError::Random`] if the OS has no randomness;
    /// [`MseError::NoCommonMethod`] if `allowed` is empty.
    pub fn new(allowed: CryptoSet) -> Result<Self, MseError> {
        Self::with_key(KeyPair::generate()?, random_pad()?, allowed)
    }

    /// Like [`new`](Self::new) with a fixed private key and PadB, for
    /// reproducible tests and fuzzing. Never use a fixed key on a network.
    ///
    /// # Errors
    /// As [`new`](Self::new); [`MseError::PaddingTooLong`] for a PadB
    /// longer than [`MAX_PAD`].
    pub fn with_private_key(
        private: &[u8; PRIVATE_LEN],
        pad: &[u8],
        allowed: CryptoSet,
    ) -> Result<Self, MseError> {
        if pad.len() > MAX_PAD {
            return Err(MseError::PaddingTooLong);
        }
        Self::with_key(KeyPair::from_private(private), pad.to_vec(), allowed)
    }

    fn with_key(keys: KeyPair, pad: Vec<u8>, allowed: CryptoSet) -> Result<Self, MseError> {
        if allowed.is_empty() {
            return Err(MseError::NoCommonMethod);
        }
        Ok(Self {
            keys,
            pad,
            allowed,
            state: ResponderState::PeerKey,
            skey: [0; 20],
            enc: None,
            dec: None,
            buf: Vec::new(),
            out: Vec::new(),
        })
    }

    /// Bytes to send to the peer now.
    pub fn take_output(&mut self) -> Vec<u8> {
        mem::take(&mut self.out)
    }

    /// Handles bytes from the peer. `lookup` maps `HASH('req2', SKEY)` (see
    /// [`req2_hash`](crate::req2_hash)) to the SKEY of a torrent we serve.
    /// Returns the established stream and the torrent once the initiator's
    /// header and IA are complete; IA and later bytes are in
    /// [`Established::leftover`]. Send [`take_output`](Self::take_output)
    /// before using the stream.
    ///
    /// # Errors
    /// Any handshake failure; the connection should then be closed.
    pub fn receive(
        &mut self,
        data: &[u8],
        lookup: impl Fn(&[u8; 20]) -> Option<[u8; 20]>,
    ) -> Result<Option<Incoming>, MseError> {
        if matches!(self.state, ResponderState::Done) {
            return Err(MseError::Finished);
        }
        self.buf.extend_from_slice(data);
        loop {
            match &self.state {
                ResponderState::PeerKey => {
                    let Some(theirs) = take::<KEY_LEN>(&mut self.buf) else {
                        return Ok(None);
                    };
                    let secret = Box::new(self.keys.shared(&theirs)?);
                    self.out.extend_from_slice(self.keys.public());
                    self.out.append(&mut self.pad);
                    let req1 = hash(&[b"req1", &secret[..]]);
                    self.state = ResponderState::Sync { secret, req1 };
                }
                ResponderState::Sync { secret, req1 } => {
                    match find(&self.buf, req1, MAX_PAD + req1.len()) {
                        Some(at) => {
                            self.buf.drain(..at + req1.len());
                            let secret = secret.clone();
                            self.state = ResponderState::Skey { secret };
                        }
                        None if self.buf.len() >= MAX_PAD + req1.len() => {
                            return Err(MseError::NoSync);
                        }
                        None => return Ok(None),
                    }
                }
                ResponderState::Skey { secret } => {
                    let Some(obfuscated) = take::<20>(&mut self.buf) else {
                        return Ok(None);
                    };
                    let req2 = xor20(&obfuscated, &hash(&[b"req3", &secret[..]]));
                    let skey = lookup(&req2).ok_or(MseError::UnknownTorrent)?;
                    self.dec = Some(Cipher::new(b"keyA", secret, &skey));
                    self.enc = Some(Cipher::new(b"keyB", secret, &skey));
                    self.skey = skey;
                    self.state = ResponderState::Provide;
                }
                ResponderState::Provide => {
                    let Some(mut header) = take::<HEADER_LEN>(&mut self.buf) else {
                        return Ok(None);
                    };
                    self.decrypt(&mut header);
                    let (vc_ok, provide, pad) = parse_header(&header);
                    if !vc_ok {
                        return Err(MseError::BadVerification);
                    }
                    if pad > MAX_PAD {
                        return Err(MseError::PaddingTooLong);
                    }
                    let provide = CryptoSet::from_bits(provide);
                    self.state = ResponderState::PadC { len: pad, provide };
                }
                &ResponderState::PadC { len, provide } => {
                    let Some(mut pad) = take_vec(&mut self.buf, len + 2) else {
                        return Ok(None);
                    };
                    self.decrypt(&mut pad);
                    let ia_len = match pad.get(len..) {
                        Some(&[hi, lo]) => usize::from(u16::from_be_bytes([hi, lo])),
                        _ => 0,
                    };
                    self.state = ResponderState::Ia {
                        len: ia_len,
                        provide,
                    };
                }
                &ResponderState::Ia { len, provide } => {
                    let Some(mut ia) = take_vec(&mut self.buf, len) else {
                        return Ok(None);
                    };
                    self.decrypt(&mut ia);
                    let method = provide
                        .intersection(self.allowed)
                        .preferred()
                        .ok_or(MseError::NoCommonMethod)?;
                    self.state = ResponderState::Done;
                    return Ok(Some(self.establish(method, ia)));
                }
                ResponderState::Done => return Err(MseError::Finished),
            }
        }
    }

    fn decrypt(&mut self, buf: &mut [u8]) {
        if let Some(dec) = &mut self.dec {
            dec.apply(buf);
        }
    }

    /// Sends ENCRYPT(VC, crypto_select, len(PadD) = 0) and builds the stream.
    fn establish(&mut self, method: Method, mut leftover: Vec<u8>) -> Incoming {
        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(&VC);
        header.extend_from_slice(&CryptoSet::from(method).bits().to_be_bytes());
        header.extend_from_slice(&0u16.to_be_bytes());
        if let Some(enc) = &mut self.enc {
            enc.apply(&mut header);
        }
        self.out.extend_from_slice(&header);

        let mut rest = mem::take(&mut self.buf);
        let ciphers = match (method, self.enc.take(), self.dec.take()) {
            (Method::Rc4, Some(enc), Some(mut dec)) => {
                dec.apply(&mut rest);
                Some((enc, dec))
            }
            _ => None,
        };
        leftover.append(&mut rest);
        Incoming {
            established: Established {
                method,
                ciphers,
                leftover,
            },
            skey: self.skey,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

    use super::*;
    use crate::req2_hash;

    const SKEY: [u8; 20] = [0xab; 20];

    fn lookup(h: &[u8; 20]) -> Option<[u8; 20]> {
        (*h == req2_hash(&SKEY)).then_some(SKEY)
    }

    /// Runs both sides to completion, delivering `chunk` bytes at a time.
    fn run(
        mut a: Initiator,
        mut b: Responder,
        chunk: usize,
    ) -> Result<(Established, Incoming), MseError> {
        let (mut done_a, mut done_b) = (None, None);
        for _ in 0..10_000 {
            let to_b = a.take_output();
            for part in to_b.chunks(chunk.max(1)) {
                if done_b.is_none() {
                    done_b = b.receive(part, lookup)?;
                }
            }
            let to_a = b.take_output();
            for part in to_a.chunks(chunk.max(1)) {
                if done_a.is_none() {
                    done_a = a.receive(part)?;
                }
            }
            if let (Some(_), Some(_)) = (&done_a, &done_b) {
                return Ok((done_a.unwrap(), done_b.unwrap()));
            }
            if to_a.is_empty() && to_b.is_empty() {
                break;
            }
        }
        Err(MseError::Closed)
    }

    #[test]
    fn negotiates_rc4_and_plaintext() {
        for (provide, allowed, method) in [
            (CryptoSet::ALL, CryptoSet::ALL, Method::Rc4),
            (CryptoSet::RC4, CryptoSet::ALL, Method::Rc4),
            (CryptoSet::ALL, CryptoSet::PLAINTEXT, Method::Plaintext),
            (CryptoSet::PLAINTEXT, CryptoSet::ALL, Method::Plaintext),
        ] {
            for chunk in [1, 7, 4096] {
                let a = Initiator::new(&SKEY, provide).unwrap();
                let b = Responder::new(allowed).unwrap();
                let (mut ea, mut eb) = run(a, b, chunk).unwrap();
                assert_eq!(ea.method(), method);
                assert_eq!(eb.established.method(), method);
                assert_eq!(eb.skey, SKEY);
                let mut msg = *b"payload";
                ea.encrypt(&mut msg);
                assert_eq!(msg != *b"payload", method == Method::Rc4);
                eb.established.decrypt(&mut msg);
                assert_eq!(&msg, b"payload");
                let mut back = *b"reply";
                eb.established.encrypt(&mut back);
                ea.decrypt(&mut back);
                assert_eq!(&back, b"reply");
            }
        }
    }

    #[test]
    fn disjoint_methods_fail() {
        let a = Initiator::new(&SKEY, CryptoSet::PLAINTEXT).unwrap();
        let b = Responder::new(CryptoSet::RC4).unwrap();
        assert!(matches!(run(a, b, 4096), Err(MseError::NoCommonMethod)));
    }

    #[test]
    fn unknown_torrents_fail() {
        let a = Initiator::new(&[1; 20], CryptoSet::ALL).unwrap();
        let b = Responder::new(CryptoSet::ALL).unwrap();
        assert!(matches!(run(a, b, 4096), Err(MseError::UnknownTorrent)));
    }

    #[test]
    fn empty_sets_are_refused() {
        assert!(matches!(
            Initiator::new(&SKEY, CryptoSet::NONE),
            Err(MseError::NoCommonMethod)
        ));
        assert!(matches!(
            Responder::new(CryptoSet::NONE),
            Err(MseError::NoCommonMethod)
        ));
    }

    #[test]
    fn missing_sync_is_detected() {
        let mut b = Responder::with_private_key(&[5; 20], &[], CryptoSet::ALL).unwrap();
        let a = KeyPair::from_private(&[6; 20]);
        assert!(b.receive(a.public(), lookup).unwrap().is_none());
        assert!(matches!(
            b.receive(&[0u8; MAX_PAD + 20], lookup),
            Err(MseError::NoSync)
        ));

        let mut a = Initiator::with_private_key(&[7; 20], &[], &SKEY, CryptoSet::ALL).unwrap();
        let peer = KeyPair::from_private(&[8; 20]);
        assert!(a.receive(peer.public()).unwrap().is_none());
        assert!(matches!(
            a.receive(&[0u8; MAX_PAD + 8]),
            Err(MseError::NoSync)
        ));
        assert!(matches!(a.receive(&[]), Err(MseError::NoSync)));
    }

    #[test]
    fn a_plain_reply_fails_fast() {
        let mut a = Initiator::new(&SKEY, CryptoSet::ALL).unwrap();
        assert!(a.receive(b"\x13BitTorrent").unwrap().is_none());
        assert!(matches!(
            a.receive(b" protocol\0\0\0\0\0\0\0\0"),
            Err(MseError::PlaintextPeer)
        ));
    }

    #[test]
    fn finished_handshakes_refuse_more_bytes() {
        let a = Initiator::new(&SKEY, CryptoSet::ALL).unwrap();
        let b = Responder::new(CryptoSet::ALL).unwrap();
        let mut a2 = Initiator::new(&SKEY, CryptoSet::ALL).unwrap();
        let mut b2 = Responder::new(CryptoSet::ALL).unwrap();
        // Drive a2/b2 by hand to keep the state machines.
        let mut done = (false, false);
        while !(done.0 && done.1) {
            let out = a2.take_output();
            if !done.1 && b2.receive(&out, lookup).unwrap().is_some() {
                done.1 = true;
            }
            let out = b2.take_output();
            if !done.0 && a2.receive(&out).unwrap().is_some() {
                done.0 = true;
            }
        }
        assert!(matches!(a2.receive(b"x"), Err(MseError::Finished)));
        assert!(matches!(b2.receive(b"x", lookup), Err(MseError::Finished)));
        drop(run(a, b, 4096).unwrap());
    }

    #[test]
    fn crypto_sets() {
        assert_eq!(CryptoSet::from_bits(0xffff_ffff), CryptoSet::ALL);
        assert_eq!(CryptoSet::ALL.preferred(), Some(Method::Rc4));
        assert_eq!(CryptoSet::PLAINTEXT.preferred(), Some(Method::Plaintext));
        assert_eq!(CryptoSet::NONE.preferred(), None);
        assert!(CryptoSet::ALL.contains(CryptoSet::RC4));
        assert!(!CryptoSet::PLAINTEXT.contains(CryptoSet::RC4));
    }

    proptest::proptest! {
        /// Arbitrary bytes after a valid key never panic either side.
        #[test]
        fn garbage_never_panics(data in proptest::collection::vec(proptest::num::u8::ANY, 0..2048)) {
            let peer = KeyPair::from_private(&[3; 20]);
            let mut b = Responder::with_private_key(&[4; 20], &[], CryptoSet::ALL).unwrap();
            let _ = b.receive(peer.public(), lookup);
            let _ = b.receive(&data, lookup);
            let mut a = Initiator::with_private_key(&[4; 20], &[], &SKEY, CryptoSet::ALL).unwrap();
            let _ = a.receive(peer.public());
            let _ = a.receive(&data);
            let mut raw = Responder::new(CryptoSet::ALL).unwrap();
            let _ = raw.receive(&data, lookup);
        }
    }
}
