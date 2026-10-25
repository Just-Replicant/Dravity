//! `.torrent` parsing must never panic, and accepted torrents must satisfy
//! the crate's safety invariants.
#![no_main]

use std::path::Component;

use dravity_core::{InfoHash, Metainfo};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(m) = Metainfo::from_bytes(data) else {
        return;
    };
    assert_eq!(m.info_hash(), InfoHash::from_info_bytes(m.info_bytes()));
    let info = m.info();
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
