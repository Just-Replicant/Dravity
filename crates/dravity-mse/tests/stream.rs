//! The async handshake and `MseStream` over in-memory connections.

#![cfg(feature = "stream")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use dravity_mse::{
    CryptoSet, Method, MseError, MseStream, initiate, is_plaintext_handshake, req2_hash, respond,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

const SKEY: [u8; 20] = [0x42; 20];

fn lookup(hash: &[u8; 20]) -> Option<[u8; 20]> {
    (*hash == req2_hash(&SKEY)).then_some(SKEY)
}

async fn pair(
    provide: CryptoSet,
    allowed: CryptoSet,
) -> Result<(MseStream<DuplexStream>, MseStream<DuplexStream>), MseError> {
    // A small pipe forces partial reads and writes.
    let (a, mut b) = tokio::io::duplex(64);
    let server = tokio::spawn(async move {
        // Read the first bytes as a listener does to tell MSE from plain.
        let mut first = vec![0u8; 20];
        b.read_exact(&mut first).await?;
        assert_eq!(is_plaintext_handshake(&first), Some(false));
        respond(b, &first, allowed, lookup).await
    });
    let a = initiate(a, &SKEY, provide).await;
    let b = server.await.expect("task");
    let (b, skey) = b?;
    assert_eq!(skey, SKEY);
    Ok((a?, b))
}

#[tokio::test]
async fn payload_flows_both_ways() {
    for (provide, allowed, method) in [
        (CryptoSet::ALL, CryptoSet::ALL, Method::Rc4),
        (CryptoSet::ALL, CryptoSet::PLAINTEXT, Method::Plaintext),
    ] {
        let (mut a, mut b) = pair(provide, allowed).await.unwrap();
        assert_eq!(a.method(), Some(method));
        assert_eq!(b.method(), Some(method));
        assert_eq!(a.is_encrypted(), method == Method::Rc4);
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let sent = data.clone();
        let writer = tokio::spawn(async move {
            a.write_all(&sent).await.unwrap();
            a.flush().await.unwrap();
            let mut back = vec![0u8; 5];
            a.read_exact(&mut back).await.unwrap();
            back
        });
        let mut got = vec![0u8; data.len()];
        b.read_exact(&mut got).await.unwrap();
        assert_eq!(got, data);
        b.write_all(b"thanks").await.unwrap();
        b.flush().await.unwrap();
        assert_eq!(writer.await.unwrap(), b"thank");
    }
}

#[tokio::test]
async fn mismatched_policies_fail() {
    let err = pair(CryptoSet::PLAINTEXT, CryptoSet::RC4)
        .await
        .unwrap_err();
    assert!(
        matches!(err, MseError::NoCommonMethod | MseError::Closed),
        "{err}"
    );
}

#[tokio::test]
async fn a_peer_without_mse_closes_the_initiator() {
    let (a, mut b) = tokio::io::duplex(1024);
    let peer = tokio::spawn(async move {
        let mut hs = [0u8; 68];
        b.read_exact(&mut hs).await.unwrap();
        // Not "\x13BitTorrent protocol": a plain client hangs up.
        assert_eq!(is_plaintext_handshake(&hs), Some(false));
        drop(b);
    });
    let err = initiate(a, &SKEY, CryptoSet::ALL).await.unwrap_err();
    assert!(matches!(err, MseError::Closed | MseError::Io(_)), "{err}");
    peer.await.unwrap();
}

#[tokio::test]
async fn plain_streams_pass_through_with_a_prefix() {
    let (a, mut b) = tokio::io::duplex(64);
    let mut a = MseStream::plain(a).with_prefix(b"early".to_vec());
    assert_eq!(a.method(), None);
    b.write_all(b" late").await.unwrap();
    let mut got = [0u8; 10];
    a.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"early late");
    a.write_all(b"out").await.unwrap();
    let mut out = [0u8; 3];
    b.read_exact(&mut out).await.unwrap();
    assert_eq!(&out, b"out");
}
