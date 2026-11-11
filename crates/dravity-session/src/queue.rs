//! The torrent queue: at most [`LiveSettings::max_active_downloads`]
//! torrents download and [`LiveSettings::max_active_seeds`] seed at once.
//!
//! Torrents keep their place by when they were added. A queued torrent's
//! task is stopped (like a pause, but not the user's: it is not persisted
//! as paused and the queue starts it again when a slot frees up). Paused
//! torrents and torrents that stopped on an error take no slot.
//!
//! Whether a torrent seeds is learned from its running task; a queued
//! torrent counts as whatever it was last seen as (a download until then).

use std::sync::{Arc, Weak};
use std::time::Duration;

use dravity_core::InfoHash;
use dravity_engine::TorrentStatus;

use crate::settings::LiveSettings;
use crate::{Inner, Session, TorrentHandle};

/// What the queue decides for one torrent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Leave it as it is.
    Keep,
    /// Start its task.
    Start,
    /// Stop its task and queue it.
    Queue,
}

/// One torrent as the queue sees it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Slot {
    pub(crate) running: bool,
    pub(crate) queued: bool,
    pub(crate) complete: bool,
}

/// Decides for `slots`, oldest first (pure, for testing).
pub(crate) fn plan(settings: &LiveSettings, slots: &[Slot]) -> Vec<Decision> {
    let mut downloads = settings.max_active_downloads.unwrap_or(usize::MAX);
    let mut seeds = settings.max_active_seeds.unwrap_or(usize::MAX);
    slots
        .iter()
        .map(|s| {
            if !s.running && !s.queued {
                return Decision::Keep; // paused or failed: no slot
            }
            let free = if s.complete {
                &mut seeds
            } else {
                &mut downloads
            };
            if *free > 0 {
                *free -= 1;
                if s.running {
                    Decision::Keep
                } else {
                    Decision::Start
                }
            } else if s.running {
                Decision::Queue
            } else {
                Decision::Keep
            }
        })
        .collect()
}

/// Runs the queue until the session is gone.
pub(crate) async fn run(inner: Weak<Inner>, every: Duration) {
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let Some(inner) = inner.upgrade() else {
            return;
        };
        if inner.stop.is_cancelled() {
            return;
        }
        step(&Session { inner }).await;
    }
}

/// One round: learn which running torrents seed, then start and stop.
async fn step(session: &Session) {
    let settings = session.settings();
    // Learn completion from running tasks.
    let running: Vec<(InfoHash, TorrentHandle)> = session
        .inner
        .catalog()
        .iter()
        .filter(|(_, e)| e.is_running())
        .filter_map(|(h, e)| {
            e.commands.clone().map(|commands| {
                (
                    *h,
                    TorrentHandle {
                        info_hash: *h,
                        commands,
                        session: Arc::downgrade(&session.inner),
                    },
                )
            })
        })
        .collect();
    let mut seen = Vec::with_capacity(running.len());
    for (hash, handle) in running {
        if let Ok(Ok(stats)) = tokio::time::timeout(Duration::from_secs(1), handle.stats()).await {
            seen.push((hash, stats.status == TorrentStatus::Finished));
        }
    }
    let mut stop = Vec::new();
    {
        let mut catalog = session.inner.catalog();
        for (hash, complete) in seen {
            if let Some(e) = catalog.get_mut(&hash) {
                e.complete = complete;
            }
        }
        let mut order: Vec<(u64, InfoHash)> = catalog
            .iter()
            .filter(|(_, e)| !e.paused)
            .map(|(h, e)| (e.added_at, *h))
            .collect();
        order.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.as_bytes().cmp(b.1.as_bytes()))
        });
        let slots: Vec<Slot> = order
            .iter()
            .filter_map(|(_, h)| catalog.get(h))
            .map(|e| Slot {
                running: e.is_running(),
                queued: e.queued,
                complete: e.complete,
            })
            .collect();
        let decisions = if settings.queues() {
            plan(&settings, &slots)
        } else {
            // No limits: everything queued starts.
            slots
                .iter()
                .map(|s| {
                    if s.queued && !s.running {
                        Decision::Start
                    } else {
                        Decision::Keep
                    }
                })
                .collect()
        };
        for ((_, hash), decision) in order.iter().zip(decisions) {
            let Some(entry) = catalog.get_mut(hash) else {
                continue;
            };
            match decision {
                Decision::Keep => {}
                Decision::Start => {
                    entry.queued = false;
                    if let Err(e) = session.spawn(*hash, entry) {
                        tracing::warn!(info_hash = %hash, error = %e, "queued torrent failed to start");
                    }
                }
                Decision::Queue => {
                    entry.queued = true;
                    if let Some(commands) = entry.commands.take() {
                        stop.push(commands);
                    }
                }
            }
        }
    }
    for commands in stop {
        crate::stop_task(&commands).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn slot(running: bool, queued: bool, complete: bool) -> Slot {
        Slot {
            running,
            queued,
            complete,
        }
    }

    #[test]
    fn the_oldest_torrents_get_the_slots() {
        let settings = LiveSettings {
            max_active_downloads: Some(1),
            max_active_seeds: Some(1),
            ..LiveSettings::default()
        };
        let slots = [
            slot(false, false, false), // paused: ignored
            slot(true, false, false),  // download 1: keeps its slot
            slot(true, false, false),  // download 2: queued
            slot(false, true, false),  // queued download: stays queued
            slot(false, true, true),   // queued seed: starts
            slot(true, false, true),   // second seed: queued
        ];
        assert_eq!(
            plan(&settings, &slots),
            [
                Decision::Keep,
                Decision::Keep,
                Decision::Queue,
                Decision::Keep,
                Decision::Start,
                Decision::Queue
            ]
        );
        // Zero means everything waits.
        let none = LiveSettings {
            max_active_downloads: Some(0),
            ..LiveSettings::default()
        };
        assert_eq!(plan(&none, &[slot(true, false, false)]), [Decision::Queue]);
    }
}
