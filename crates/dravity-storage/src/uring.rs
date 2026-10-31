//! io_uring file I/O on Linux (feature `uring`), with
//! [`tokio-uring`](https://docs.rs/tokio-uring).
//!
//! tokio-uring runs its own single-threaded runtime that owns the ring, so
//! it cannot share a thread with the session's multi-threaded Tokio
//! runtime. [`UringIo`] therefore starts one dedicated thread that runs a
//! tokio-uring runtime and takes jobs over a channel. A caller (the
//! session's blocking pool) sends a job and waits for its reply; the uring
//! thread runs many jobs concurrently on the ring.
//!
//! Files are handed over once: [`FileIo::adopt`] moves the descriptor the
//! storage opened to the uring thread, which keeps it as a
//! `tokio_uring::fs::File` for as long as the storage's descriptor cache
//! holds the returned handle. Dropping the handle closes the file on the
//! ring. Reads, writes and `fdatasync` then go through the ring with no
//! per-operation `dup`.
//!
//! Every call is a round trip to that thread, so the storage batches:
//! [`OpenFile::write_batch`] and [`OpenFile::read_batch`] send a whole batch
//! of blocks as one job. The uring thread merges blocks that are adjacent in
//! the file into one `writev`/`readv` (at most `MAX_IOVECS` buffers each) and
//! submits every run of the batch together, then replies once. Whole pieces
//! (for verification) are already one read per file.
//!
//! No `unsafe` is needed, and no buffer is copied: writes move the caller's
//! [`Bytes`] into the ring and reads fill the caller's `Vec<u8>` and hand it
//! back, which is how tokio-uring keeps buffers alive until the kernel is
//! done with them.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::thread;

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio_uring::buf::BoundedBuf;

use crate::io::{FileIo, OpenFile};

/// Default submission queue size.
pub const DEFAULT_URING_ENTRIES: u32 = 256;

/// Most buffers merged into one `readv`/`writev` (Linux's `IOV_MAX` is
/// 1024).
const MAX_IOVECS: usize = 256;

type Reply<T> = std_mpsc::SyncSender<io::Result<T>>;

enum Job {
    /// Keep `file` under `id` until [`Job::Close`].
    Adopt {
        id: u64,
        file: File,
    },
    Read {
        id: u64,
        buf: Vec<u8>,
        len: usize,
        offset: u64,
        reply: Reply<Vec<u8>>,
    },
    Write {
        id: u64,
        data: Bytes,
        offset: u64,
        reply: Reply<()>,
    },
    /// Runs of adjacent writes: one `writev` each, all submitted at once.
    WriteBatch {
        id: u64,
        runs: Vec<(u64, Vec<Bytes>)>,
        reply: Reply<()>,
    },
    /// Reads `(offset, len)`, merged into one `readv` per run of adjacent
    /// ranges and submitted at once; the buffers come back in order.
    ReadBatch {
        id: u64,
        reads: Vec<(u64, usize)>,
        reply: Reply<Vec<Vec<u8>>>,
    },
    SyncData {
        id: u64,
        reply: Reply<()>,
    },
    Close {
        id: u64,
    },
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Adopt { id, .. } => write!(f, "Adopt({id})"),
            Self::Read {
                id, len, offset, ..
            } => write!(f, "Read({id}, offset {offset}, len {len})"),
            Self::Write {
                id, data, offset, ..
            } => write!(f, "Write({id}, offset {offset}, len {})", data.len()),
            Self::WriteBatch { id, runs, .. } => write!(f, "WriteBatch({id}, {} runs)", runs.len()),
            Self::ReadBatch { id, reads, .. } => {
                write!(f, "ReadBatch({id}, {} reads)", reads.len())
            }
            Self::SyncData { id, .. } => write!(f, "SyncData({id})"),
            Self::Close { id } => write!(f, "Close({id})"),
        }
    }
}

#[derive(Debug)]
struct Shared {
    jobs: mpsc::UnboundedSender<Job>,
    next_id: AtomicU64,
}

/// File I/O through io_uring on a dedicated thread.
///
/// Clones share the thread. It stops once every clone and every adopted
/// file (so every storage using it) is dropped and the queued jobs are
/// done.
///
/// ```no_run
/// # fn main() -> std::io::Result<()> {
/// use std::sync::Arc;
/// use dravity_storage::{BlockingIo, FileIo, UringIo};
///
/// // Fall back to blocking I/O where the kernel or a sandbox refuses io_uring.
/// let io: Arc<dyn FileIo> = match UringIo::start() {
///     Ok(uring) => Arc::new(uring),
///     Err(_) => Arc::new(BlockingIo),
/// };
/// # Ok(()) }
/// ```
#[derive(Debug, Clone)]
pub struct UringIo {
    shared: Arc<Shared>,
}

impl UringIo {
    /// Starts the uring thread with [`DEFAULT_URING_ENTRIES`].
    ///
    /// # Errors
    /// When io_uring is unavailable: an old kernel (`ENOSYS`), a sandbox or
    /// container that blocks the syscalls (`EPERM`, seccomp), the
    /// `kernel.io_uring_disabled` sysctl, or a ring that cannot run a no-op.
    pub fn start() -> io::Result<Self> {
        Self::start_with(DEFAULT_URING_ENTRIES)
    }

    /// Starts the uring thread with a submission queue of `entries`.
    ///
    /// # Errors
    /// As [`start`](Self::start); also for an invalid `entries` (0, or too
    /// large for the kernel).
    pub fn start_with(entries: u32) -> io::Result<Self> {
        let (jobs, queue) = mpsc::unbounded_channel::<Job>();
        let (ready_tx, ready_rx) = std_mpsc::sync_channel::<io::Result<()>>(1);
        thread::Builder::new()
            .name("dravity-uring".to_owned())
            .spawn(move || {
                let runtime =
                    match tokio_uring::Runtime::new(tokio_uring::builder().entries(entries)) {
                        Ok(runtime) => runtime,
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                            return;
                        }
                    };
                runtime.block_on(serve(queue, ready_tx));
            })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                shared: Arc::new(Shared {
                    jobs,
                    next_id: AtomicU64::new(0),
                }),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(io::Error::other(
                "the io_uring thread stopped while starting",
            )),
        }
    }
}

/// The uring thread: owns the adopted files and runs every job as its own
/// task on the ring.
async fn serve(mut queue: mpsc::UnboundedReceiver<Job>, ready: Reply<()>) {
    // Setting up a ring can succeed where submitting to it does not (some
    // seccomp profiles): prove a round trip.
    if let Err(e) = tokio_uring::no_op().await {
        let _ = ready.send(Err(e));
        return;
    }
    let _ = ready.send(Ok(()));
    let mut files: HashMap<u64, Rc<tokio_uring::fs::File>> = HashMap::new();
    let mut running = Vec::new();
    while let Some(job) = queue.recv().await {
        running.retain(|t: &tokio::task::JoinHandle<()>| !t.is_finished());
        let task = match job {
            Job::Adopt { id, file } => {
                files.insert(id, Rc::new(tokio_uring::fs::File::from_std(file)));
                continue;
            }
            Job::Close { id } => match files.remove(&id) {
                Some(file) => tokio_uring::spawn(close(file)),
                None => continue,
            },
            Job::Read {
                id,
                buf,
                len,
                offset,
                reply,
            } => match files.get(&id).cloned() {
                Some(file) => tokio_uring::spawn(async move {
                    let _ = reply.send(read(&file, buf, len, offset).await);
                }),
                None => {
                    let _ = reply.send(Err(not_open()));
                    continue;
                }
            },
            Job::Write {
                id,
                data,
                offset,
                reply,
            } => match files.get(&id).cloned() {
                Some(file) => tokio_uring::spawn(async move {
                    let (result, _) = file.write_all_at(data, offset).await;
                    let _ = reply.send(result);
                }),
                None => {
                    let _ = reply.send(Err(not_open()));
                    continue;
                }
            },
            Job::WriteBatch { id, runs, reply } => match files.get(&id).cloned() {
                Some(file) => tokio_uring::spawn(async move {
                    let _ = reply.send(write_runs(file, runs).await);
                }),
                None => {
                    let _ = reply.send(Err(not_open()));
                    continue;
                }
            },
            Job::ReadBatch { id, reads, reply } => match files.get(&id).cloned() {
                Some(file) => tokio_uring::spawn(async move {
                    let _ = reply.send(read_runs(file, &reads).await);
                }),
                None => {
                    let _ = reply.send(Err(not_open()));
                    continue;
                }
            },
            Job::SyncData { id, reply } => match files.get(&id).cloned() {
                Some(file) => tokio_uring::spawn(async move {
                    let _ = reply.send(file.sync_data().await);
                }),
                None => {
                    let _ = reply.send(Err(not_open()));
                    continue;
                }
            },
        };
        running.push(task);
    }
    // Finish what was queued before the last sender left; files still in
    // the map close when it drops.
    for task in running {
        let _ = task.await;
    }
}

/// Appends `len` bytes read at `offset` to `buf`, in place.
async fn read(
    file: &tokio_uring::fs::File,
    mut buf: Vec<u8>,
    len: usize,
    offset: u64,
) -> io::Result<Vec<u8>> {
    let start = buf.len();
    let end = start
        .checked_add(len)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    buf.reserve(len);
    // The slice bounds the read to `start..end` of the allocation; tokio-uring
    // extends the vector's length as bytes arrive.
    let (result, slice) = file.read_exact_at(buf.slice(start..end), offset).await;
    result?;
    let buf = slice.into_inner();
    if buf.len() == end {
        Ok(buf)
    } else {
        Err(io::ErrorKind::UnexpectedEof.into())
    }
}

/// Writes every run at once (one task, so one submission, per run) and
/// waits for all of them.
async fn write_runs(
    file: Rc<tokio_uring::fs::File>,
    runs: Vec<(u64, Vec<Bytes>)>,
) -> io::Result<()> {
    let tasks: Vec<_> = runs
        .into_iter()
        .map(|(offset, bufs)| {
            let file = Rc::clone(&file);
            tokio_uring::spawn(async move { writev_all(&file, bufs, offset).await })
        })
        .collect();
    let mut result = Ok(());
    for task in tasks {
        let done = task.await.unwrap_or_else(|e| Err(io::Error::other(e)));
        if result.is_ok() {
            result = done;
        }
    }
    result
}

/// Writes `bufs` back to back from `offset` with `writev`, continuing after
/// a short write.
async fn writev_all(
    file: &tokio_uring::fs::File,
    mut bufs: Vec<Bytes>,
    mut offset: u64,
) -> io::Result<()> {
    bufs.retain(|b| !b.is_empty());
    loop {
        if bufs.len() <= 1 {
            let Some(data) = bufs.pop() else {
                return Ok(());
            };
            return file.write_all_at(data, offset).await.0;
        }
        let (result, written) = file.writev_at(bufs, offset).await;
        let mut n = result?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        offset += n as u64;
        bufs = Vec::with_capacity(written.len());
        for buf in written {
            if n >= buf.len() {
                n -= buf.len();
            } else {
                bufs.push(Bytes::slice(&buf, n..));
                n = 0;
            }
        }
    }
}

/// Reads every `(offset, len)` of `reads`: adjacent ranges (in offset
/// order) share one `readv`, and every run is submitted at once.
async fn read_runs(
    file: Rc<tokio_uring::fs::File>,
    reads: &[(u64, usize)],
) -> io::Result<Vec<Vec<u8>>> {
    let mut order: Vec<usize> = (0..reads.len()).collect();
    order.sort_by_key(|&i| reads.get(i).map_or(0, |r| r.0));
    // Runs of positions in `reads`.
    let mut runs: Vec<Vec<usize>> = Vec::new();
    let mut end = None;
    for i in order {
        let Some(&(offset, len)) = reads.get(i) else {
            continue;
        };
        match runs.last_mut() {
            Some(run) if end == Some(offset) && run.len() < MAX_IOVECS => run.push(i),
            _ => runs.push(vec![i]),
        }
        end = offset.checked_add(len as u64);
    }
    let tasks: Vec<_> = runs
        .into_iter()
        .map(|run| {
            let file = Rc::clone(&file);
            let ranges: Vec<(u64, usize)> =
                run.iter().filter_map(|&i| reads.get(i).copied()).collect();
            let task = tokio_uring::spawn(async move { readv_all(&file, &ranges).await });
            (run, task)
        })
        .collect();
    let mut out: Vec<Vec<u8>> = vec![Vec::new(); reads.len()];
    for (run, task) in tasks {
        let bufs = task.await.unwrap_or_else(|e| Err(io::Error::other(e)))?;
        for (i, buf) in run.into_iter().zip(bufs) {
            if let Some(slot) = out.get_mut(i) {
                *slot = buf;
            }
        }
    }
    Ok(out)
}

/// Reads adjacent `ranges` (the first starts the run) with one `readv`,
/// finishing any buffer a short read left incomplete on its own.
async fn readv_all(
    file: &tokio_uring::fs::File,
    ranges: &[(u64, usize)],
) -> io::Result<Vec<Vec<u8>>> {
    let Some(&(start, _)) = ranges.first() else {
        return Ok(Vec::new());
    };
    if let [(offset, len)] = ranges {
        return Ok(vec![
            read(file, Vec::with_capacity(*len), *len, *offset).await?,
        ]);
    }
    let bufs: Vec<_> = ranges
        .iter()
        .map(|&(_, len)| Vec::with_capacity(len).slice(0..len))
        .collect();
    let (result, bufs) = file.readv_at(bufs, start).await;
    result?;
    let mut out = Vec::with_capacity(bufs.len());
    for (buf, &(offset, len)) in bufs.into_iter().zip(ranges) {
        let buf = buf.into_inner();
        let have = buf.len();
        out.push(if have < len {
            read(file, buf, len - have, offset + have as u64).await?
        } else {
            buf
        });
    }
    Ok(out)
}

/// Closes `file` on the ring once the operations still holding it are done.
async fn close(mut file: Rc<tokio_uring::fs::File>) {
    loop {
        match Rc::try_unwrap(file) {
            Ok(file) => {
                let _ = file.close().await;
                return;
            }
            Err(shared) => {
                file = shared;
                tokio::task::yield_now().await;
            }
        }
    }
}

fn not_open() -> io::Error {
    io::Error::other("the file is not open on the io_uring thread")
}

fn stopped() -> io::Error {
    io::Error::other("the io_uring thread has stopped")
}

impl FileIo for UringIo {
    fn name(&self) -> &'static str {
        "io_uring"
    }

    fn adopt(&self, file: File) -> io::Result<Arc<dyn OpenFile>> {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        self.shared
            .jobs
            .send(Job::Adopt { id, file })
            .map_err(|_| stopped())?;
        Ok(Arc::new(UringFile {
            id,
            jobs: self.shared.jobs.clone(),
        }))
    }
}

/// A file kept open on the uring thread; dropping it closes the file there.
#[derive(Debug)]
struct UringFile {
    id: u64,
    jobs: mpsc::UnboundedSender<Job>,
}

impl UringFile {
    fn submit<T>(&self, job: Job, reply: &std_mpsc::Receiver<io::Result<T>>) -> io::Result<T> {
        self.jobs.send(job).map_err(|_| stopped())?;
        reply.recv().map_err(|_| stopped())?
    }
}

impl OpenFile for UringFile {
    fn write_all_at(&self, data: Bytes, offset: u64) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let (reply, rx) = std_mpsc::sync_channel(1);
        let job = Job::Write {
            id: self.id,
            data,
            offset,
            reply,
        };
        self.submit(job, &rx)
    }

    fn read_exact_at(&self, buf: Vec<u8>, len: usize, offset: u64) -> io::Result<Vec<u8>> {
        if len == 0 {
            return Ok(buf);
        }
        let (reply, rx) = std_mpsc::sync_channel(1);
        let job = Job::Read {
            id: self.id,
            buf,
            len,
            offset,
            reply,
        };
        self.submit(job, &rx)
    }

    fn write_batch(&self, mut writes: Vec<(u64, Bytes)>) -> io::Result<()> {
        writes.retain(|(_, data)| !data.is_empty());
        if writes.is_empty() {
            return Ok(());
        }
        writes.sort_by_key(|(offset, _)| *offset);
        let mut runs: Vec<(u64, Vec<Bytes>)> = Vec::new();
        let mut end = None;
        for (offset, data) in writes {
            let next = offset.checked_add(data.len() as u64);
            match runs.last_mut() {
                Some((_, bufs)) if end == Some(offset) && bufs.len() < MAX_IOVECS => {
                    bufs.push(data)
                }
                _ => runs.push((offset, vec![data])),
            }
            end = next;
        }
        let (reply, rx) = std_mpsc::sync_channel(1);
        let job = Job::WriteBatch {
            id: self.id,
            runs,
            reply,
        };
        self.submit(job, &rx)
    }

    fn read_batch(&self, reads: &[(u64, usize)]) -> io::Result<Vec<Vec<u8>>> {
        let (reply, rx) = std_mpsc::sync_channel(1);
        let job = Job::ReadBatch {
            id: self.id,
            reads: reads.to_vec(),
            reply,
        };
        self.submit(job, &rx)
    }

    fn sync_data(&self) -> io::Result<()> {
        let (reply, rx) = std_mpsc::sync_channel(1);
        self.submit(Job::SyncData { id: self.id, reply }, &rx)
    }
}

impl Drop for UringFile {
    fn drop(&mut self) {
        let _ = self.jobs.send(Job::Close { id: self.id });
    }
}
