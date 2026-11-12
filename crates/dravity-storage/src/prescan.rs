//! Verify-before-download prescan: which pieces cannot be on disk.
//!
//! A full check hashes every piece. Before it runs, one `stat` per file
//! tells which byte ranges are certainly missing: a file that does not
//! exist, or is shorter than the torrent says, cannot hold a valid piece
//! that touches the missing part. [`absent_pieces`] lists those pieces so
//! the check can skip them. A partial or new download is then checked in
//! O(files) plus the pieces that might be present, instead of O(data).

use dravity_core::Info;

use crate::FileStamp;

/// Pieces of `info` that touch bytes no file on disk can hold, given one
/// [`FileStamp`] per file (from [`Storage::stamps`](crate::Storage::stamps)).
/// Padding files read as zeros and are never missing. Returns an empty
/// list when `stamps` does not match the files.
#[must_use]
pub fn absent_pieces(info: &Info, stamps: &[FileStamp]) -> Vec<u32> {
    let files = info.files();
    if stamps.len() != files.len() {
        return Vec::new();
    }
    let layout = info.layout();
    let mut absent: Vec<u32> = Vec::new();
    for (file, stamp) in files.iter().zip(stamps) {
        if file.is_padding() || file.length() == 0 {
            continue;
        }
        let on_disk = match stamp {
            FileStamp::Missing => 0,
            FileStamp::Present { len, .. } => *len,
        };
        if on_disk >= file.length() {
            continue;
        }
        let start = file.offset() + on_disk;
        let end = file.offset() + file.length() - 1;
        let (Some(first), Some(last)) = (layout.piece_at(start), layout.piece_at(end)) else {
            continue;
        };
        // Files are in stream order, so the list stays sorted; skip a
        // piece already listed for the previous file.
        let from = match absent.last() {
            Some(&prev) if prev >= first => prev + 1,
            _ => first,
        };
        absent.extend(from..=last);
    }
    absent
}

#[cfg(test)]
mod tests {
    use dravity_core::Metainfo;

    use super::*;

    /// Three files of 20 000, 30 000 and 14 000 bytes, 16 KiB pieces.
    fn info() -> Metainfo {
        let pieces = vec![0u8; 20 * 4];
        let mut info = b"d5:filesl".to_vec();
        for (len, name) in [(20_000, "a"), (30_000, "b"), (14_000, "c")] {
            info.extend_from_slice(format!("d6:lengthi{len}e4:pathl1:{name}ee").as_bytes());
        }
        info.extend_from_slice(b"e4:name1:t12:piece lengthi16384e6:pieces80:");
        info.extend_from_slice(&pieces);
        info.push(b'e');
        Metainfo::from_info_bytes(&info, Vec::new()).unwrap()
    }

    fn present(len: u64) -> FileStamp {
        FileStamp::Present { len, mtime_ns: 1 }
    }

    #[test]
    fn complete_files_need_a_full_check() {
        let m = info();
        let stamps = [present(20_000), present(30_000), present(14_000)];
        assert!(absent_pieces(m.info(), &stamps).is_empty());
    }

    #[test]
    fn missing_and_short_files_rule_out_their_pieces() {
        let m = info();
        // Nothing on disk: every piece.
        let none = [FileStamp::Missing; 3];
        assert_eq!(absent_pieces(m.info(), &none), [0, 1, 2, 3]);
        // b.bin (bytes 20 000..50 000) is 10 000 bytes short: 40 000..50 000
        // is missing, which is in piece 2 (32 768..49 152) and piece 3.
        let short = [present(20_000), present(20_000), present(14_000)];
        assert_eq!(absent_pieces(m.info(), &short), [2, 3]);
        // A missing first file: pieces 0 and 1 (byte 19 999 is in piece 1).
        let first = [FileStamp::Missing, present(30_000), present(14_000)];
        assert_eq!(absent_pieces(m.info(), &first), [0, 1]);
        // Wrong count: no claim.
        assert!(absent_pieces(m.info(), &none[..2]).is_empty());
    }
}
