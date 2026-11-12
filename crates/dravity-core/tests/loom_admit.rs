//! Exhaustive interleavings of connection admission (`dravity_core::admit`).
//!
//! Run with:
//! `RUSTFLAGS="--cfg loom" cargo test -p dravity-core --test loom_admit --release`

#![cfg(loom)]
#![allow(clippy::unwrap_used)]

use dravity_core::admit::ConnectionLimit;
use loom::sync::Arc;
use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::thread;

/// Two threads race for one slot: whoever holds it is alone.
#[test]
fn one_slot_has_one_holder_at_a_time() {
    loom::model(|| {
        let limit = ConnectionLimit::new(Some(1));
        let wins = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let (limit, wins) = (limit.clone(), Arc::clone(&wins));
                thread::spawn(move || {
                    if let Some(slot) = limit.try_admit() {
                        wins.fetch_add(1, Ordering::Relaxed);
                        assert_eq!(limit.live(), 1, "two holders at once");
                        drop(slot);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let wins = wins.load(Ordering::Relaxed);
        assert!((1..=2).contains(&wins), "{wins}");
        assert_eq!(limit.live(), 0);
    });
}

/// A holder and two contenders: the count never exceeds the cap, and every
/// slot is returned.
#[test]
fn the_cap_holds_while_slots_are_released() {
    loom::model(|| {
        let limit = ConnectionLimit::new(Some(1));
        let held = limit.try_admit().unwrap();
        let contender = {
            let limit = limit.clone();
            thread::spawn(move || {
                let slot = limit.try_admit();
                assert!(limit.live() <= 1);
                slot.is_some()
            })
        };
        let releaser = thread::spawn(move || drop(held));
        let got = contender.join().unwrap();
        releaser.join().unwrap();
        assert!(limit.live() <= 1);
        if !got {
            assert!(limit.try_admit().is_some(), "the slot came back");
        }
    });
}

/// Lowering the cap races an admission: once the change is visible, no
/// new slot is handed out, and the count is never above the larger cap.
#[test]
fn lowering_the_cap_races_an_admit() {
    loom::model(|| {
        let limit = ConnectionLimit::new(Some(2));
        let admitter = {
            let limit = limit.clone();
            thread::spawn(move || {
                let slot = limit.try_admit();
                assert!(limit.live() <= 2);
                slot
            })
        };
        let setter = {
            let limit = limit.clone();
            thread::spawn(move || limit.set_max(Some(0)))
        };
        setter.join().unwrap();
        // The setter has finished: every later admit sees the new cap.
        assert!(limit.try_admit().is_none());
        let slot = admitter.join().unwrap();
        assert_eq!(limit.live(), usize::from(slot.is_some()));
        drop(slot);
        assert_eq!(limit.live(), 0);
        assert_eq!(limit.max(), Some(0));
    });
}
