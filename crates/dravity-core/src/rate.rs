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

/// Legacy wait interval retained for source compatibility.
///
/// [`TokenBucket::take`] returns the complete wait now; callers must not
/// use this value to truncate it, because doing so lets deep debt escape.
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

    #[allow(clippy::cast_precision_loss)] // rates use floating-point token arithmetic
    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = self.last.max(now);
        if let Some(rate) = self.rate {
            let rate = rate as f64;
            self.tokens = (self.tokens + elapsed * rate).min(rate);
        }
    }

    #[allow(clippy::cast_precision_loss)] // rates use floating-point token arithmetic
    fn debt_wait(&self) -> Duration {
        let Some(rate) = self.rate else {
            return Duration::ZERO;
        };
        if self.tokens >= 0.0 {
            return Duration::ZERO;
        }
        let seconds = -self.tokens / rate as f64;
        if seconds >= Duration::MAX.as_secs_f64() {
            Duration::MAX
        } else {
            Duration::from_secs_f64(seconds)
        }
    }

    /// Changes the limit from `now`.
    ///
    /// Debt is first repaid at the old rate through `now`, then repriced
    /// at the new rate. Saved-up burst credit is dropped when the rate
    /// changes. Disabling the limit clears debt, and setting the same rate
    /// leaves accumulated credit intact.
    pub fn set_rate(&mut self, rate: Option<u64>, now: Instant) {
        let rate = rate.filter(|&r| r > 0);
        self.refill(now);
        if rate == self.rate {
            return;
        }
        self.tokens = if rate.is_none() {
            0.0
        } else {
            self.tokens.min(0.0)
        };
        self.rate = rate;
    }

    /// Takes `bytes` tokens at `now` and returns the complete time needed
    /// to repay the resulting debt. At most one second of rate is saved up
    /// as burst.
    #[allow(clippy::cast_precision_loss)] // byte counts far below 2^52
    pub fn take(&mut self, bytes: usize, now: Instant) -> Duration {
        if self.rate.is_none() {
            return Duration::ZERO;
        }
        self.refill(now);
        self.tokens -= bytes as f64;
        self.debt_wait()
    }

    /// Returns the time needed to repay existing debt at `now`, without
    /// taking more tokens.
    pub fn wait(&mut self, now: Instant) -> Duration {
        self.refill(now);
        self.debt_wait()
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

    /// See [`TokenBucket::wait`].
    pub fn wait(&self, now: Instant) -> Duration {
        self.with(|b| b.wait(now))
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
        assert_eq!(b.wait(t), Duration::from_millis(500));
        assert_eq!(b.take(500, t + Duration::from_millis(1500)), Duration::ZERO);
        let later = t + Duration::from_secs(100);
        assert_eq!(b.take(1000, later), Duration::ZERO);
        assert_eq!(b.take(1000, later), Duration::from_secs(1));
        assert_eq!(
            b.take(4000, later),
            Duration::from_secs(5),
            "deep debt must not be truncated"
        );
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
        assert_eq!(shared.take(100, t), Duration::from_secs(10));
        shared.set_rate(Some(20), t + Duration::from_secs(2));
        assert_eq!(
            shared.wait(t + Duration::from_secs(2)),
            Duration::from_secs(4),
            "elapsed time uses the old rate and remaining debt the new rate"
        );
        shared.set_rate(None, t + Duration::from_secs(2));
        assert_eq!(shared.take(1 << 20, t), Duration::ZERO);
        shared.set_rate(Some(10), t + Duration::from_secs(2));
        assert_eq!(
            shared.wait(t + Duration::from_secs(2)),
            Duration::ZERO,
            "disabling a limit must not leave dormant debt"
        );
    }
}
