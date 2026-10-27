//! Token-bucket rate limiting with caller-supplied time (sans-IO).

use std::time::Instant;

/// A token bucket: `rate` tokens (bytes) per second, holding at most
/// `capacity`. The caller passes the current time; nothing reads a clock.
///
/// ```
/// use std::time::{Duration, Instant};
/// use dravity_engine::TokenBucket;
///
/// let start = Instant::now();
/// let mut bucket = TokenBucket::new(1000, 1000, start);
/// assert!(bucket.try_take(600));
/// assert!(!bucket.try_take(600));
/// bucket.refill(start + Duration::from_millis(200));
/// assert!(bucket.try_take(600));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenBucket {
    rate: u64,
    capacity: u64,
    tokens: u64,
    last: Instant,
}

impl TokenBucket {
    /// A full bucket refilled at `rate` per second up to `capacity`.
    #[must_use]
    pub fn new(rate: u64, capacity: u64, now: Instant) -> Self {
        let capacity = capacity.max(1);
        Self {
            rate,
            capacity,
            tokens: capacity,
            last: now,
        }
    }

    /// Tokens per second.
    #[must_use]
    pub const fn rate(&self) -> u64 {
        self.rate
    }

    /// Tokens available now.
    #[must_use]
    pub const fn available(&self) -> u64 {
        self.tokens
    }

    /// Adds the tokens earned since the last refill. Fractions of a token
    /// carry over to the next call.
    pub fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_nanos();
        let earned = elapsed.saturating_mul(u128::from(self.rate)) / 1_000_000_000;
        if earned == 0 {
            return;
        }
        let earned = u64::try_from(earned).unwrap_or(u64::MAX);
        self.tokens = self.tokens.saturating_add(earned).min(self.capacity);
        self.last = now;
    }

    /// Takes `n` tokens if that many are available.
    pub fn try_take(&mut self, n: u64) -> bool {
        if self.tokens >= n {
            self.tokens -= n;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn caps_and_carries_fractions() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(3, 10, t0);
        assert!(b.try_take(10));
        // 3 tokens/s: 100 ms earns 0.3 tokens, which must not be lost.
        for i in 1..=3 {
            b.refill(t0 + Duration::from_millis(100 * i));
        }
        assert_eq!(b.available(), 0);
        b.refill(t0 + Duration::from_millis(400));
        assert_eq!(b.available(), 1);
        b.refill(t0 + Duration::from_secs(100));
        assert_eq!(b.available(), 10);
        assert_eq!(b.rate(), 3);
    }
}
