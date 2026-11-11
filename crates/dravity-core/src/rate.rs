//! A token bucket for rate limits, shared between threads.
//!
//! The bucket may go into debt: [`TokenBucket::take`] always succeeds and
//! returns how long the caller should wait, so a large block never waits
//! forever and concurrent callers queue in order of arrival. The clock is
//! passed in, so the arithmetic is deterministic and testable.
//!
//! [`SharedBucket`] wraps it in a mutex. Under `--cfg loom` that is
//! `loom`'s mutex, so `tests/loom_rate.rs` checks every interleaving of
//! concurrent takes and rate changes.

use std::time::{Duration, Instant};

#[cfg(loom)]
use loom::sync::{Arc, Mutex};
#[cfg(not(loom))]
use std::sync::{Arc, Mutex};

/// Longest a single take asks the caller to wait, so lowering a limit
/// sharply does not stall a connection for minutes.
pub const MAX_WAIT: Duration = Duration::from_secs(2);

/// One direction's token bucket.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    /// Bytes per second; `None` is unlimited.
    rate: Option<u64>,
    /// Available bytes; negative is debt.
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    /// An empty bucket filling at `rate` bytes per second from `now`
    /// (`None` or 0: unlimited).
    #[must_use]
    pub fn new(rate: Option<u64>, now: Instant) -> Self {
        Self {
            rate: rate.filter(|&r| r > 0),
            tokens: 0.0,
            last: now,
        }
    }

    /// The limit in bytes per second.
    #[must_use]
    pub const fn rate(&self) -> Option<u64> {
        self.rate
    }

    /// Changes the limit from `now`; any saved-up burst is dropped, debt
    /// is kept.
    pub fn set_rate(&mut self, rate: Option<u64>, now: Instant) {
        self.rate = rate.filter(|&r| r > 0);
        self.tokens = self.tokens.min(0.0);
        self.last = now;
    }

    /// Takes `bytes` tokens at `now` and returns how long to wait (at most
    /// [`MAX_WAIT`]). At most one second of rate is saved up as burst.
    #[allow(clippy::cast_precision_loss)] // byte counts far below 2^52
    pub fn take(&mut self, bytes: usize, now: Instant) -> Duration {
        let Some(rate) = self.rate else {
            return Duration::ZERO;
        };
        let rate = rate as f64;
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = self.last.max(now);
        self.tokens = (self.tokens + elapsed * rate).min(rate);
        self.tokens -= bytes as f64;
        if self.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-self.tokens / rate).min(MAX_WAIT)
        }
    }

    /// Available tokens (negative: debt), as of the last take.
    #[must_use]
    pub const fn tokens(&self) -> f64 {
        self.tokens
    }
}

/// A [`TokenBucket`] shared by many connections.
#[derive(Debug, Clone)]
pub struct SharedBucket(Arc<Mutex<TokenBucket>>);

impl SharedBucket {
    /// See [`TokenBucket::new`].
    #[must_use]
    pub fn new(rate: Option<u64>, now: Instant) -> Self {
        Self(Arc::new(Mutex::new(TokenBucket::new(rate, now))))
    }

    fn with<T>(&self, f: impl FnOnce(&mut TokenBucket) -> T) -> T {
        // A panic while holding the lock cannot leave the bucket in a
        // state worse than "slightly wrong debt", so poisoning is ignored.
        let mut guard = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut guard)
    }

    /// See [`TokenBucket::take`].
    pub fn take(&self, bytes: usize, now: Instant) -> Duration {
        self.with(|b| b.take(bytes, now))
    }

    /// See [`TokenBucket::set_rate`].
    pub fn set_rate(&self, rate: Option<u64>, now: Instant) {
        self.with(|b| b.set_rate(rate, now));
    }

    /// See [`TokenBucket::rate`].
    #[must_use]
    pub fn rate(&self) -> Option<u64> {
        self.with(|b| b.rate())
    }

    /// See [`TokenBucket::tokens`].
    #[must_use]
    pub fn tokens(&self) -> f64 {
        self.with(|b| b.tokens())
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn debt_is_repaid_at_the_rate() {
        let t = Instant::now();
        let mut b = TokenBucket::new(Some(1000), t);
        assert_eq!(b.take(500, t), Duration::from_millis(500));
        assert_eq!(b.take(500, t + Duration::from_millis(1500)), Duration::ZERO);
        let later = t + Duration::from_secs(100);
        assert_eq!(b.take(1000, later), Duration::ZERO);
        assert_eq!(b.take(1000, later), Duration::from_secs(1));
        assert_eq!(b.take(1 << 30, later), MAX_WAIT);
        // A clock that goes backwards (callers race to the lock) adds no
        // tokens and does not rewind the bucket.
        let before = b.tokens();
        b.take(0, t);
        assert!((b.tokens() - before).abs() < f64::EPSILON);
    }

    #[test]
    fn unlimited_and_live_changes() {
        let t = Instant::now();
        let shared = SharedBucket::new(Some(0), t);
        assert_eq!(shared.rate(), None);
        assert_eq!(shared.take(1 << 30, t), Duration::ZERO);
        shared.set_rate(Some(10), t);
        assert!(!shared.take(100, t).is_zero());
        shared.set_rate(None, t);
        assert_eq!(shared.take(1 << 20, t), Duration::ZERO);
    }
}
