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

#[cfg(test)]
use dravity_core::rate::MAX_WAIT;
use dravity_core::rate::SharedBucket;
use tokio::time::Instant;

/// One direction's bucket ([`dravity_core::rate`]), on tokio's clock so
/// paused-time tests control it.
#[derive(Debug)]
pub(crate) struct Limiter(SharedBucket);

impl Limiter {
    fn new(rate: Option<u64>) -> Self {
        Self(SharedBucket::new(rate, Instant::now().into_std()))
    }

    /// The limit in bytes per second.
    #[cfg(test)]
    fn rate(&self) -> Option<u64> {
        self.0.rate()
    }

    /// Changes the limit; takes effect for the next call.
    pub(crate) fn set_rate(&self, rate: Option<u64>) {
        self.0.set_rate(rate, Instant::now().into_std());
    }

    /// Takes `bytes` tokens and returns how long the caller must wait.
    fn take(&self, bytes: usize, now: Instant) -> Duration {
        self.0.take(bytes, now.into_std())
    }

    /// Waits until `bytes` may pass.
    pub(crate) async fn acquire(&self, bytes: usize) {
        let wait = self.take(bytes, Instant::now());
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
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
        // Huge debts are capped per call.
        assert_eq!(l.take(1 << 30, later), MAX_WAIT);
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

    #[tokio::test]
    async fn acquire_sleeps_for_the_debt() {
        let l = Limiter::new(Some(10_000));
        let start = Instant::now();
        l.acquire(2000).await;
        assert!(start.elapsed() >= Duration::from_millis(190));
    }
}
