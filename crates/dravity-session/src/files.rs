//! File priorities and per-file progress.
//!
//! Users choose priorities per file; the engine works per piece. A piece
//! gets the highest priority of the files it overlaps, so a piece shared by
//! a wanted and a skipped file is still downloaded (the wanted file needs
//! it). BEP 47 padding files never count.

use std::io;
use std::path::{Path, PathBuf};

use dravity_core::Info;
use dravity_engine::{Bitfield, Priority};

/// One file of a torrent, with its priority and download progress.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FileProgress {
    /// Index in the torrent's file list.
    pub index: usize,
    /// Path relative to the download directory (including the torrent's
    /// directory for multi-file torrents).
    pub path: PathBuf,
    /// Length in bytes.
    pub length: u64,
    /// Bytes of the file in verified pieces.
    pub have: u64,
    /// Download priority.
    pub priority: Priority,
    /// A BEP 47 padding file (never written to disk).
    pub padding: bool,
}

/// `priorities` normalized for `info`: one entry per file, [`Priority::Normal`]
/// when empty. `None` when the count does not match.
pub(crate) fn normalize(info: &Info, priorities: &[Priority]) -> Option<Vec<Priority>> {
    let count = info.files().len();
    if priorities.is_empty() {
        return Some(vec![Priority::Normal; count]);
    }
    (priorities.len() == count).then(|| priorities.to_vec())
}

/// Piece priorities from file priorities (one per file): the highest
/// priority among the non-padding files each piece overlaps, or
/// [`Priority::Skip`] for a piece of padding only.
pub(crate) fn piece_priorities(info: &Info, files: &[Priority]) -> Vec<Priority> {
    let entries = info.files();
    (0..info.piece_count())
        .map(|piece| {
            info.piece_file_slices(piece)
                .filter(|slice| {
                    entries
                        .get(slice.file_index)
                        .is_some_and(|f| !f.is_padding())
                })
                .filter_map(|slice| files.get(slice.file_index).copied())
                .max()
                .unwrap_or(Priority::Skip)
        })
        .collect()
}

/// Per-file progress: bytes of each file covered by pieces in `have`.
pub(crate) fn progress(info: &Info, have: &Bitfield, files: &[Priority]) -> Vec<FileProgress> {
    let mut done = vec![0u64; info.files().len()];
    for piece in have.ones() {
        for slice in info.piece_file_slices(piece) {
            if let Some(d) = done.get_mut(slice.file_index) {
                *d = d.saturating_add(slice.length);
            }
        }
    }
    info.files()
        .iter()
        .enumerate()
        .map(|(index, file)| FileProgress {
            index,
            path: info.file_path(index).unwrap_or_default(),
            length: file.length(),
            have: done.get(index).copied().unwrap_or(0),
            priority: files.get(index).copied().unwrap_or_default(),
            padding: file.is_padding(),
        })
        .collect()
}

/// Pieces that hold bytes `start..end` of the content stream.
pub(crate) fn pieces_for(info: &Info, start: u64, end: u64) -> std::ops::Range<u32> {
    let layout = info.layout();
    if start >= end {
        return 0..0;
    }
    let first = layout.piece_at(start).unwrap_or(layout.piece_count());
    let last = layout
        .piece_at(end - 1)
        .map_or(layout.piece_count(), |p| p + 1);
    first..last
}

/// Deletes the files of `info` below `dir`, then the directories they
/// leave empty. Paths whose directories below `dir` are symbolic links are
/// left alone, so a swapped-in link cannot redirect the deletion. Missing
/// files are fine.
pub(crate) fn delete_content(dir: &Path, info: &Info) -> io::Result<()> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut first_error = None;
    for (index, file) in info.files().iter().enumerate() {
        let Some(relative) = info.file_path(index) else {
            continue;
        };
        if file.is_padding() || !below_without_links(dir, &relative) {
            continue;
        }
        let path = dir.join(&relative);
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                first_error.get_or_insert(e);
            }
            _ => {}
        }
        let mut parent = relative.parent();
        while let Some(p) = parent.filter(|p| !p.as_os_str().is_empty()) {
            let full = dir.join(p);
            if !dirs.contains(&full) {
                dirs.push(full);
            }
            parent = p.parent();
        }
    }
    // Deepest first; a directory still holding other files stays.
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for d in dirs {
        let _ = std::fs::remove_dir(d);
    }
    first_error.map_or(Ok(()), Err)
}

/// Whether no directory between `dir` and `dir/relative` is a symbolic
/// link (the file itself is removed, not followed, either way).
fn below_without_links(dir: &Path, relative: &Path) -> bool {
    let mut current = dir.to_path_buf();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            return true;
        }
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => return false,
            _ => {}
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use dravity_bencode::{Dict, Value};
    use dravity_core::Metainfo;
    use dravity_engine::{Bitfield, Priority};

    use super::{delete_content, normalize, piece_priorities, pieces_for, progress};

    fn torrent(files: &[(u64, bool)], piece_len: u64) -> Metainfo {
        let b = |s: &[u8]| Value::Bytes(Cow::Owned(s.to_vec()));
        let key = |s: &str| Cow::Owned(s.as_bytes().to_vec());
        let total: u64 = files.iter().map(|f| f.0).sum();
        let pieces = total.div_ceil(piece_len);
        let list = files
            .iter()
            .enumerate()
            .map(|(i, &(len, pad))| {
                let mut d = Dict::new();
                d.insert(key("length"), Value::Int(i64::try_from(len).unwrap()));
                d.insert(
                    key("path"),
                    Value::List(vec![b(format!("f{i}").as_bytes())]),
                );
                if pad {
                    d.insert(key("attr"), b(b"p"));
                }
                Value::Dict(d)
            })
            .collect();
        let mut info = Dict::new();
        info.insert(key("name"), b(b"t"));
        info.insert(key("files"), Value::List(list));
        info.insert(
            key("piece length"),
            Value::Int(i64::try_from(piece_len).unwrap()),
        );
        info.insert(
            key("pieces"),
            b(&vec![0u8; usize::try_from(pieces).unwrap() * 20]),
        );
        let mut top = Dict::new();
        top.insert(key("info"), Value::Dict(info));
        Metainfo::from_bytes(&Value::Dict(top).to_vec()).unwrap()
    }

    #[test]
    fn a_shared_piece_takes_the_highest_priority() {
        use Priority::{High, Low, Normal, Skip};
        // Pieces of 16 bytes; files of 20, 12 (padding) and 30 bytes.
        let t = torrent(&[(20, false), (12, true), (30, false)], 16);
        let info = t.info();
        assert_eq!(
            piece_priorities(info, &[Skip, High, Low]),
            vec![Skip, Skip, Low, Low],
            "padding never counts"
        );
        assert_eq!(
            piece_priorities(info, &[Normal, Skip, Skip]),
            vec![Normal, Normal, Skip, Skip]
        );
        assert_eq!(normalize(info, &[]), Some(vec![Normal; 3]));
        assert_eq!(normalize(info, &[High]), None);
    }

    #[test]
    fn progress_counts_bytes_in_verified_pieces() {
        let t = torrent(&[(20, false), (30, false)], 16);
        let info = t.info();
        let mut have = Bitfield::new(info.piece_count());
        have.set(1);
        have.set(3);
        let files = progress(info, &have, &[Priority::Normal, Priority::Skip]);
        // Piece 1 is bytes 16..32: 4 of f0, 12 of f1. Piece 3 is 48..50.
        assert_eq!(files[0].have, 4);
        assert_eq!(files[1].have, 14);
        assert_eq!(files[1].priority, Priority::Skip);
        assert_eq!(files[1].path, std::path::PathBuf::from("t/f1"));
    }

    #[test]
    fn byte_ranges_map_to_pieces() {
        let t = torrent(&[(20, false), (30, false)], 16);
        let info = t.info();
        assert_eq!(pieces_for(info, 20, 50), 1..4);
        assert_eq!(pieces_for(info, 0, 16), 0..1);
        assert_eq!(pieces_for(info, 5, 5), 0..0);
    }

    #[test]
    fn deleting_content_removes_files_and_empty_directories() {
        let t = torrent(&[(20, false), (30, false)], 16);
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("t")).unwrap();
        std::fs::write(dir.path().join("t/f0"), b"x").unwrap();
        // f1 was never written; other files in the directory survive.
        delete_content(dir.path(), t.info()).unwrap();
        assert!(!dir.path().join("t").exists());
        std::fs::create_dir_all(dir.path().join("t")).unwrap();
        std::fs::write(dir.path().join("t/f0"), b"x").unwrap();
        std::fs::write(dir.path().join("t/mine.txt"), b"keep").unwrap();
        delete_content(dir.path(), t.info()).unwrap();
        assert!(!dir.path().join("t/f0").exists());
        assert!(dir.path().join("t/mine.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn deleting_content_does_not_follow_directory_links() {
        let t = torrent(&[(20, false), (30, false)], 16);
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::write(elsewhere.path().join("f0"), b"precious").unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), dir.path().join("t")).unwrap();
        delete_content(dir.path(), t.info()).unwrap();
        assert!(elsewhere.path().join("f0").exists());
    }
}
