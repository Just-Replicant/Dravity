//! AIMD request window (per peer): how many block requests may be
//! outstanding.
//!
//! Like TCP congestion control: the window grows by one per answered block
//! until the first timeout (slow start, doubling every round trip), then by
//! one per window of answered blocks (additive increase). A request timeout
//! halves it (multiplicative decrease). Fast peers get deep pipelines and
//! slow ones are not flooded with requests that will time out.

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
    pub(crate) fn on_timeout(&mut self) {
        self.threshold = (self.size / 2).max(1);
        self.size = self.threshold;
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
        w.on_timeout();
        assert_eq!(w.size(), 4);
        for _ in 0..4 {
            w.on_block();
        }
        assert_eq!(w.size(), 5, "+1 per window after a loss");
        for _ in 0..10 {
            w.on_timeout();
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
}
