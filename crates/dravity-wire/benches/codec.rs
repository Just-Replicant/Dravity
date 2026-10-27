//! Wire codec throughput: encoding and streaming decode of block messages.

#![allow(missing_docs, clippy::unwrap_used)]

use std::hint::black_box;

use bytes::{Bytes, BytesMut};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use dravity_wire::{BlockRequest, Message, MessageDecoder};

fn stream() -> BytesMut {
    let mut buf = BytesMut::new();
    let block = Bytes::from(vec![0xa5; 16 * 1024]);
    for i in 0..64 {
        Message::Piece {
            piece: i,
            offset: 0,
            data: block.clone(),
        }
        .encode(&mut buf)
        .unwrap();
        Message::Request(BlockRequest {
            piece: i,
            offset: 16 * 1024,
            length: 16 * 1024,
        })
        .encode(&mut buf)
        .unwrap();
        Message::Have { piece: i }.encode(&mut buf).unwrap();
    }
    buf
}

fn codec(c: &mut Criterion) {
    let encoded = stream();
    let mut group = c.benchmark_group("wire");
    group.throughput(Throughput::Bytes(encoded.len() as u64));
    group.bench_function("decode", |b| {
        b.iter(|| {
            let mut buf = encoded.clone();
            let mut dec = MessageDecoder::new();
            let mut n = 0;
            while let Some(m) = dec.decode(&mut buf).unwrap() {
                black_box(m);
                n += 1;
            }
            assert_eq!(n, 192);
        });
    });
    group.bench_function("encode", |b| {
        let block = Bytes::from(vec![0xa5; 16 * 1024]);
        let mut out = BytesMut::with_capacity(encoded.len());
        b.iter(|| {
            out.clear();
            for i in 0..64 {
                Message::Piece {
                    piece: i,
                    offset: 0,
                    data: block.clone(),
                }
                .encode(&mut out)
                .unwrap();
            }
            black_box(&out);
        });
    });
    group.finish();
}

criterion_group!(benches, codec);
criterion_main!(benches);
