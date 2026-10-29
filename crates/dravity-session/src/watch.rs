//! Watch folder: torrents dropped into a directory are added.
//!
//! [`Session::watch`](crate::Session::watch) polls a directory (polling is
//! portable and needs no platform notification API) for `*.torrent` files
//! and `*.magnet` files (a text file whose first line is a magnet link).
//! A file is only read once it has looked the same (length and
//! modification time) on two consecutive scans, so a file still being
//! copied is not read half-written. Afterwards it is renamed:
//!
//! * `name.torrent.added` — added, or already in the session;
//! * `name.torrent.invalid` — not a torrent or magnet link (see the log).

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use dravity_core::{Magnet, Metainfo};
use tokio::task::JoinHandle;
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::persist::read_bounded;
use crate::{AddTorrentOptions, Session, SessionError};

/// Largest file read from the watch folder.
const MAX_WATCHED_FILE: u64 = 64 * 1024 * 1024;

/// What to watch and how to add what appears.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct WatchConfig {
    /// Directory to watch.
    pub dir: PathBuf,
    /// Download directory for torrents added from it.
    pub download_dir: PathBuf,
    /// Time between scans.
    pub interval: Duration,
    /// Add torrents paused.
    pub paused: bool,
}

impl WatchConfig {
    /// Watches `dir`, downloading into `download_dir`, every two seconds.
    pub fn new(dir: impl Into<PathBuf>, download_dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            download_dir: download_dir.into(),
            interval: Duration::from_secs(2),
            paused: false,
        }
    }
}

/// A running watch; dropping it stops the watch.
#[derive(Debug)]
pub struct WatchHandle {
    stop: CancellationToken,
    _guard: DropGuard,
    task: JoinHandle<()>,
}

impl WatchHandle {
    /// Stops watching and waits for a scan in progress to end.
    pub async fn stop(self) {
        self.stop.cancel();
        let _ = self.task.await;
    }
}

pub(crate) fn start(session: Session, config: WatchConfig) -> WatchHandle {
    let stop = CancellationToken::new();
    let task = tokio::spawn(run(session, config, stop.clone()));
    WatchHandle {
        _guard: stop.clone().drop_guard(),
        stop,
        task,
    }
}

/// What a candidate file looked like on the last scan.
type Seen = HashMap<PathBuf, (u64, Option<SystemTime>)>;

async fn run(session: Session, config: WatchConfig, stop: CancellationToken) {
    let mut seen = Seen::new();
    let mut tick = tokio::time::interval(config.interval.max(Duration::from_millis(10)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            _ = tick.tick() => {}
        }
        let dir = config.dir.clone();
        let previous = std::mem::take(&mut seen);
        let scanned = tokio::task::spawn_blocking(move || scan(&dir, previous)).await;
        let (ready, now) = match scanned {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => {
                tracing::debug!(dir = %config.dir.display(), error = %e, "cannot scan watch folder");
                continue;
            }
            Err(_) => continue,
        };
        seen = now;
        for path in ready {
            if stop.is_cancelled() {
                return;
            }
            add_file(&session, &config, path).await;
        }
    }
}

/// Lists candidate files; returns those unchanged since the previous scan
/// (ready to read) and what every candidate looks like now.
fn scan(dir: &Path, previous: Seen) -> io::Result<(Vec<PathBuf>, Seen)> {
    let mut ready = Vec::new();
    let mut now = Seen::new();
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if kind(&path).is_none() {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let stamp = (meta.len(), meta.modified().ok());
        if previous.get(&path) == Some(&stamp) {
            ready.push(path);
        } else {
            now.insert(path, stamp);
        }
    }
    ready.sort();
    Ok((ready, now))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Torrent,
    Magnet,
}

fn kind(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "torrent" => Some(Kind::Torrent),
        "magnet" => Some(Kind::Magnet),
        _ => None,
    }
}

async fn add_file(session: &Session, config: &WatchConfig, path: PathBuf) {
    let Some(kind) = kind(&path) else { return };
    let read_path = path.clone();
    let bytes =
        match tokio::task::spawn_blocking(move || read_bounded(&read_path, MAX_WATCHED_FILE)).await
        {
            Ok(Ok(bytes)) => bytes,
            // Gone or unreadable: try again on a later scan.
            Ok(Err(_)) | Err(_) => return,
        };
    let options = AddTorrentOptions::new(&config.download_dir).paused(config.paused);
    let result = match kind {
        Kind::Torrent => Metainfo::from_bytes(&bytes)
            .map_err(|e| Some(e.to_string()))
            .and_then(|m| session.add_torrent(m, options).map_err(added_error)),
        Kind::Magnet => std::str::from_utf8(&bytes)
            .map_err(|e| Some(e.to_string()))
            .and_then(|text| {
                let line = text.lines().map(str::trim).find(|l| !l.is_empty());
                Magnet::parse(line.unwrap_or_default()).map_err(|e| Some(e.to_string()))
            })
            .and_then(|m| session.add_magnet(&m, options).map_err(added_error)),
    };
    let suffix = match result {
        Ok(handle) => {
            tracing::info!(file = %path.display(), info_hash = %handle.info_hash(), "added from watch folder");
            "added"
        }
        Err(None) => "added",
        Err(Some(e)) => {
            tracing::warn!(file = %path.display(), error = %e, "watch folder file not added");
            "invalid"
        }
    };
    let mut target = path.as_os_str().to_owned();
    target.push(".");
    target.push(suffix);
    let target = PathBuf::from(target);
    let renamed = tokio::task::spawn_blocking(move || {
        let _ = std::fs::remove_file(&target);
        std::fs::rename(&path, &target)
    })
    .await;
    if let Ok(Err(e)) = renamed {
        tracing::warn!(error = %e, "cannot rename watch folder file");
    }
}

/// `None` for a torrent already in the session (not an error here).
fn added_error(e: SessionError) -> Option<String> {
    match e {
        SessionError::Duplicate(_) => None,
        e => Some(e.to_string()),
    }
}
