//! Fastresume files read back from disk (possibly corrupted or tampered).
#![no_main]

use dravity_storage::ResumeData;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(resume) = ResumeData::decode(data) {
        assert_eq!(
            u32::try_from(resume.have.len()).ok(),
            Some(resume.piece_count.div_ceil(8))
        );
        assert_eq!(ResumeData::decode(&resume.encode()).ok(), Some(resume));
    }
});
