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
//! Each job hands the ring its own duplicate of the cached file descriptor
//! (`dup`), which the uring thread closes when the operation is done. The
//! storage's descriptor cache, path checks and `fsync`s stay exactly as in
//! the blocking backend.
//!
//! No `unsafe` is needed: buffers are moved into the ring by value
//! (`Vec<u8>`), which is how tokio-uring keeps them alive until the kernel
//! is done with them.

use std::fs::File;
use std::io;
use std::sync::mpsc as std_mpsc;
use std::thread;

use tokio::sync::mpsc;
use tokio_uring::buf::BoundedBuf;

use crate::io::FileIo;

/// Default submission queue size.
pub const DEFAULT_URING_ENTRIES: u32 = 256;

enum Job {
    Read {
        file: File,
        offset: u64,
        len: usize,
        reply: std_mpsc::SyncSender<io::Result<Vec<u8>>>,
    },
    Write {
        file: File,
        offset: u64,
        data: Vec<u8>,
        reply: std_mpsc::SyncSender<io::Result<()>>,
    },
}

/// Positional file I/O through io_uring on a dedicated thread.
///
/// Clones share the thread. It stops once every clone (and every storage
/// using it) is dropped and the queued jobs are done.
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
    jobs: mpsc::UnboundedSender<Job>,
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read { offset, len, .. } => {
                write!(f, "Read {{ offset: {offset}, len: {len} }}")
            }
            Self::Write { offset, data, .. } => {
                write!(f, "Write {{ offset: {offset}, len: {} }}", data.len())
            }
        }
    }
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
        let (jobs, mut queue) = mpsc::unbounded_channel::<Job>();
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
                runtime.block_on(async move {
                    // Setting up a ring can succeed where submitting to it
                    // does not (some seccomp profiles): prove a round trip.
                    if let Err(e) = tokio_uring::no_op().await {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                    let _ = ready_tx.send(Ok(()));
                    let mut running = Vec::new();
                    while let Some(job) = queue.recv().await {
                        running.retain(|t: &tokio::task::JoinHandle<()>| !t.is_finished());
                        running.push(tokio_uring::spawn(run(job)));
                    }
                    // Finish what was queued before the last sender left.
                    for task in running {
                        let _ = task.await;
                    }
                });
            })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self { jobs }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(io::Error::other(
                "the io_uring thread stopped while starting",
            )),
        }
    }

    fn submit<T>(&self, job: Job, reply: &std_mpsc::Receiver<io::Result<T>>) -> io::Result<T> {
        self.jobs.send(job).map_err(|_| stopped())?;
        reply.recv().map_err(|_| stopped())?
    }
}

fn stopped() -> io::Error {
    io::Error::other("the io_uring thread has stopped")
}

async fn run(job: Job) {
    match job {
        Job::Read {
            file,
            offset,
            len,
            reply,
        } => {
            let file = tokio_uring::fs::File::from_std(file);
            // A slice bounds the read to `len` bytes, whatever the
            // allocation's capacity.
            let (result, buf) = file
                .read_exact_at(Vec::with_capacity(len).slice(..len), offset)
                .await;
            let _ = file.close().await;
            let buf = buf.into_inner();
            let _ = reply.send(result.and_then(|()| {
                if buf.len() == len {
                    Ok(buf)
                } else {
                    Err(io::ErrorKind::UnexpectedEof.into())
                }
            }));
        }
        Job::Write {
            file,
            offset,
            data,
            reply,
        } => {
            let file = tokio_uring::fs::File::from_std(file);
            let (result, _) = file.write_all_at(data, offset).await;
            let _ = file.close().await;
            let _ = reply.send(result);
        }
    }
}

impl FileIo for UringIo {
    fn name(&self) -> &'static str {
        "io_uring"
    }

    fn write_all_at(&self, file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let (reply, rx) = std_mpsc::sync_channel(1);
        let job = Job::Write {
            file: file.try_clone()?,
            offset,
            data: buf.to_vec(),
            reply,
        };
        self.submit(job, &rx)
    }

    fn read_exact_at(&self, file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let (reply, rx) = std_mpsc::sync_channel(1);
        let job = Job::Read {
            file: file.try_clone()?,
            offset,
            len: buf.len(),
            reply,
        };
        let data = self.submit(job, &rx)?;
        buf.copy_from_slice(&data);
        Ok(())
    }
}
