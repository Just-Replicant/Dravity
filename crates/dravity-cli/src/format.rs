//! Human-readable formatting for progress output.

use dravity_engine::{Stats, TorrentStatus};

/// Formats a byte count with binary units (`1.5 MiB`).
#[must_use]
pub(crate) fn bytes(n: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let mut value = n;
    let mut unit = 0;
    let mut rem = 0;
    while value >= 1024 && unit + 1 < UNITS.len() {
        rem = value % 1024;
        value /= 1024;
        unit += 1;
    }
    let name = UNITS.get(unit).copied().unwrap_or("B");
    if unit == 0 {
        format!("{value} {name}")
    } else {
        // One decimal, rounded down: 1536 -> 1.5.
        format!("{value}.{} {name}", rem * 10 / 1024)
    }
}

/// Completion in tenths of a percent (0..=1000).
#[must_use]
pub(crate) fn permille(have: u64, total: u64) -> u64 {
    if total == 0 {
        1000
    } else {
        u64::try_from(u128::from(have) * 1000 / u128::from(total)).unwrap_or(1000)
    }
}

/// A fixed-width bar: `[#####.....]`.
#[must_use]
pub(crate) fn bar(permille: u64, width: usize) -> String {
    let w = u64::try_from(width).unwrap_or(0);
    let filled = usize::try_from(permille.min(1000) * w / 1000).unwrap_or(width);
    format!(
        "[{}{}]",
        "#".repeat(filled),
        ".".repeat(width.saturating_sub(filled))
    )
}

fn status(s: TorrentStatus) -> &'static str {
    match s {
        TorrentStatus::Idle => "idle",
        TorrentStatus::Checking => "checking",
        TorrentStatus::Downloading => "downloading",
        TorrentStatus::Finished => "finished",
        TorrentStatus::Stopped => "stopped",
        _ => "unknown",
    }
}

/// One progress line, given the download rate in bytes per second.
#[must_use]
pub(crate) fn progress_line(stats: &Stats, rate: u64) -> String {
    let p = permille(stats.bytes_have, stats.total_length);
    format!(
        "{} {:>3}.{}% {}/{} {}/s peers {} {}",
        bar(p, 30),
        p / 10,
        p % 10,
        bytes(stats.bytes_have),
        bytes(stats.total_length),
        bytes(rate),
        stats.peers,
        status(stats.status),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_bytes() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(bytes(u64::MAX), "15.9 EiB");
    }

    #[test]
    fn progress() {
        assert_eq!(permille(1, 3), 333);
        assert_eq!(permille(0, 0), 1000);
        assert_eq!(permille(u64::MAX, u64::MAX), 1000);
        assert_eq!(bar(500, 10), "[#####.....]");
        assert_eq!(bar(2000, 4), "[####]");
    }
}
