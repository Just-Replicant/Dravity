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

/// Share ratio `uploaded / downloaded` with two decimals, or `—` when
/// nothing was downloaded.
#[must_use]
pub(crate) fn ratio(uploaded: u64, downloaded: u64) -> String {
    if downloaded == 0 {
        return "—".to_owned();
    }
    let hundredths =
        u64::try_from(u128::from(uploaded) * 100 / u128::from(downloaded)).unwrap_or(u64::MAX);
    format!("{}.{:02}", hundredths / 100, hundredths % 100)
}

/// Time left for `left` bytes at `rate` bytes per second (`1h02m`,
/// `3m05s`, `42s`, `>99d`), or `—` when the rate is 0.
#[must_use]
pub(crate) fn eta(left: u64, rate: u64) -> String {
    if rate == 0 {
        return "—".to_owned();
    }
    let secs = left.div_ceil(rate);
    let (d, h, m, s) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    if d > 99 {
        ">99d".to_owned()
    } else if d > 0 {
        format!("{d}d{h:02}h")
    } else if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// A short name for an engine state.
pub(crate) fn status(s: TorrentStatus) -> &'static str {
    match s {
        TorrentStatus::Idle => "idle",
        TorrentStatus::FetchingMetadata => "fetching metadata",
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
    if stats.status == TorrentStatus::FetchingMetadata {
        // Sizes are unknown until the metadata arrives.
        return format!("{} peers {}", status(stats.status), stats.peers);
    }
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

/// The status line while seeding: what we have uploaded and how fast.
pub(crate) fn seeding_line(stats: &Stats, up_rate: u64) -> String {
    format!(
        "seeding {} up {}/s peers {} unchoked {}",
        bytes(stats.uploaded),
        bytes(up_rate),
        stats.peers,
        stats.unchoked,
    )
}

/// Parses a byte rate: a plain number, or one with a binary `K`, `M` or
/// `G` suffix (`500K`, `1.5M`). A trailing `B`, `iB` or `/s` is accepted.
pub(crate) fn parse_rate(s: &str) -> Result<u64, String> {
    let lower = s.trim().to_ascii_lowercase();
    let mut t = lower.as_str();
    t = t.strip_suffix("/s").unwrap_or(t);
    t = t
        .strip_suffix("ib")
        .or_else(|| t.strip_suffix('b'))
        .unwrap_or(t);
    let (number, scale) = match t.char_indices().last() {
        Some((i, 'k')) => (t.get(..i), 1u64 << 10),
        Some((i, 'm')) => (t.get(..i), 1 << 20),
        Some((i, 'g')) => (t.get(..i), 1 << 30),
        _ => (Some(t), 1),
    };
    let number = number.unwrap_or_default().trim();
    let invalid = || format!("invalid rate `{s}` (examples: 65536, 500K, 2M)");
    if let Ok(n) = number.parse::<u64>() {
        return n.checked_mul(scale).ok_or_else(invalid);
    }
    // One decimal place at most: 1.5M.
    let (whole, frac) = number.split_once('.').ok_or_else(invalid)?;
    let whole: u64 = whole.parse().map_err(|_| invalid())?;
    let tenths: u64 = match frac {
        d if d.len() == 1 => d.parse().map_err(|_| invalid())?,
        _ => return Err(invalid()),
    };
    whole
        .checked_mul(scale)
        .and_then(|w| w.checked_add(tenths * scale / 10))
        .ok_or_else(invalid)
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

    #[test]
    fn ratio_and_eta() {
        assert_eq!(ratio(0, 0), "—");
        assert_eq!(ratio(5, 0), "—");
        assert_eq!(ratio(0, 10), "0.00");
        assert_eq!(ratio(3, 2), "1.50");
        assert_eq!(ratio(1, 3), "0.33");
        // Saturates instead of overflowing.
        assert!(ratio(u64::MAX, 1).starts_with(&(u64::MAX / 100).to_string()));
        assert_eq!(eta(100, 0), "—");
        assert_eq!(eta(0, 10), "0s");
        assert_eq!(eta(41, 1), "41s");
        assert_eq!(eta(185, 1), "3m05s");
        assert_eq!(eta(3720, 1), "1h02m");
        assert_eq!(eta(90_000, 1), "1d01h");
        assert_eq!(eta(u64::MAX, 1), ">99d");
    }

    #[test]
    fn rates() {
        assert_eq!(parse_rate("65536"), Ok(65536));
        assert_eq!(parse_rate("500K"), Ok(500 * 1024));
        assert_eq!(parse_rate("2m"), Ok(2 * 1024 * 1024));
        assert_eq!(parse_rate("1.5MiB/s"), Ok(1536 * 1024));
        assert_eq!(parse_rate("1G"), Ok(1 << 30));
        assert_eq!(parse_rate("10kb"), Ok(10 * 1024));
        assert!(parse_rate("").is_err());
        assert!(parse_rate("fast").is_err());
        assert!(parse_rate("1.25M").is_err());
        assert!(parse_rate("-1").is_err());
        assert!(parse_rate("99999999999999999999G").is_err());
    }

    #[test]
    fn seeding_shows_uploads() {
        let state = dravity_engine::TorrentState::new(
            dravity_core::PieceLayout::new(16_384, 16_384).unwrap(),
            dravity_engine::Config::default(),
        );
        assert_eq!(
            seeding_line(&state.stats(), 2048),
            "seeding 0 B up 2.0 KiB/s peers 0 unchoked 0"
        );
    }

    #[test]
    fn metadata_progress_has_no_sizes() {
        let mut config = dravity_engine::Config::default();
        config.dht = false;
        let mut state =
            dravity_engine::MetadataState::new(dravity_core::InfoHash::new([1; 20]), config);
        let _ = state.update(std::time::Instant::now(), dravity_engine::Event::Start);
        assert_eq!(state.status(), TorrentStatus::FetchingMetadata);
        assert_eq!(
            progress_line(&state.stats(), 0),
            "fetching metadata peers 0"
        );
    }
}
