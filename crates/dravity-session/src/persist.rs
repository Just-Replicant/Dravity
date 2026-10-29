//! Session persistence: which torrents the session holds and how, so a
//! restart can bring them back ([`crate::Session::restore`]).
//!
//! With [`SessionConfig::state_dir`](crate::SessionConfig::state_dir) set,
//! every torrent has two files there, named after its hex info-hash:
//!
//! * `<info-hash>.torrent` — a `.torrent` file rebuilt from the verified
//!   `info` dictionary and the tracker tiers (absent for a magnet link
//!   whose metadata has not arrived yet);
//! * `<info-hash>.state` — a [`TorrentRecord`]: download directory, file
//!   priorities, whether it is paused, when it was added, and the magnet
//!   link while the metadata is unknown.
//!
//! Piece progress is not stored here; that is fastresume's job
//! ([`dravity_storage::resume`]). Files are written atomically (temporary
//! file, sync, rename) by one background task, in order, so the owner tasks
//! and callers never block on the disk.
//!
//! The `.state` format is a bencoded dictionary:
//!
//! ```text
//! d 5:addedi<unix seconds>e 12:download-dir<path> 9:info-hash20:<hash>
//!   [6:magnet<uri>] 6:pausedi<0|1>e 10:prioritiesl<i0..3e>...e 7:versioni1e e
//! ```

use std::borrow::Cow;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use dravity_bencode::{Dict, Value};
use dravity_core::{InfoHash, Metainfo};
use dravity_engine::Priority;
use tokio::sync::{mpsc, oneshot};

/// Current `.state` format version.
const VERSION: i64 = 1;

/// Largest `.state` file accepted.
pub const MAX_STATE_FILE: u64 = 1024 * 1024;

/// Largest `.torrent` file accepted from the state directory.
pub const MAX_TORRENT_FILE: u64 = 64 * 1024 * 1024;

/// Errors decoding a [`TorrentRecord`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RecordError {
    /// The file is not valid bencode.
    #[error("session state is not valid bencode: {0}")]
    Decode(#[from] dravity_bencode::Error),
    /// The bencode does not describe a torrent record.
    #[error("invalid session state: {0}")]
    Invalid(&'static str),
}

/// What the session remembers about one torrent.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TorrentRecord {
    /// The torrent.
    pub info_hash: InfoHash,
    /// Download directory.
    pub download_dir: PathBuf,
    /// The magnet link, while the metadata is not known.
    pub magnet: Option<String>,
    /// File priorities (empty: all normal).
    pub priorities: Vec<Priority>,
    /// Paused by the user: restored without starting.
    pub paused: bool,
    /// When the torrent was added, in seconds since the Unix epoch.
    pub added_at: u64,
}

impl TorrentRecord {
    /// A record for a torrent in `download_dir`, added now, not paused.
    #[must_use]
    pub fn new(info_hash: InfoHash, download_dir: impl Into<PathBuf>) -> Self {
        Self {
            info_hash,
            download_dir: download_dir.into(),
            magnet: None,
            priorities: Vec::new(),
            paused: false,
            added_at: unix_now(),
        }
    }

    /// Bencodes the record.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let key = |k: &str| Cow::Owned(k.as_bytes().to_vec());
        let mut d = Dict::new();
        d.insert(key("version"), Value::Int(VERSION));
        d.insert(
            key("info-hash"),
            Value::Bytes(Cow::Owned(self.info_hash.as_bytes().to_vec())),
        );
        d.insert(
            key("download-dir"),
            Value::Bytes(Cow::Owned(
                self.download_dir.to_string_lossy().as_bytes().to_vec(),
            )),
        );
        if let Some(magnet) = &self.magnet {
            d.insert(
                key("magnet"),
                Value::Bytes(Cow::Owned(magnet.as_bytes().to_vec())),
            );
        }
        d.insert(
            key("priorities"),
            Value::List(
                self.priorities
                    .iter()
                    .map(|p| Value::Int(priority_code(*p)))
                    .collect(),
            ),
        );
        d.insert(key("paused"), Value::Int(i64::from(self.paused)));
        d.insert(
            key("added"),
            Value::Int(i64::try_from(self.added_at).unwrap_or(i64::MAX)),
        );
        Value::Dict(d).to_vec()
    }

    /// Decodes a record.
    ///
    /// # Errors
    /// [`RecordError`] for anything that is not a valid record.
    pub fn decode(input: &[u8]) -> Result<Self, RecordError> {
        let value = Value::decode(input)?;
        let d = value
            .as_dict()
            .ok_or(RecordError::Invalid("not a dictionary"))?;
        let get = |k: &str| d.get(k.as_bytes());
        if get("version").and_then(Value::as_int) != Some(VERSION) {
            return Err(RecordError::Invalid("unsupported version"));
        }
        let hash: [u8; 20] = get("info-hash")
            .and_then(Value::as_bytes)
            .and_then(|b| b.try_into().ok())
            .ok_or(RecordError::Invalid("missing or malformed info-hash"))?;
        let download_dir = get("download-dir")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .ok_or(RecordError::Invalid("missing download-dir"))?;
        let magnet = match get("magnet") {
            None => None,
            Some(v) => Some(
                v.as_str()
                    .ok_or(RecordError::Invalid("magnet is not UTF-8"))?
                    .to_owned(),
            ),
        };
        let priorities = get("priorities")
            .and_then(Value::as_list)
            .ok_or(RecordError::Invalid("missing priorities"))?
            .iter()
            .map(|v| {
                v.as_int()
                    .and_then(priority_from_code)
                    .ok_or(RecordError::Invalid("unknown priority"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let paused = match get("paused").and_then(Value::as_int) {
            Some(0) => false,
            Some(1) => true,
            _ => return Err(RecordError::Invalid("paused is not 0 or 1")),
        };
        let added_at = get("added")
            .and_then(Value::as_int)
            .and_then(|v| u64::try_from(v).ok())
            .ok_or(RecordError::Invalid("missing or negative added"))?;
        Ok(Self {
            info_hash: InfoHash::new(hash),
            download_dir,
            magnet,
            priorities,
            paused,
            added_at,
        })
    }
}

const fn priority_code(p: Priority) -> i64 {
    match p {
        Priority::Skip => 0,
        Priority::Low => 1,
        Priority::Normal => 2,
        Priority::High => 3,
    }
}

const fn priority_from_code(code: i64) -> Option<Priority> {
    match code {
        0 => Some(Priority::Skip),
        1 => Some(Priority::Low),
        2 => Some(Priority::Normal),
        3 => Some(Priority::High),
        _ => None,
    }
}

/// Seconds since the Unix epoch (0 if the clock is before it).
pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// A `.torrent` file for `metainfo`: its original `info` bytes (so the
/// info-hash is unchanged) plus `announce`/`announce-list` from its tiers.
#[must_use]
pub fn torrent_file_bytes(metainfo: &Metainfo) -> Vec<u8> {
    let tiers = metainfo.trackers();
    let mut out = b"d".to_vec();
    if let Some(first) = tiers.iter().flatten().next() {
        out.extend_from_slice(b"8:announce");
        Value::Bytes(Cow::Borrowed(first.as_bytes())).encode(&mut out);
        out.extend_from_slice(b"13:announce-list");
        Value::List(
            tiers
                .iter()
                .map(|tier| {
                    Value::List(
                        tier.iter()
                            .map(|url| Value::Bytes(Cow::Borrowed(url.as_bytes())))
                            .collect(),
                    )
                })
                .collect(),
        )
        .encode(&mut out);
    }
    out.extend_from_slice(b"4:info");
    out.extend_from_slice(metainfo.info_bytes());
    out.push(b'e');
    out
}

/// Path of a torrent's file with `extension` in `dir`.
pub(crate) fn path_for(dir: &Path, info_hash: &InfoHash, extension: &str) -> PathBuf {
    dir.join(format!("{}.{extension}", info_hash.to_hex()))
}

/// Writes `bytes` to `path` atomically (temporary file, sync, rename).
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Removes `path`; a missing file is fine.
pub(crate) fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Reads `path` if it is a regular file no larger than `max`.
pub(crate) fn read_bounded(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.len() > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a regular file or too large",
        ));
    }
    fs::read(path)
}

/// One write for the persistence task.
#[derive(Debug)]
enum Op {
    Write(PathBuf, Vec<u8>),
    Remove(PathBuf),
    Flush(oneshot::Sender<()>),
}

/// Writes session state in a background task, in submission order.
#[derive(Debug, Clone)]
pub(crate) struct Persister {
    /// The state directory.
    dir: PathBuf,
    tx: mpsc::Sender<Op>,
}

impl Persister {
    /// Starts the writer task. Must be called within a Tokio runtime.
    pub(crate) fn start(dir: PathBuf) -> Self {
        let (tx, mut rx) = mpsc::channel::<Op>(1024);
        tokio::spawn(async move {
            while let Some(op) = rx.recv().await {
                let result = match op {
                    Op::Flush(ack) => {
                        let _ = ack.send(());
                        continue;
                    }
                    Op::Write(path, bytes) => {
                        let shown = path.clone();
                        let done = tokio::task::spawn_blocking(move || write_atomic(&path, &bytes));
                        (shown, done.await)
                    }
                    Op::Remove(path) => {
                        let shown = path.clone();
                        let done = tokio::task::spawn_blocking(move || remove_if_exists(&path));
                        (shown, done.await)
                    }
                };
                match result {
                    (_, Ok(Ok(()))) => {}
                    (path, Ok(Err(e))) => {
                        tracing::warn!(path = %path.display(), error = %e, "saving session state failed");
                    }
                    (path, Err(e)) => {
                        tracing::warn!(path = %path.display(), error = %e, "session state task failed");
                    }
                }
            }
        });
        Self { dir, tx }
    }

    fn submit(&self, op: Op) {
        if self.tx.try_send(op).is_err() {
            tracing::warn!("session state queue full; a state update was dropped");
        }
    }

    /// Saves a torrent's record.
    pub(crate) fn save_record(&self, record: &TorrentRecord) {
        let path = path_for(&self.dir, &record.info_hash, "state");
        self.submit(Op::Write(path, record.encode()));
    }

    /// Saves a torrent's metainfo.
    pub(crate) fn save_torrent(&self, metainfo: &Metainfo) {
        let path = path_for(&self.dir, &metainfo.info_hash(), "torrent");
        self.submit(Op::Write(path, torrent_file_bytes(metainfo)));
    }

    /// Forgets a torrent.
    pub(crate) fn remove(&self, info_hash: &InfoHash) {
        for extension in ["state", "torrent"] {
            self.submit(Op::Remove(path_for(&self.dir, info_hash, extension)));
        }
    }

    /// Waits until everything submitted so far is on disk.
    pub(crate) async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Op::Flush(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use dravity_core::InfoHash;
    use dravity_engine::Priority;
    use proptest::prelude::*;

    use super::{RecordError, TorrentRecord};

    fn sample() -> TorrentRecord {
        TorrentRecord {
            info_hash: InfoHash::new([7; 20]),
            download_dir: "/tmp/downloads".into(),
            magnet: Some("magnet:?xt=urn:btih:0707070707070707070707070707070707070707".into()),
            priorities: vec![Priority::Skip, Priority::High],
            paused: true,
            added_at: 1_700_000_000,
        }
    }

    #[test]
    fn roundtrip() {
        let record = sample();
        assert_eq!(TorrentRecord::decode(&record.encode()).unwrap(), record);
        let plain = TorrentRecord::new(InfoHash::new([1; 20]), "d");
        assert_eq!(TorrentRecord::decode(&plain.encode()).unwrap(), plain);
    }

    #[test]
    fn rejects_bad_records() {
        let good = String::from_utf8(sample().encode()).unwrap();
        for (from, to) in [
            ("7:versioni1e", "7:versioni2e"),
            ("6:pausedi1e", "6:pausedi7e"),
            ("i3e", "i4e"),
            ("5:addedi1700000000e", "5:addedi-1e"),
            ("12:download-dir14:/tmp/downloads", "12:download-dir0:"),
        ] {
            let bad = good.replace(from, to);
            assert_ne!(bad, good, "{from}");
            assert!(
                matches!(
                    TorrentRecord::decode(bad.as_bytes()),
                    Err(RecordError::Invalid(_))
                ),
                "{from} -> {to}"
            );
        }
        assert!(matches!(
            TorrentRecord::decode(b"i1e"),
            Err(RecordError::Invalid(_))
        ));
        assert!(matches!(
            TorrentRecord::decode(b"d"),
            Err(RecordError::Decode(_))
        ));
    }

    proptest! {
        #[test]
        fn records_roundtrip(
            hash in any::<[u8; 20]>(),
            dir in "[a-z/]{1,20}",
            magnet in proptest::option::of("[ -~]{0,40}"),
            prios in proptest::collection::vec(0usize..4, 0..20),
            paused in any::<bool>(),
            added in 0u64..=i64::MAX as u64,
        ) {
            let record = TorrentRecord {
                info_hash: InfoHash::new(hash),
                download_dir: dir.into(),
                magnet,
                priorities: prios.into_iter().map(|i| Priority::ALL[i]).collect(),
                paused,
                added_at: added,
            };
            prop_assert_eq!(TorrentRecord::decode(&record.encode()).unwrap(), record);
        }

        #[test]
        fn decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..200)) {
            let _ = TorrentRecord::decode(&bytes);
        }
    }
}
