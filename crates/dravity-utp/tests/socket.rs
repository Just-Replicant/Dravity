//! `UtpSocket` over loopback UDP.

#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation
)]

use std::time::Duration;

use dravity_utp::{Config, UtpSocket};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn pair() -> (UtpSocket, UtpSocket) {
    let a = UtpSocket::bind("127.0.0.1:0".parse().unwrap(), Config::default())
        .await
        .unwrap();
    let b = UtpSocket::bind("127.0.0.1:0".parse().unwrap(), Config::default())
        .await
        .unwrap();
    (a, b)
}

fn data(len: usize, salt: u8) -> Vec<u8> {
    (0..len).map(|i| (i as u8) ^ salt).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streams_both_ways_and_reads_eof() {
    let (client, server) = pair().await;
    let server_addr = server.local_addr();
    let up = data(2 << 20, 0x5a);
    let down = data(700_000, 0xa5);
    let (up2, down2) = (up.clone(), down.clone());
    let serving = tokio::spawn(async move {
        let (mut stream, from) = server.accept().await.unwrap();
        assert_eq!(from, stream.peer_addr());
        let (mut rd, mut wr) = tokio::io::split(stream);
        let writer = tokio::spawn(async move {
            wr.write_all(&down2).await.unwrap();
            wr.shutdown().await.unwrap();
            wr
        });
        let mut got = Vec::new();
        rd.read_to_end(&mut got).await.unwrap();
        assert!(got == up2, "server got {} bytes", got.len());
        stream = rd.unsplit(writer.await.unwrap());
        drop(stream);
        server
    });
    let mut stream = tokio::time::timeout(Duration::from_secs(5), client.connect(server_addr))
        .await
        .unwrap()
        .unwrap();
    let (mut rd, mut wr) = tokio::io::split(stream);
    let writer = tokio::spawn(async move {
        wr.write_all(&up).await.unwrap();
        wr.shutdown().await.unwrap();
        wr
    });
    let mut got = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), rd.read_to_end(&mut got))
        .await
        .unwrap()
        .unwrap();
    assert!(got == down, "client got {} bytes", got.len());
    stream = rd.unsplit(writer.await.unwrap());
    drop(stream);
    let _server = tokio::time::timeout(Duration::from_secs(30), serving)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn many_connections_share_a_socket() {
    let (client, server) = pair().await;
    let server_addr = server.local_addr();
    let echo = tokio::spawn(async move {
        for _ in 0..16 {
            let (mut stream, _) = server.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                loop {
                    let n = stream.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    stream.write_all(&buf[..n]).await.unwrap();
                }
                stream.shutdown().await.unwrap();
            });
        }
        server
    });
    let client = std::sync::Arc::new(client);
    let mut tasks = Vec::new();
    for i in 0..16u8 {
        let client = std::sync::Arc::clone(&client);
        tasks.push(tokio::spawn(async move {
            let mut stream = client.connect(server_addr).await.unwrap();
            let msg = data(10_000 + usize::from(i) * 997, i);
            stream.write_all(&msg).await.unwrap();
            let mut back = vec![0u8; msg.len()];
            stream.read_exact(&mut back).await.unwrap();
            assert_eq!(back, msg);
        }));
    }
    for t in tasks {
        tokio::time::timeout(Duration::from_secs(30), t)
            .await
            .unwrap()
            .unwrap();
    }
    let _server = echo.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connecting_to_a_closed_socket_is_refused_or_times_out() {
    let (_, server) = pair().await;
    let mut quick = Config::default();
    quick.syn_retries = 0;
    let client = UtpSocket::bind("127.0.0.1:0".parse().unwrap(), quick)
        .await
        .unwrap();
    let addr = server.local_addr();
    // Not accepting any more: SYNs are answered with a reset.
    drop(server);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let result = tokio::time::timeout(Duration::from_secs(15), client.connect(addr))
        .await
        .unwrap();
    let err = result.unwrap_err();
    assert!(
        matches!(
            err.kind(),
            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::TimedOut
        ),
        "{err:?}"
    );
}
