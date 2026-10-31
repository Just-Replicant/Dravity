//! Batched block I/O checks shared by the storage and io_uring tests.

use bytes::Bytes;
use dravity_core::{BlockRef, Metainfo};
use dravity_storage::{Storage, StorageError, read_blocks, verify_piece, write_blocks};

/// Every block of `m`, with its data from `content`, in a scrambled order.
fn all_blocks(m: &Metainfo, content: &Bytes) -> Vec<(BlockRef, Bytes)> {
    let layout = m.info().layout();
    let mut blocks: Vec<(BlockRef, Bytes)> = (0..layout.piece_count())
        .flat_map(|piece| layout.blocks(piece))
        .map(|b| {
            let start = (layout.piece_offset(b.piece).unwrap() + u64::from(b.offset)) as usize;
            (b, content.slice(start..start + b.length as usize))
        })
        .collect();
    // Deterministic shuffle: adjacent blocks are not adjacent in the batch.
    let n = blocks.len();
    for i in 0..n {
        blocks.swap(i, (i * 7 + 3) % n);
    }
    blocks
}

/// Writes and reads `content` through `storage` in batches: scrambled,
/// across file boundaries, in runs longer than one `writev`, and with bad
/// blocks and missing data.
pub(crate) fn batches_round_trip(storage: &dyn Storage, m: &Metainfo, content: &[u8]) {
    let layout = m.info().layout();
    let content = Bytes::copy_from_slice(content);
    let blocks = all_blocks(m, &content);

    // Nothing written yet: missing data (or, in memory, zeros).
    let refs: Vec<BlockRef> = blocks.iter().map(|(b, _)| *b).collect();
    let unwritten = |storage: &dyn Storage| match read_blocks(storage, layout, &refs[..1]) {
        Ok(data) => data[0] != blocks[0].1,
        Err(e) => e.is_missing_data(),
    };
    assert!(unwritten(storage));

    // A bad block refuses the whole batch before anything is written.
    let mut bad = blocks.clone();
    bad.push((
        BlockRef {
            piece: 0,
            offset: 0,
            length: 1,
        },
        // The wrong length for the block.
        Bytes::from_static(b"xy"),
    ));
    assert!(matches!(
        write_blocks(storage, layout, bad),
        Err(StorageError::InvalidBlock(_))
    ));
    assert!(unwritten(storage));

    write_blocks(storage, layout, blocks.clone()).unwrap();
    for piece in 0..m.info().piece_count() {
        assert!(
            verify_piece(storage, m.info(), piece).unwrap(),
            "piece {piece}"
        );
    }
    let back = read_blocks(storage, layout, &refs).unwrap();
    for ((_, want), got) in blocks.iter().zip(&back) {
        assert_eq!(want, got);
    }

    // Many small adjacent writes (more than one `writev` holds), zeros
    // then the content again, and reads that cross file boundaries.
    // (Padding is not stored: it reads as zeros, which `content` has.)
    let step = 97;
    let chunks = |fill: Option<u8>| -> Vec<(u64, Bytes)> {
        (0..content.len())
            .step_by(step)
            .map(|at| {
                let end = (at + step).min(content.len());
                let data = match fill {
                    Some(b) => Bytes::from(vec![b; end - at]),
                    None => content.slice(at..end),
                };
                (at as u64, data)
            })
            .rev()
            .collect()
    };
    let reads: Vec<(u64, usize)> = (0..content.len())
        .step_by(1_000)
        .map(|at| (at as u64, 1_000.min(content.len() - at)))
        .collect();
    storage.write_many(chunks(Some(0))).unwrap();
    for data in storage.read_many(&reads).unwrap() {
        assert!(data.iter().all(|&b| b == 0));
    }
    storage.write_many(chunks(None)).unwrap();
    let got = storage.read_many(&reads).unwrap();
    for (&(at, len), data) in reads.iter().zip(&got) {
        assert_eq!(
            data[..],
            content[at as usize..at as usize + len],
            "read at {at}"
        );
    }
    assert!(storage.read_many(&[]).unwrap().is_empty());
    storage.write_many(Vec::new()).unwrap();
}
