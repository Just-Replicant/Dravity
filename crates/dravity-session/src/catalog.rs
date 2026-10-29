//! Every torrent the session holds, running or paused.
//!
//! The listener's registry only maps info-hashes of running torrents to
//! their command queues. The catalog remembers how each torrent was added
//! (source, directory, peers, storage, file priorities) so it can be paused,
//! resumed, persisted and listed.
//!
//! Lock order: the catalog before the registry, never the reverse.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use dravity_core::{InfoHash, Magnet, Metainfo};
use dravity_engine::Priority;
use dravity_storage::Storage;
use tokio::sync::mpsc;

use crate::persist::TorrentRecord;
use crate::torrent::Command;

/// The catalog, shared with torrent tasks (which record metadata arrival).
pub(crate) type Catalog = Arc<Mutex<HashMap<InfoHash, Entry>>>;

/// Where a torrent's metadata comes from.
#[derive(Debug, Clone)]
pub(crate) enum EntrySource {
    Torrent(Arc<Metainfo>),
    Magnet(Box<Magnet>),
}

/// One torrent of the session.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub(crate) source: EntrySource,
    pub(crate) download_dir: PathBuf,
    pub(crate) peers: Vec<SocketAddr>,
    pub(crate) storage: Option<Arc<dyn Storage>>,
    /// File priorities as last set (empty: all normal).
    pub(crate) priorities: Vec<Priority>,
    pub(crate) paused: bool,
    pub(crate) added_at: u64,
    /// The running task's queue, if one was started.
    pub(crate) commands: Option<mpsc::Sender<Command>>,
}

impl Entry {
    /// Whether a task is running for this torrent.
    pub(crate) fn is_running(&self) -> bool {
        self.commands.as_ref().is_some_and(|tx| !tx.is_closed())
    }

    /// The metainfo, once known.
    pub(crate) fn metainfo(&self) -> Option<&Arc<Metainfo>> {
        match &self.source {
            EntrySource::Torrent(m) => Some(m),
            EntrySource::Magnet(_) => None,
        }
    }

    /// The torrent's name, or the magnet's display name.
    pub(crate) fn name(&self) -> Option<String> {
        match &self.source {
            EntrySource::Torrent(m) => Some(m.info().name().to_owned()),
            EntrySource::Magnet(m) => m.display_name.clone(),
        }
    }

    /// What persistence stores for this entry.
    pub(crate) fn record(&self, info_hash: InfoHash) -> TorrentRecord {
        let mut record = TorrentRecord::new(info_hash, self.download_dir.clone());
        record.magnet = match &self.source {
            EntrySource::Magnet(m) => Some(m.to_uri()),
            EntrySource::Torrent(_) => None,
        };
        record.priorities.clone_from(&self.priorities);
        record.paused = self.paused;
        record.added_at = self.added_at;
        record
    }
}
