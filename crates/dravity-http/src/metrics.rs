//! Prometheus text exposition (format 0.0.4) of session numbers.
//!
//! Written by hand rather than through a metrics registry: every value is
//! read from the session when scraped, so there is no state to keep in
//! sync and no global recorder to install.

use std::fmt::Write as _;
use std::future::Future;
use std::time::Duration;

use dravity_engine::Stats;
use dravity_session::{Session, TorrentSummary};
use futures_util::stream::{FuturesUnordered, StreamExt};

use crate::dto;

/// `Content-Type` of the exposition.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// How long a scrape waits for the torrents, altogether.
const SCRAPE_TIMEOUT: Duration = Duration::from_secs(2);

/// One torrent's numbers for a scrape.
#[derive(Debug, Clone)]
pub struct TorrentSample {
    /// What the session knows without asking the torrent.
    pub summary: TorrentSummary,
    /// Live numbers; `None` unless running (and answering).
    pub stats: Option<Stats>,
}

/// Collects a sample of every torrent of `session`.
///
/// The torrents are asked at the same time, and the wait ends at
/// `SCRAPE_TIMEOUT` (two seconds). A torrent that has not answered is still listed,
/// without live numbers, so one stuck task cannot stretch the scrape or
/// hide the others.
pub async fn collect(session: &Session) -> Vec<TorrentSample> {
    let mut samples: Vec<_> = session
        .list()
        .into_iter()
        .map(|summary| TorrentSample {
            summary,
            stats: None,
        })
        .collect();
    let pending = FuturesUnordered::new();
    for (index, sample) in samples.iter().enumerate() {
        let Some(handle) = session.torrent(sample.summary.info_hash) else {
            continue;
        };
        pending.push(async move {
            let stats = handle.stats().await.ok();
            (index, stats)
        });
    }
    for (index, stats) in take_ready(pending, SCRAPE_TIMEOUT).await {
        if let Some(sample) = samples.get_mut(index) {
            sample.stats = stats;
        }
    }
    samples.sort_by_key(|s| s.summary.info_hash);
    samples
}

/// Items that finish within `timeout`, in completion order. The rest are
/// dropped when the deadline passes.
async fn take_ready<F>(mut pending: FuturesUnordered<F>, timeout: Duration) -> Vec<F::Output>
where
    F: Future,
{
    let mut ready = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    while !pending.is_empty() {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        match tokio::time::timeout(left, pending.next()).await {
            Ok(Some(item)) => ready.push(item),
            _ => break,
        }
    }
    ready
}

/// Renders the exposition.
#[must_use]
pub fn render(samples: &[TorrentSample]) -> String {
    let mut out = String::new();
    let count = |state: &str| {
        samples
            .iter()
            .filter(|s| dto::state(&s.summary) == state)
            .count()
    };
    header(
        &mut out,
        "dravity_torrents",
        "gauge",
        "Torrents in the session, by state.",
    );
    for state in ["running", "paused", "stopped"] {
        let _ = writeln!(
            out,
            "dravity_torrents{{state=\"{state}\"}} {}",
            count(state)
        );
    }

    header(
        &mut out,
        "dravity_torrent_info",
        "gauge",
        "Torrent names and states (always 1).",
    );
    for s in samples {
        let _ = writeln!(
            out,
            "dravity_torrent_info{{info_hash=\"{}\",name=\"{}\",state=\"{}\"}} 1",
            s.summary.info_hash.to_hex(),
            escape(s.summary.name.as_deref().unwrap_or("")),
            dto::state(&s.summary),
        );
    }

    let series: [Series; 9] = [
        (
            "dravity_torrent_size_bytes",
            "gauge",
            "Content size.",
            |s| s.total_length,
        ),
        (
            "dravity_torrent_have_bytes",
            "gauge",
            "Verified content bytes.",
            |s| s.bytes_have,
        ),
        (
            "dravity_torrent_wanted_left_bytes",
            "gauge",
            "Bytes of wanted pieces still missing.",
            |s| s.wanted_left,
        ),
        (
            "dravity_torrent_downloaded_bytes_total",
            "counter",
            "Payload bytes received since the torrent started.",
            |s| s.downloaded,
        ),
        (
            "dravity_torrent_uploaded_bytes_total",
            "counter",
            "Payload bytes sent since the torrent started.",
            |s| s.uploaded,
        ),
        (
            "dravity_torrent_pieces_have",
            "gauge",
            "Verified pieces.",
            |s| u64::from(s.pieces_have),
        ),
        ("dravity_torrent_peers", "gauge", "Connected peers.", |s| {
            as_u64(s.peers)
        }),
        (
            "dravity_torrent_unchoked_peers",
            "gauge",
            "Peers we upload to.",
            |s| as_u64(s.unchoked),
        ),
        (
            "dravity_torrent_banned_peers",
            "gauge",
            "Addresses banned for bad data.",
            |s| as_u64(s.banned),
        ),
    ];
    for (name, kind, help, value) in series {
        header(&mut out, name, kind, help);
        for s in samples {
            if let Some(stats) = &s.stats {
                let _ = writeln!(
                    out,
                    "{name}{{info_hash=\"{}\"}} {}",
                    s.summary.info_hash.to_hex(),
                    value(stats)
                );
            }
        }
    }
    out
}

/// Name, type, help and value of a per-torrent series.
type Series = (&'static str, &'static str, &'static str, fn(&Stats) -> u64);

fn as_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

fn header(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

/// Escapes a label value: backslash, double quote and newline.
#[must_use]
pub fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::stream::FuturesUnordered;

    use super::{escape, render, take_ready};

    #[test]
    fn label_values_are_escaped() {
        assert_eq!(escape(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(escape("line\nbreak"), "line\\nbreak");
        assert_eq!(escape("plain ünïcode"), "plain ünïcode");
    }

    #[test]
    fn an_empty_session_still_reports_every_family() {
        let text = render(&[]);
        assert!(text.contains("dravity_torrents{state=\"running\"} 0\n"));
        assert!(text.contains("# TYPE dravity_torrent_downloaded_bytes_total counter\n"));
        // Every line is a comment or a sample, and ends with a newline.
        assert!(text.ends_with('\n'));
        for line in text.lines() {
            assert!(
                line.starts_with("# ") || line.starts_with("dravity_"),
                "{line}"
            );
        }
    }

    #[tokio::test]
    async fn a_slow_answer_cannot_stretch_the_deadline() {
        let pending = FuturesUnordered::new();
        pending.push(answer(1, Duration::ZERO));
        pending.push(answer(2, Duration::from_millis(20)));
        pending.push(answer(3, Duration::from_secs(60)));
        let started = std::time::Instant::now();
        let mut got = take_ready(pending, Duration::from_secs(1)).await;
        got.sort_unstable();
        assert_eq!(got, vec![1, 2]);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    async fn answer(n: u32, wait: Duration) -> u32 {
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        n
    }
}
