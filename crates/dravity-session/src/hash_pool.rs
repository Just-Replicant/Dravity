//! The session-wide pool for piece hashing.
//!
//! Every torrent's hash jobs (piece checks, rechecks and smart-ban block
//! digests) share one bound, [`SessionConfig::hash_workers`]. With a bound
//! per torrent, N torrents rechecking at once would keep N × cores
//! blocking threads busy and starve the runtime. The jobs run on Tokio's
//! blocking pool. The semaphore is FIFO, so torrents take turns. A job
//! that panics is reported as `None` and frees its slot like any other.
//!
//! [`SessionConfig::hash_workers`]: crate::SessionConfig::hash_workers

use std::sync::Arc;

use tokio::sync::Semaphore;

/// A bounded pool of hashing jobs. Clones share the bound.
#[derive(Debug, Clone)]
pub(crate) struct HashPool {
    slots: Arc<Semaphore>,
    workers: usize,
}

impl HashPool {
    /// A pool running at most `workers` jobs at once (at least one).
    pub(crate) fn new(workers: usize) -> Self {
        let workers = workers.max(1);
        Self {
            slots: Arc::new(Semaphore::new(workers)),
            workers,
        }
    }

    /// Jobs running now.
    pub(crate) fn busy(&self) -> usize {
        self.workers.saturating_sub(self.slots.available_permits())
    }

    /// Runs `job` on a blocking thread once a slot is free. `None` if the
    /// job panicked.
    pub(crate) async fn run<T, F>(&self, job: F) -> Option<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let _permit = Arc::clone(&self.slots).acquire_owned().await.ok()?;
        match tokio::task::spawn_blocking(job).await {
            Ok(out) => Some(out),
            Err(e) => {
                tracing::warn!(error = %e, "hash job failed");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn jobs_from_every_caller_share_the_bound() {
        let pool = HashPool::new(2);
        let (running, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let mut tasks = Vec::new();
        // Two "torrents" with clones of the same pool, four jobs each.
        for _ in 0..8 {
            let (pool, running, peak) = (pool.clone(), Arc::clone(&running), Arc::clone(&peak));
            tasks.push(tokio::spawn(async move {
                pool.run(move || {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(20));
                    running.fetch_sub(1, Ordering::SeqCst);
                })
                .await
            }));
        }
        for t in tasks {
            assert_eq!(t.await.unwrap(), Some(()));
        }
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        assert_eq!(pool.busy(), 0);
    }

    #[tokio::test]
    async fn a_panicking_job_frees_its_slot() {
        let pool = HashPool::new(1);
        let failed: Option<()> = pool.run(|| panic!("bad job")).await;
        assert_eq!(failed, None);
        assert_eq!(pool.run(|| 7).await, Some(7));
        assert_eq!(
            HashPool::new(0).run(|| 1).await,
            Some(1),
            "at least one worker"
        );
    }
}
