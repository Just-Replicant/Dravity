//! Session-wide rate limits: one token bucket per direction, shared by
//! every peer connection of every torrent, changeable while running.
//!
//! Peer tasks take tokens for each piece payload they send or receive.
//! A bucket may go into debt: the caller then sleeps until the debt is
//! repaid, so a large block never waits forever and concurrent callers
//! queue in order of arrival. Waiting on receive stops reading the
//! socket, which lets TCP (or uTP) flow control slow the sender.

use std::sync::Arc;
use std::time::Duration;

use dravity_core::rate::SharedBucket;
use tokio::sync::{Mutex, watch};
use tokio::time::Instant;

/// One direction's bucket ([`dravity_core::rate`]), on tokio's clock so
/// paused-time tests control it.
#[derive(Debug)]
pub(crate) struct Limiter {
    bucket: SharedBucket,
    /// FIFO admission prevents later callers from sharing an earlier
    /// caller's wakeup and exceeding the aggregate limit.
    turn: Mutex<()>,
    /// Rate changes wake the current admission so its remaining debt is
    /// immediately repriced without taking the bytes again.
    changes: watch::Sender<()>,
}

impl Limiter {
    fn new(rate: Option<u64>) -> Self {
        let (changes, _) = watch::channel(());
        Self {
            bucket: SharedBucket::new(rate, Instant::now().into_std()),
            turn: Mutex::new(()),
            changes,
        }
    }

    /// The limit in bytes per second.
    #[cfg(test)]
    fn rate(&self) -> Option<u64> {
        self.bucket.rate()
    }

    /// Changes the limit and wakes an in-flight admission immediately.
    pub(crate) fn set_rate(&self, rate: Option<u64>) {
        self.bucket.set_rate(rate, Instant::now().into_std());
        self.changes.send_replace(());
    }

    /// Takes `bytes` tokens and returns how long the caller must wait.
    fn take(&self, bytes: usize, now: Instant) -> Duration {
        self.bucket.take(bytes, now.into_std())
    }

    /// Waits until `bytes` may pass.
    pub(crate) async fn acquire(&self, bytes: usize) {
        let _turn = self.turn.lock().await;
        let mut changes = self.changes.subscribe();
        let mut wait = self.take(bytes, Instant::now());
        while !wait.is_zero() {
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                result = changes.changed() => {
                    debug_assert!(result.is_ok(), "limiter owns the watch sender");
                }
            }
            // The bytes were charged above. Only update existing debt after
            // a timer or setting change, otherwise every wake double-charges.
            wait = self.bucket.wait(Instant::now().into_std());
        }
    }
}

/// The session's upload and download limiters.
#[derive(Debug, Clone)]
pub(crate) struct RateLimits {
    pub(crate) up: Arc<Limiter>,
    pub(crate) down: Arc<Limiter>,
}

impl RateLimits {
    pub(crate) fn new(up: Option<u64>, down: Option<u64>) -> Self {
        Self {
            up: Arc::new(Limiter::new(up)),
            down: Arc::new(Limiter::new(down)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_never_waits() {
        let l = Limiter::new(None);
        assert_eq!(l.take(1 << 30, Instant::now()), Duration::ZERO);
        assert_eq!(Limiter::new(Some(0)).rate(), None);
    }

    /// Within 10 ms (the bucket's clock starts a moment before the test's).
    fn near(a: Duration, b: Duration) -> bool {
        a.abs_diff(b) <= Duration::from_millis(10)
    }

    #[test]
    fn debt_is_repaid_at_the_rate() {
        let l = Limiter::new(Some(1000));
        let t = Instant::now();
        // Empty bucket: 500 bytes cost half a second.
        assert!(near(l.take(500, t), Duration::from_millis(500)));
        // A second later the debt is repaid and 500 more tokens accrued.
        assert_eq!(l.take(500, t + Duration::from_millis(1500)), Duration::ZERO);
        // Bursts cap at one second of rate.
        let later = t + Duration::from_secs(100);
        assert_eq!(l.take(1000, later), Duration::ZERO);
        assert!(near(l.take(1000, later), Duration::from_secs(1)));
        assert_eq!(l.take(4000, later), Duration::from_secs(5));
    }

    #[test]
    fn rates_change_live() {
        let l = Limiter::new(Some(10));
        let t = Instant::now();
        assert!(!l.take(100, t).is_zero());
        l.set_rate(None);
        assert_eq!(l.take(1 << 20, t), Duration::ZERO);
        l.set_rate(Some(1 << 20));
        assert_eq!(l.rate(), Some(1 << 20));
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_acquires_queue_at_the_aggregate_rate() {
        let l = Arc::new(Limiter::new(Some(100)));
        let start = Instant::now();
        let first = {
            let l = Arc::clone(&l);
            tokio::spawn(async move {
                l.acquire(100).await;
                Instant::now() - start
            })
        };
        tokio::task::yield_now().await;
        let second = {
            let l = Arc::clone(&l);
            tokio::spawn(async move {
                l.acquire(100).await;
                Instant::now() - start
            })
        };
        tokio::task::yield_now().await;

        tokio::time::advance(Duration::from_millis(999)).await;
        tokio::task::yield_now().await;
        assert!(!first.is_finished());
        assert!(!second.is_finished());

        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert!(first.is_finished());
        assert!(!second.is_finished());

        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(matches!(
            first.await,
            Ok(elapsed) if elapsed == Duration::from_secs(1)
        ));
        assert!(matches!(
            second.await,
            Ok(elapsed) if elapsed == Duration::from_secs(2)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_live_rate_change_reprices_debt_without_charging_twice() {
        let l = Arc::new(Limiter::new(Some(10)));
        let waiter = {
            let l = Arc::clone(&l);
            tokio::spawn(async move { l.acquire(100).await })
        };
        tokio::task::yield_now().await;

        tokio::time::advance(Duration::from_secs(1)).await;
        l.set_rate(Some(90));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(999)).await;
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());

        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(waiter.await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn disabling_a_limit_wakes_all_queued_callers() {
        let l = Arc::new(Limiter::new(Some(1)));
        let first = {
            let l = Arc::clone(&l);
            tokio::spawn(async move { l.acquire(100).await })
        };
        let second = {
            let l = Arc::clone(&l);
            tokio::spawn(async move { l.acquire(100).await })
        };
        tokio::task::yield_now().await;

        l.set_rate(None);
        tokio::task::yield_now().await;
        assert!(first.await.is_ok());
        assert!(second.await.is_ok());
    }
}
