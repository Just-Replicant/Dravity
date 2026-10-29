//! Session state records read back from the state directory.
#![no_main]

use dravity_session::persist::TorrentRecord;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(record) = TorrentRecord::decode(data) {
        assert_eq!(TorrentRecord::decode(&record.encode()).ok(), Some(record));
    }
});
