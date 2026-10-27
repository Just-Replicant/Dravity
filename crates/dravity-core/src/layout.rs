//! Piece, block and file arithmetic.
//!
//! A torrent's content is one contiguous byte stream (all files concatenated in
//! metainfo order) split into fixed-size pieces (the last may be shorter).
//! Pieces are transferred in blocks of [`BLOCK_SIZE`] bytes.

use std::ops::Range;

/// Standard request block size (16 KiB). Peers may drop larger requests.
pub const BLOCK_SIZE: u32 = 16 * 1024;

/// Errors constructing a layout.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LayoutError {
    /// Piece length was zero.
    #[error("piece length must be greater than zero")]
    ZeroPieceLength,
    /// More pieces than fit in a `u32` piece index.
    #[error("too many pieces for a u32 index")]
    TooManyPieces,
    /// Summing file lengths overflowed `u64`.
    #[error("total length overflows u64")]
    LengthOverflow,
}

/// A block within a piece: what a `request`/`cancel` message addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockRef {
    /// Piece index.
    pub piece: u32,
    /// Byte offset within the piece.
    pub offset: u32,
    /// Length in bytes (at most [`BLOCK_SIZE`]).
    pub length: u32,
}

/// Piece geometry of a torrent.
///
/// ```
/// use dravity_core::layout::PieceLayout;
///
/// let l = PieceLayout::new(32 * 1024, 70_000).unwrap();
/// assert_eq!(l.piece_count(), 3);
/// assert_eq!(l.piece_size(2), Some(70_000 - 2 * 32 * 1024));
/// assert_eq!(l.block_count(0), Some(2));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PieceLayout {
    piece_length: u32,
    total_length: u64,
    piece_count: u32,
}

impl PieceLayout {
    /// Creates a layout.
    ///
    /// # Errors
    /// Fails for a zero piece length or more than `u32::MAX` pieces.
    /// A total length of zero is an empty torrent and has [`piece_count`](Self::piece_count) 0.
    pub fn new(piece_length: u32, total_length: u64) -> Result<Self, LayoutError> {
        if piece_length == 0 {
            return Err(LayoutError::ZeroPieceLength);
        }
        let count = total_length.div_ceil(u64::from(piece_length));
        let piece_count = u32::try_from(count).map_err(|_| LayoutError::TooManyPieces)?;
        Ok(Self {
            piece_length,
            total_length,
            piece_count,
        })
    }

    /// Nominal piece length.
    #[must_use]
    pub const fn piece_length(&self) -> u32 {
        self.piece_length
    }

    /// Total content length in bytes.
    #[must_use]
    pub const fn total_length(&self) -> u64 {
        self.total_length
    }

    /// Number of pieces.
    #[must_use]
    pub const fn piece_count(&self) -> u32 {
        self.piece_count
    }

    /// Byte offset of piece `index` in the content stream.
    #[must_use]
    pub fn piece_offset(&self, index: u32) -> Option<u64> {
        (index < self.piece_count).then(|| u64::from(index) * u64::from(self.piece_length))
    }

    /// Actual size of piece `index` (the last piece may be shorter).
    #[must_use]
    pub fn piece_size(&self, index: u32) -> Option<u32> {
        let start = self.piece_offset(index)?;
        let remaining = self.total_length - start;
        // `remaining` > 0 for valid indexes and the min fits in u32.
        u32::try_from(remaining.min(u64::from(self.piece_length))).ok()
    }

    /// Byte range of piece `index` in the content stream.
    #[must_use]
    pub fn piece_range(&self, index: u32) -> Option<Range<u64>> {
        let start = self.piece_offset(index)?;
        Some(start..start + u64::from(self.piece_size(index)?))
    }

    /// Piece containing content byte `offset`.
    #[must_use]
    pub fn piece_at(&self, offset: u64) -> Option<u32> {
        if offset >= self.total_length {
            return None;
        }
        u32::try_from(offset / u64::from(self.piece_length)).ok()
    }

    /// Number of [`BLOCK_SIZE`] blocks in piece `index`.
    #[must_use]
    pub fn block_count(&self, index: u32) -> Option<u32> {
        Some(self.piece_size(index)?.div_ceil(BLOCK_SIZE))
    }

    /// Block `block` of piece `index`.
    #[must_use]
    pub fn block(&self, index: u32, block: u32) -> Option<BlockRef> {
        let size = self.piece_size(index)?;
        let offset = block.checked_mul(BLOCK_SIZE).filter(|&o| o < size)?;
        Some(BlockRef {
            piece: index,
            offset,
            length: (size - offset).min(BLOCK_SIZE),
        })
    }

    /// All blocks of piece `index`, in order (empty for an invalid index).
    pub fn blocks(&self, index: u32) -> impl Iterator<Item = BlockRef> + '_ {
        let n = self.block_count(index).unwrap_or(0);
        (0..n).filter_map(move |b| self.block(index, b))
    }

    /// Whether `block` is a valid, in-bounds request for this torrent.
    #[must_use]
    pub fn is_valid_request(&self, block: BlockRef) -> bool {
        self.piece_size(block.piece).is_some_and(|size| {
            block.length > 0
                && block.length <= BLOCK_SIZE
                && u64::from(block.offset) + u64::from(block.length) <= u64::from(size)
        })
    }
}

/// A contiguous part of one file covered by a byte range of the content stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSlice {
    /// Index into the torrent's file list.
    pub file_index: usize,
    /// Offset within that file.
    pub file_offset: u64,
    /// Number of bytes.
    pub length: u64,
}

/// Maps content-stream ranges onto files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileLayout {
    /// `(start offset, length)` per file.
    spans: Vec<(u64, u64)>,
    total: u64,
}

impl FileLayout {
    /// Builds the layout from file lengths in metainfo order.
    ///
    /// # Errors
    /// Returns [`LayoutError::LengthOverflow`] if the total exceeds `u64`.
    pub fn new(lengths: impl IntoIterator<Item = u64>) -> Result<Self, LayoutError> {
        let mut spans = Vec::new();
        let mut total: u64 = 0;
        for len in lengths {
            spans.push((total, len));
            total = total.checked_add(len).ok_or(LayoutError::LengthOverflow)?;
        }
        Ok(Self { spans, total })
    }

    /// Total length of all files.
    #[must_use]
    pub const fn total_length(&self) -> u64 {
        self.total
    }

    /// Start offset of file `index` in the content stream.
    #[must_use]
    pub fn file_offset(&self, index: usize) -> Option<u64> {
        self.spans.get(index).map(|s| s.0)
    }

    /// The file slices covering `range` (clamped to the content), skipping
    /// zero-length files.
    pub fn slices(&self, range: Range<u64>) -> impl Iterator<Item = FileSlice> + '_ {
        let end = range.end.min(self.total);
        let start = range.start.min(end);
        // First file whose end is beyond `start`.
        let first = self.spans.partition_point(|&(off, len)| off + len <= start);
        self.spans
            .iter()
            .enumerate()
            .skip(first)
            .take_while(move |(_, (off, _))| *off < end)
            .filter(|(_, (_, len))| *len > 0)
            .map(move |(file_index, &(off, len))| {
                let s = start.max(off);
                let e = end.min(off + len);
                FileSlice {
                    file_index,
                    file_offset: s - off,
                    length: e - s,
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn rejects_bad_geometry() {
        assert_eq!(PieceLayout::new(0, 10), Err(LayoutError::ZeroPieceLength));
        assert_eq!(
            PieceLayout::new(1, u64::MAX),
            Err(LayoutError::TooManyPieces)
        );
        assert_eq!(
            FileLayout::new([u64::MAX, 1]),
            Err(LayoutError::LengthOverflow)
        );
    }

    #[test]
    fn exact_multiple() {
        let l = PieceLayout::new(BLOCK_SIZE * 2, u64::from(BLOCK_SIZE) * 4).unwrap();
        assert_eq!(l.piece_count(), 2);
        assert_eq!(l.piece_size(1), Some(BLOCK_SIZE * 2));
        assert_eq!(l.piece_size(2), None);
        assert_eq!(l.blocks(1).count(), 2);
        assert_eq!(l.piece_at(u64::from(BLOCK_SIZE) * 4), None);
    }

    #[test]
    fn short_last_piece_and_block() {
        let l = PieceLayout::new(BLOCK_SIZE * 2, u64::from(BLOCK_SIZE) * 3 + 5).unwrap();
        assert_eq!(l.piece_count(), 2);
        assert_eq!(l.piece_size(1), Some(BLOCK_SIZE + 5));
        let blocks: Vec<_> = l.blocks(1).collect();
        assert_eq!(
            blocks,
            [
                BlockRef {
                    piece: 1,
                    offset: 0,
                    length: BLOCK_SIZE
                },
                BlockRef {
                    piece: 1,
                    offset: BLOCK_SIZE,
                    length: 5
                },
            ]
        );
        assert!(l.is_valid_request(blocks[1]));
        assert!(!l.is_valid_request(BlockRef {
            piece: 1,
            offset: BLOCK_SIZE,
            length: 6
        }));
        assert!(!l.is_valid_request(BlockRef {
            piece: 1,
            offset: 0,
            length: 0
        }));
    }

    #[test]
    fn file_slices_span_boundaries() {
        // Files: a=5, (empty)=0, b=10, c=3 -> total 18; pieces of 8.
        let f = FileLayout::new([5, 0, 10, 3]).unwrap();
        let l = PieceLayout::new(8, f.total_length()).unwrap();
        let p0: Vec<_> = f.slices(l.piece_range(0).unwrap()).collect();
        assert_eq!(
            p0,
            [
                FileSlice {
                    file_index: 0,
                    file_offset: 0,
                    length: 5
                },
                FileSlice {
                    file_index: 2,
                    file_offset: 0,
                    length: 3
                },
            ]
        );
        let p1: Vec<_> = f.slices(l.piece_range(1).unwrap()).collect();
        assert_eq!(
            p1,
            [
                FileSlice {
                    file_index: 2,
                    file_offset: 3,
                    length: 7
                },
                FileSlice {
                    file_index: 3,
                    file_offset: 0,
                    length: 1
                },
            ]
        );
        let p2: Vec<_> = f.slices(l.piece_range(2).unwrap()).collect();
        assert_eq!(
            p2,
            [FileSlice {
                file_index: 3,
                file_offset: 1,
                length: 2
            }]
        );
        assert_eq!(f.slices(100..200).count(), 0);
    }

    proptest! {
        #[test]
        fn blocks_tile_every_piece(piece_len in 1u32..200_000, total in 1u64..2_000_000) {
            let l = PieceLayout::new(piece_len, total).unwrap();
            let mut sum = 0u64;
            for p in 0..l.piece_count() {
                let mut expected = 0u32;
                for b in l.blocks(p) {
                    prop_assert_eq!(b.offset, expected);
                    prop_assert!(b.length > 0 && b.length <= BLOCK_SIZE);
                    prop_assert!(l.is_valid_request(b));
                    expected += b.length;
                }
                prop_assert_eq!(Some(expected), l.piece_size(p));
                sum += u64::from(expected);
            }
            prop_assert_eq!(sum, total);
        }

        #[test]
        fn slices_tile_ranges(lengths in proptest::collection::vec(0u64..50, 1..10), piece_len in 1u32..64) {
            let f = FileLayout::new(lengths.iter().copied()).unwrap();
            prop_assume!(f.total_length() > 0);
            let l = PieceLayout::new(piece_len, f.total_length()).unwrap();
            for p in 0..l.piece_count() {
                let r = l.piece_range(p).unwrap();
                let covered: u64 = f.slices(r.clone()).map(|s| s.length).sum();
                prop_assert_eq!(covered, r.end - r.start);
                for s in f.slices(r) {
                    prop_assert!(s.file_offset + s.length <= lengths[s.file_index]);
                }
            }
        }
    }
}
