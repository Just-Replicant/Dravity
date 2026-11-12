//! Local Service Discovery (BEP 14) for the session's torrents.
//!
//! One task per session announces every running, public torrent whose
//! metadata is known, again each [`LsdConfig::interval`], and hands the
//! peers it hears about to the matching torrent. Private torrents (BEP 27)
//! are never announced, and discoveries for them are dropped. Magnets
//! wait for their metadata, which says whether they are private.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use dravity_core::InfoHash;
use dravity_lsd::{Discovered, Lsd, LsdConfig};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::catalog::Catalog;
use crate::torrent::Command;

/// How often the task looks for torrents due an announce.
const CHECK_EVERY: Duration = Duration::from_secs(5);

/// The torrents LSD may announce: running, metadata known, not private.
fn public_running(catalog: &Catalog) -> HashSet<InfoHash> {
    catalog
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|(_, e)| e.is_running() && e.metainfo().is_some_and(|m| !m.info().is_private()))
        .map(|(h, _)| *h)
        .collect()
}

/// Starts LSD for a session listening on `port`. Failing to bind is
/// logged, not fatal.
pub(crate) fn start(config: &LsdConfig, port: u16, catalog: Catalog, stop: CancellationToken) {
    match Lsd::start(config, port) {
        Ok((lsd, found)) => {
            tokio::spawn(run(lsd, found, catalog, config.interval, stop));
        }
        Err(e) => tracing::warn!(error = %e, "local service discovery unavailable"),
    }
}

async fn run(
    lsd: Lsd,
    mut found: mpsc::Receiver<Discovered>,
    catalog: Catalog,
    interval: Duration,
    stop: CancellationToken,
) {
    let mut last: HashMap<InfoHash, Instant> = HashMap::new();
    let mut check = tokio::time::interval(CHECK_EVERY.min(interval.max(Duration::from_millis(10))));
    check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            _ = check.tick() => {
                let eligible = public_running(&catalog);
                last.retain(|h, _| eligible.contains(h));
                let now = Instant::now();
                let due: Vec<InfoHash> = eligible
                    .into_iter()
                    .filter(|h| last.get(h).is_none_or(|&t| now.duration_since(t) >= interval))
                    .collect();
                for h in &due {
                    last.insert(*h, now);
                }
                lsd.announce(due);
            }
            Some(d) = found.recv() => {
                let commands = catalog
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&d.info_hash)
                    .filter(|e| e.metainfo().is_some_and(|m| !m.info().is_private()))
                    .and_then(|e| e.commands.clone());
                if let Some(tx) = commands {
                    tracing::debug!(peer = %d.peer, info_hash = %d.info_hash, "LSD peer");
                    let _ = tx.try_send(Command::AddPeers(vec![d.peer]));
                }
            }
        }
    }
}
