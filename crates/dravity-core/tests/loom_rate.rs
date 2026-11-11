//! Exhaustive interleavings of the shared token bucket
//! (`dravity_core::rate::SharedBucket`).
//!
//! Run with:
//! `RUSTFLAGS="--cfg loom" cargo test -p dravity-core --test loom_rate --release`

#![cfg(loom)]
#![allow(clippy::unwrap_used)]

use std::time::{Duration, Instant};

use dravity_core::rate::SharedBucket;
use loom::thread;

/// Two connections take tokens at the same instant: no tokens are lost or
/// double-spent, and they queue one behind the other.
#[test]
fn concurrent_takes_conserve_tokens_and_queue() {
    loom::model(|| {
        let now = Instant::now();
        let bucket = SharedBucket::new(Some(1000), now);
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let bucket = bucket.clone();
                thread::spawn(move || bucket.take(500, now))
            })
            .collect();
        let mut waits: Vec<Duration> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        waits.sort();
        assert_eq!(
            waits,
            [Duration::from_millis(500), Duration::from_millis(1000)]
        );
        assert!((bucket.tokens() + 1000.0).abs() < 1e-9);
    });
}

/// A rate change racing a take: afterwards the new rate is in force and
/// the debt was neither forgiven twice nor lost.
#[test]
fn a_rate_change_racing_a_take_is_atomic() {
    loom::model(|| {
        let now = Instant::now();
        let bucket = SharedBucket::new(Some(1000), now);
        let taker = {
            let bucket = bucket.clone();
            thread::spawn(move || bucket.take(400, now))
        };
        let setter = {
            let bucket = bucket.clone();
            thread::spawn(move || bucket.set_rate(Some(2000), now))
        };
        let wait = taker.join().unwrap();
        setter.join().unwrap();
        assert_eq!(bucket.rate(), Some(2000));
        // Either order leaves exactly 400 bytes of debt.
        assert!((bucket.tokens() + 400.0).abs() < 1e-9);
        // Priced at whichever rate was in force when it took the lock.
        assert!(
            wait == Duration::from_millis(400) || wait == Duration::from_millis(200),
            "{wait:?}"
        );
    });
}
