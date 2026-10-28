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

#[cfg(test)]
mod tests {
    use super::*;

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
