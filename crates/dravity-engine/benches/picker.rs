//! Picker cost: picking every block of a large torrent from many peers.

#![allow(missing_docs, clippy::unwrap_used)]

use std::hint::black_box;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use dravity_core::PieceLayout;
use dravity_engine::{Bitfield, Picker};

const PIECES: u32 = 4096;

fn peers() -> Vec<Bitfield> {
    (0..50u32)
        .map(|p| {
            let mut b = Bitfield::new(PIECES);
            for i in 0..PIECES {
                if (i.wrapping_mul(2_654_435_761) ^ p) % 3 != 0 {
                    b.set(i);
                }
            }
            b
        })
        .collect()
}

fn picker(c: &mut Criterion) {
    let layout = PieceLayout::new(256 * 1024, u64::from(PIECES) * 256 * 1024).unwrap();
    let peers = peers();
    c.bench_function("picker/add_50_peers", |b| {
        b.iter_batched(
            || Picker::new(layout, Bitfield::new(PIECES)),
            |mut picker| {
                for p in &peers {
                    picker.add_peer_pieces(p);
                }
                picker
            },
            BatchSize::SmallInput,
        );
    });
    c.bench_function("picker/pick_1000_blocks", |b| {
        b.iter_batched(
            || {
                let mut picker = Picker::new(layout, Bitfield::new(PIECES));
                for p in &peers {
                    picker.add_peer_pieces(p);
                }
                picker
            },
            |mut picker| {
                for peer in peers.iter().cycle().take(1000) {
                    black_box(picker.pick(peer));
                }
                picker
            },
            BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, picker);
criterion_main!(benches);
