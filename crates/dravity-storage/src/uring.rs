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
