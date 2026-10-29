//! Reading a file while it downloads (streaming with seek).
//!
//! A [`FileStream`] reads one file of a torrent at any position. Reading a
//! piece that is not verified yet waits for it, and marks the pieces from
//! the read position on as urgent so the engine fetches them first, in
//! order ([`dravity_engine::Event::SetUrgent`]). Seeking moves that window.
//! Several streams can be open at once; their windows are combined.

use std::io::{self, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use dravity_core::Metainfo;
use dravity_storage::Storage;
use tokio::sync::{mpsc, oneshot};

use crate::files::pieces_for;
use crate::torrent::Command;

/// Pieces ahead of the read position marked urgent by default.
pub const DEFAULT_READAHEAD: u32 = 8;

/// Largest chunk [`FileStream::read_chunk`] returns.
const MAX_CHUNK: usize = 256 * 1024;

static NEXT_STREAM: AtomicU64 = AtomicU64::new(1);

/// Reads one file of a torrent, waiting for pieces as needed. Created by
/// [`TorrentHandle::open_file`](crate::TorrentHandle::open_file).
///
/// Dropping the stream releases its urgent pieces.
#[derive(Debug)]
pub struct FileStream {
    id: u64,
    commands: mpsc::Sender<Command>,
    metainfo: Arc<Metainfo>,
    storage: Arc<dyn Storage>,
    /// Content-stream offset of the file's first byte.
    start: u64,
    len: u64,
    pos: u64,
    readahead: u32,
    /// First piece of the window last sent to the torrent.
    window: Option<u32>,
}

impl FileStream {
    pub(crate) fn new(
        commands: mpsc::Sender<Command>,
        metainfo: Arc<Metainfo>,
        storage: Arc<dyn Storage>,
        start: u64,
        len: u64,
    ) -> Self {
        Self {
            id: NEXT_STREAM.fetch_add(1, Ordering::Relaxed),
            commands,
            metainfo,
            storage,
            start,
            len,
            pos: 0,
            readahead: DEFAULT_READAHEAD,
            window: None,
        }
    }

    /// Length of the file in bytes.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.len
    }

    /// Whether the file is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Current read position.
    #[must_use]
    pub const fn position(&self) -> u64 {
        self.pos
    }

    /// Pieces marked urgent ahead of the read position (at least one).
    pub fn set_readahead(&mut self, pieces: u32) {
        self.readahead = pieces.max(1);
        self.window = None;
    }

    /// Moves the read position. Positions past the end are allowed (reads
    /// there return nothing).
    ///
    /// # Errors
    /// [`io::ErrorKind::InvalidInput`] for a position before the start or
    /// an overflow.
    pub fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        let target = target.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "seek before start or overflow")
        })?;
        self.pos = target;
        Ok(target)
    }

    /// Reads up to `max` bytes (at most one piece, at most 256 KiB) at the
    /// current position and advances it; an empty result is the end of the
    /// file. Waits until the piece is downloaded and verified.
    ///
    /// # Errors
    /// [`io::ErrorKind::BrokenPipe`] when the torrent stops; storage errors.
    pub async fn read_chunk(&mut self, max: usize) -> io::Result<Bytes> {
        if self.pos >= self.len || max == 0 {
            return Ok(Bytes::new());
        }
        let abs = self.start + self.pos;
        let layout = *self.metainfo.info().layout();
        let piece = layout
            .piece_at(abs)
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "past the content"))?;
        self.request_window(piece).await?;
        self.wait_piece(piece).await?;
        let piece_end = layout
            .piece_range(piece)
            .map_or(abs, |r| r.end)
            .min(self.start + self.len);
        let want = usize::try_from(piece_end - abs)
            .unwrap_or(usize::MAX)
            .min(max)
            .min(MAX_CHUNK);
        let storage = Arc::clone(&self.storage);
        let data = tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; want];
            storage
                .read_at(abs, &mut buf)
                .map(|()| buf)
                .map_err(io::Error::other)
        })
        .await
        .map_err(io::Error::other)??;
        self.pos += u64::try_from(data.len()).unwrap_or(0);
        Ok(Bytes::from(data))
    }

    /// Reads into `buf`; returns the number of bytes read (0 at the end).
    ///
    /// # Errors
    /// As [`Self::read_chunk`].
    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let chunk = self.read_chunk(buf.len()).await?;
        if let Some(dst) = buf.get_mut(..chunk.len()) {
            dst.copy_from_slice(&chunk);
        }
        Ok(chunk.len())
    }

    /// Reads `len` bytes at `offset` (or fewer at the end of the file),
    /// leaving the position after them.
    ///
    /// # Errors
    /// As [`Self::read_chunk`].
    pub async fn read_range(&mut self, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        self.seek(SeekFrom::Start(offset))?;
        let end = offset.saturating_add(len).min(self.len);
        let mut out = Vec::new();
        while self.pos < end {
            let left = usize::try_from(end - self.pos).unwrap_or(usize::MAX);
            let chunk = self.read_chunk(left).await?;
            if chunk.is_empty() {
                break;
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    /// Marks `piece..` (up to the readahead, within the file) urgent when
    /// the window moved.
    async fn request_window(&mut self, piece: u32) -> io::Result<()> {
        if self.window == Some(piece) {
            return Ok(());
        }
        let file_pieces = pieces_for(self.metainfo.info(), self.start, self.start + self.len);
        let end = piece.saturating_add(self.readahead).min(file_pieces.end);
        let pieces: Vec<u32> = (piece..end.max(piece + 1)).collect();
        self.commands
            .send(Command::Stream {
                id: self.id,
                pieces,
            })
            .await
            .map_err(|_| stopped())?;
        self.window = Some(piece);
        Ok(())
    }

    async fn wait_piece(&self, piece: u32) -> io::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Command::WaitPiece { piece, reply: tx })
            .await
            .map_err(|_| stopped())?;
        rx.await.map_err(|_| stopped())
    }
}

impl Drop for FileStream {
    fn drop(&mut self) {
        if self.window.is_none() {
            return;
        }
        match self.commands.try_send(Command::CloseStream(self.id)) {
            Err(mpsc::error::TrySendError::Full(command)) => {
                // Do not block in `drop`: deliver it from a task.
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    let commands = self.commands.clone();
                    runtime.spawn(async move {
                        let _ = commands.send(command).await;
                    });
                }
            }
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
}

fn stopped() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "torrent stopped")
}
