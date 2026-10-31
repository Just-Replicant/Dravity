//! File storage throughput through each I/O backend: 16 KiB block writes and
//! reads (what peers send and request) and whole-piece verification reads.
//!
//! `cargo bench -p dravity-storage --features uring` adds the io_uring
//! backend when the kernel allows it.

#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::indexing_slicing
)]

use std::borrow::Cow;
use std::hint::black_box;
use std::sync::Arc;

use bytes::Bytes;
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use dravity_bencode::{Dict, Value};
use dravity_core::{BlockRef, Metainfo};
use dravity_storage::{
    BlockingIo, FileIo, FileStorage, Storage, read_block, verify_piece, write_block,
};
use sha1::{Digest, Sha1};

const PIECE: usize = 256 * 1024;
const BLOCK: u32 = 16 * 1024;
const PIECES: usize = 16;

fn torrent() -> (Metainfo, Vec<u8>) {
    let content: Vec<u8> = (0..PIECE * PIECES).map(|i| (i * 7 % 251) as u8).collect();
    let pieces: Vec<u8> = content
        .chunks(PIECE)
        .flat_map(|c| Sha1::digest(c).to_vec())
        .collect();
    let mut info = Dict::new();
    let mut put = |k: &str, v: Value<'static>| {
        info.insert(Cow::Owned(k.as_bytes().to_vec()), v);
    };
    put("length", Value::Int(content.len() as i64));
    put("name", Value::Bytes(Cow::Owned(b"bench.bin".to_vec())));
    put("piece length", Value::Int(PIECE as i64));
    put("pieces", Value::Bytes(Cow::Owned(pieces)));
    let mut top = Dict::new();
    top.insert(Cow::Borrowed(&b"info"[..]), Value::Dict(info));
    let encoded = Value::Dict(top).to_vec();
    (Metainfo::from_bytes(&encoded).unwrap(), content)
}

fn backends() -> Vec<Arc<dyn FileIo>> {
    let blocking: Arc<dyn FileIo> = Arc::new(BlockingIo);
    #[cfg(all(feature = "uring", target_os = "linux"))]
    if let Ok(uring) = dravity_storage::UringIo::start() {
        return vec![blocking, Arc::new(uring)];
    }
    vec![blocking]
}

fn blocks(content: &[u8]) -> Vec<(BlockRef, Bytes)> {
    let per_piece = PIECE as u32 / BLOCK;
    (0..PIECES as u32)
        .flat_map(|piece| (0..per_piece).map(move |b| (piece, b * BLOCK)))
        .map(|(piece, offset)| {
            let start = piece as usize * PIECE + offset as usize;
            let block = BlockRef {
                piece,
                offset,
                length: BLOCK,
            };
            (
                block,
                Bytes::copy_from_slice(&content[start..start + BLOCK as usize]),
            )
        })
        .collect()
}

fn file_io(c: &mut Criterion) {
    let (m, content) = torrent();
    let blocks = blocks(&content);
    let mut group = c.benchmark_group("storage");
    group.throughput(Throughput::Bytes(content.len() as u64));
    for io in backends() {
        let name = io.name();
        let dir = tempfile::tempdir().unwrap();
        let storage = FileStorage::new(dir.path(), m.info()).unwrap().with_io(io);
        let layout = m.info().layout();
        group.bench_function(format!("{name}/write_blocks"), |b| {
            b.iter(|| {
                for (block, data) in &blocks {
                    write_block(&storage, layout, *block, data).unwrap();
                }
            });
        });
        group.bench_function(format!("{name}/read_blocks"), |b| {
            b.iter(|| {
                for (block, _) in &blocks {
                    black_box(read_block(&storage, layout, *block).unwrap());
                }
            });
        });
        group.bench_function(format!("{name}/verify_pieces"), |b| {
            b.iter(|| {
                for piece in 0..PIECES as u32 {
                    assert!(verify_piece(&storage, m.info(), piece).unwrap());
                }
            });
        });
        storage.flush().unwrap();
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(20)
        .warm_up_time(std::time::Duration::from_secs(1))
        .measurement_time(std::time::Duration::from_secs(3));
    targets = file_io
}
criterion_main!(benches);
