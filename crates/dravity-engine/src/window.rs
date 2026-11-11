//! AIMD request window (per peer): how many block requests may be
//! outstanding.
//!
//! Like TCP congestion control: the window grows by one per answered block
//! until the first timeout (slow start, doubling every round trip), then by
//! one per window of answered blocks (additive increase). A request timeout
//! halves the live limit, `min(window, reqq)`. Answered blocks still grow
//! the congestion window up to its maximum while a smaller `reqq` is what
//! the peer is actually sent. Fast peers get deep pipelines and slow ones
//! are not flooded with requests that will time out.
//!
//! [`Bdp`] adds a second bound from measurement: a peer's delivery rate
//! times its base round trip. A fast peer far away gets that depth at once
//! instead of after many round trips of growth. After a timeout the AIMD
//! window alone decides again for a while.

use std::time::{Duration, Instant};

use dravity_core::BLOCK_SIZE;

/// The request window of one peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Window {
    size: u32,
    threshold: u32,
    acked: u32,
    max: u32,
}

impl Window {
    pub(crate) fn new(initial: u32, max: u32) -> Self {
        let max = max.max(1);
        Self {
            size: initial.clamp(1, max),
            threshold: max,
            acked: 0,
            max,
        }
    }

    pub(crate) const fn size(&self) -> u32 {
        self.size
    }

    pub(crate) const fn max(&self) -> u32 {
        self.max
    }

    /// A requested block arrived.
    pub(crate) fn on_block(&mut self) {
        if self.size < self.threshold {
            self.size += 1;
        } else {
            self.acked += 1;
            if self.acked >= self.size {
                self.acked = 0;
                self.size += 1;
            }
        }
        self.size = self.size.min(self.max);
    }

    /// One or more requests timed out (call once per loss event).
    ///
    /// `cap` is the peer's `reqq`, or `u32::MAX` when it advertised none.
    /// The outstanding-request limit is `min(size, cap)`; that is what is
    /// halved. The congestion window may already have grown past `cap`,
    /// because answered blocks increase it even while the peer's queue is
    /// the tighter bound.
    pub(crate) fn on_timeout(&mut self, cap: u32) {
        let effective = self.size.min(cap.max(1));
        self.threshold = (effective / 2).max(1);
        self.size = self.threshold.min(self.max);
        self.acked = 0;
    }
}

/// How long a minimum round-trip sample stands before a newer sample may
/// replace it (paths change; a stale low minimum would starve the queue).
const MIN_RTT_WINDOW: Duration = Duration::from_secs(10);

/// Bandwidth-delay product of one peer: its delivery rate times its base
/// round-trip time, in blocks. Keeping that many requests outstanding
/// keeps the link full without queueing more than needed at the peer.
///
/// The round trip is the *minimum* request-to-block time seen recently:
/// the time a block spends queued behind our other requests grows with the
/// queue, so the smoothed time would feed back into an ever deeper queue.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Bdp {
    /// Base round trip and when it was sampled.
    min_rtt: Option<(Duration, Instant)>,
    /// Delivery rate in bytes per second (EWMA, 1/4 weight per sample).
    rate: u64,
    /// Bytes delivered since the last rate sample, and when that was.
    bytes: u64,
    since: Option<Instant>,
    /// After a request timeout the AIMD window alone sets the depth until
    /// this time: the path is congested, and its round trips are not base.
    quiet_until: Option<Instant>,
}

impl Bdp {
    /// A block requested at `requested` arrived at `now`.
    pub(crate) fn on_block(&mut self, now: Instant, requested: Instant, length: u32) {
        self.bytes = self.bytes.saturating_add(u64::from(length));
        if self.quiet_until.is_some_and(|until| now < until) {
            return;
        }
        let rtt = now.saturating_duration_since(requested);
        self.min_rtt = match self.min_rtt {
            Some((min, at)) if rtt >= min && now.saturating_duration_since(at) < MIN_RTT_WINDOW => {
                Some((min, at))
            }
            _ => Some((rtt, now)),
        };
    }

    /// A request timed out: drop the round-trip estimate and leave the
    /// depth to the AIMD window for [`MIN_RTT_WINDOW`].
    pub(crate) fn on_loss(&mut self, now: Instant) {
        self.min_rtt = None;
        self.quiet_until = Some(now + MIN_RTT_WINDOW);
    }

    /// Folds the bytes delivered since the last call into the rate (call
    /// once per tick).
    pub(crate) fn on_tick(&mut self, now: Instant) {
        let Some(since) = self.since.replace(now) else {
            self.bytes = 0;
            return;
        };
        let dt = now.saturating_duration_since(since).as_micros();
        if dt == 0 {
            return;
        }
        let sample = u64::try_from(u128::from(self.bytes) * 1_000_000 / dt).unwrap_or(u64::MAX);
        self.bytes = 0;
        self.rate = if self.rate == 0 {
            sample
        } else {
            (self.rate.saturating_mul(3) / 4).saturating_add(sample / 4)
        };
    }

    /// Delivery rate in bytes per second.
    #[cfg(test)]
    pub(crate) const fn rate(&self) -> u64 {
        self.rate
    }

    /// Outstanding requests that cover the bandwidth-delay product with a
    /// quarter of headroom plus two blocks; 0 before there is a rate and
    /// a round-trip sample.
    pub(crate) fn target(&self) -> u32 {
        let Some((rtt, _)) = self.min_rtt else {
            return 0;
        };
        if self.rate == 0 {
            return 0;
        }
        let bytes = u128::from(self.rate) * rtt.as_micros() * 5 / 4 / 1_000_000;
        let blocks = bytes.div_ceil(u128::from(BLOCK_SIZE)) + 2;
        u32::try_from(blocks).unwrap_or(u32::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bdp_covers_rate_times_base_rtt() {
        let t0 = Instant::now();
        let mut b = Bdp::default();
        assert_eq!(b.target(), 0, "no samples yet");
        b.on_tick(t0);
        // 1 MiB/s over 100 ms base RTT: 102 KiB in flight, 6.4 blocks,
        // x1.25 = 8 blocks, +2 headroom.
        b.on_block(t0 + Duration::from_millis(100), t0, BLOCK_SIZE);
        b.bytes = 1 << 20;
        b.on_tick(t0 + Duration::from_secs(1));
        assert_eq!(b.rate(), 1 << 20);
        assert_eq!(b.target(), 10);
        // A slower sample (queueing) does not raise the base RTT.
        b.on_block(
            t0 + Duration::from_secs(2),
            t0 + Duration::from_secs(1),
            BLOCK_SIZE,
        );
        assert_eq!(b.target(), 10);
        // After the window a new sample replaces the old minimum.
        let later = t0 + Duration::from_secs(12);
        b.on_block(later, later - Duration::from_millis(200), BLOCK_SIZE);
        assert_eq!(b.target(), 18);
    }

    #[test]
    fn a_loss_hands_the_depth_back_to_aimd() {
        let t0 = Instant::now();
        let mut b = Bdp::default();
        b.on_tick(t0);
        b.on_block(t0 + Duration::from_millis(50), t0, BLOCK_SIZE);
        b.bytes = 1 << 20;
        b.on_tick(t0 + Duration::from_secs(1));
        assert!(b.target() > 0);
        b.on_loss(t0 + Duration::from_secs(1));
        assert_eq!(b.target(), 0);
        b.on_block(
            t0 + Duration::from_secs(5),
            t0 + Duration::from_secs(4),
            BLOCK_SIZE,
        );
        assert_eq!(b.target(), 0, "no samples while quiet");
        let later = t0 + Duration::from_secs(12);
        b.on_block(later, later - Duration::from_millis(50), BLOCK_SIZE);
        assert!(b.target() > 0);
    }

    #[test]
    fn the_rate_is_smoothed() {
        let t0 = Instant::now();
        let mut b = Bdp::default();
        b.on_tick(t0);
        b.bytes = 4_000;
        b.on_tick(t0 + Duration::from_secs(1));
        assert_eq!(b.rate(), 4_000);
        b.bytes = 0;
        b.on_tick(t0 + Duration::from_secs(2));
        assert_eq!(b.rate(), 3_000);
    }

    #[test]
    fn slow_start_then_additive_increase() {
        let mut w = Window::new(4, 64);
        for _ in 0..4 {
            w.on_block();
        }
        assert_eq!(w.size(), 8, "doubles per window while slow-starting");
        w.on_timeout(u32::MAX);
        assert_eq!(w.size(), 4);
        for _ in 0..4 {
            w.on_block();
        }
        assert_eq!(w.size(), 5, "+1 per window after a loss");
        for _ in 0..10 {
            w.on_timeout(u32::MAX);
        }
        assert_eq!(w.size(), 1);
    }

    #[test]
    fn clamped() {
        let mut w = Window::new(100, 8);
        assert_eq!(w.size(), 8);
        for _ in 0..100 {
            w.on_block();
        }
        assert_eq!(w.size(), 8);
        assert_eq!(Window::new(0, 0).size(), 1);
    }

    #[test]
    fn timeout_halves_the_effective_limit() {
        let mut w = Window::new(4, 128);
        for _ in 0..200 {
            w.on_block();
        }
        assert_eq!(w.size(), 128);
        // reqq is 4, so the peer never had more than 4 requests in flight.
        w.on_timeout(4);
        assert_eq!(w.size(), 2);
        // The next loss halves that live limit, not reqq again.
        w.on_timeout(4);
        assert_eq!(w.size(), 1);

        let mut below = Window::new(4, 128);
        below.on_timeout(8);
        assert_eq!(below.size(), 2);
    }
}
