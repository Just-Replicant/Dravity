//! Session-wide connection admission.
//!
//! Every peer connection, whether dialed or accepted, over TCP or uTP,
//! takes a [`ConnectionSlot`] from one [`ConnectionLimit`] before it does
//! any work. The slot is given back when it is dropped. The decision itself
//! is the pure [`admit_permitted`], so every path applies the same rule.
//!
//! The counter is lock-free. Under `--cfg loom` it is built on `loom`'s
//! atomics, so `tests/loom_admit.rs` can check every interleaving of
//! concurrent admits and releases.

use std::fmt;

#[cfg(loom)]
use loom::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
#[cfg(not(loom))]
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// Encodes "no limit" in the atomic maximum.
const UNLIMITED: usize = usize::MAX;

/// Whether one more connection may open while `live` are open under
/// `max` (`None`: no limit).
///
/// ```
/// use dravity_core::admit::admit_permitted;
/// assert!(admit_permitted(Some(2), 1));
/// assert!(!admit_permitted(Some(2), 2));
/// assert!(!admit_permitted(Some(0), 0));
/// assert!(admit_permitted(None, 1_000_000));
/// ```
#[must_use]
pub const fn admit_permitted(max: Option<usize>, live: usize) -> bool {
    match max {
        Some(max) => live < max,
        None => true,
    }
}

struct Counters {
    max: AtomicUsize,
    live: AtomicUsize,
}

/// A shared cap on open connections. Clones share the same counter.
#[derive(Clone)]
pub struct ConnectionLimit(Arc<Counters>);

impl ConnectionLimit {
    /// A limit of `max` connections (`None`: unlimited, only counted).
    #[must_use]
    pub fn new(max: Option<usize>) -> Self {
        Self(Arc::new(Counters {
            max: AtomicUsize::new(max.unwrap_or(UNLIMITED)),
            live: AtomicUsize::new(0),
        }))
    }

    /// The configured maximum.
    #[must_use]
    pub fn max(&self) -> Option<usize> {
        let max = self.0.max.load(Ordering::Relaxed);
        (max != UNLIMITED).then_some(max)
    }

    /// Changes the maximum. Open connections above a lowered maximum are
    /// not closed. New ones wait until enough of them have ended.
    pub fn set_max(&self, max: Option<usize>) {
        self.0
            .max
            .store(max.unwrap_or(UNLIMITED), Ordering::Relaxed);
    }

    /// Connections holding a slot now.
    #[must_use]
    pub fn live(&self) -> usize {
        self.0.live.load(Ordering::Acquire)
    }

    /// Takes a slot if [`admit_permitted`] allows one more connection.
    /// The count never goes above the maximum, even under contention.
    #[must_use]
    pub fn try_admit(&self) -> Option<ConnectionSlot> {
        let mut live = self.0.live.load(Ordering::Acquire);
        loop {
            if !admit_permitted(self.max(), live) {
                return None;
            }
            let next = live.checked_add(1)?;
            match self
                .0
                .live
                .compare_exchange_weak(live, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    return Some(ConnectionSlot {
                        limit: self.clone(),
                    });
                }
                Err(now) => live = now,
            }
        }
    }
}

impl Default for ConnectionLimit {
    /// Unlimited.
    fn default() -> Self {
        Self::new(None)
    }
}

impl fmt::Debug for ConnectionLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionLimit")
            .field("max", &self.max())
            .field("live", &self.live())
            .finish()
    }
}

/// One admitted connection. Dropping it frees the slot.
#[must_use = "dropping the slot frees it at once"]
pub struct ConnectionSlot {
    limit: ConnectionLimit,
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.limit.0.live.fetch_sub(1, Ordering::AcqRel);
    }
}

impl fmt::Debug for ConnectionSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionSlot").finish_non_exhaustive()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn slots_are_returned_on_drop() {
        let limit = ConnectionLimit::new(Some(2));
        let a = limit.try_admit().unwrap();
        let b = limit.try_admit().unwrap();
        assert!(limit.try_admit().is_none());
        assert_eq!(limit.live(), 2);
        drop(a);
        let c = limit.try_admit().unwrap();
        drop((b, c));
        assert_eq!(limit.live(), 0);
    }

    #[test]
    fn the_maximum_can_change_while_running() {
        let limit = ConnectionLimit::new(Some(1));
        let a = limit.try_admit().unwrap();
        limit.set_max(Some(0));
        assert!(limit.try_admit().is_none());
        assert_eq!(limit.live(), 1, "open connections are kept");
        limit.set_max(None);
        assert_eq!(limit.max(), None);
        let more: Vec<_> = (0..100).map(|_| limit.try_admit().unwrap()).collect();
        assert_eq!(limit.live(), 101);
        drop((a, more));
        assert_eq!(limit.live(), 0);
    }

    #[test]
    fn threads_never_exceed_the_cap() {
        let limit = ConnectionLimit::new(Some(8));
        let peak = Arc::new(AtomicUsize::new(0));
        std::thread::scope(|s| {
            for _ in 0..8 {
                let (limit, peak) = (limit.clone(), Arc::clone(&peak));
                s.spawn(move || {
                    for _ in 0..1_000 {
                        if let Some(slot) = limit.try_admit() {
                            peak.fetch_max(limit.live(), Ordering::Relaxed);
                            drop(slot);
                        }
                    }
                });
            }
        });
        assert!(peak.load(Ordering::Relaxed) <= 8);
        assert_eq!(limit.live(), 0);
    }
}
