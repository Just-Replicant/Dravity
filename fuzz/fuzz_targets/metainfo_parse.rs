//! `.torrent` parsing must never panic, and accepted torrents must satisfy
//! the crate's safety invariants.
#![no_main]

use std::path::Component;

use dravity_core::{InfoHash, InfoHashV2, Metainfo};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(m) = Metainfo::from_bytes(data) else {
        return;
    };
    match (m.info_hash_v1(), m.info_hash_v2()) {
        (Some(v1), _) => {
            assert_eq!(v1, InfoHash::from_info_bytes(m.info_bytes()));
            assert_eq!(m.info_hash(), v1);
        }
        (None, Some(v2)) => {
            assert_eq!(v2, InfoHashV2::from_info_bytes(m.info_bytes()));
            assert_eq!(m.info_hash(), v2.truncated());
        }
        (None, None) => panic!("a torrent without any info-hash"),
    }
    let info = m.info();
    // Verifying (wrong) data never panics, whatever the hashes look like.
    for p in 0..info.piece_count().min(4) {
        let len = info.layout().piece_size(p).expect("piece in range");
        if len <= 1 << 20 {
            let _ = info.check_piece(p, &vec![0; len as usize]);
        }
    }
    // Names are never decoded lossily: U+FFFD in a name must come from
    // its UTF-8 encoding in the input.
    let has_replacement = data.windows(3).any(|w| w == "\u{fffd}".as_bytes());
    if !has_replacement {
        assert!(!info.name().contains('\u{fffd}'), "lossy name");
        for f in info.files() {
            assert!(!f.path().to_string().contains('\u{fffd}'), "lossy path");
        }
    }
    for i in 0..info.files().len() {
        let path = info.file_path(i).expect("file index in range");
        assert!(
            path.components().all(|c| matches!(c, Component::Normal(_))),
            "unsafe path accepted: {path:?}"
        );
    }
    let covered: u64 = (0..info.piece_count())
        .flat_map(|p| info.piece_file_slices(p))
        .map(|s| s.length)
        .sum();
    assert_eq!(covered, info.total_length());
});
