//! Web seed planning over any accepted torrent never panics, and a plan
//! covers exactly the range it was asked for.
#![no_main]

use dravity_core::Metainfo;
use dravity_webseed::{Segment, plan_range};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Two bytes pick the range, the rest is the torrent.
    let [a, b, rest @ ..] = data else {
        return;
    };
    let Ok(m) = Metainfo::from_bytes(rest) else {
        return;
    };
    let total = m.info().total_length();
    if total == 0 {
        return;
    }
    let start = u64::from(*a) * total / 256;
    let end = (start + u64::from(*b) + 1).min(total);
    let plan = plan_range("http://seed.example/x/", m.info(), start..end).expect("range in bounds");
    assert_eq!(plan.iter().map(Segment::len).sum::<u64>(), end - start);
});
