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
//! file, sync, rename, then a sync of the directory) by one background
//! task. Each path keeps only its latest write or removal, so a slow disk
//! cannot drop a save or a removal, and callers never block on the disk.
//!
//! The `.state` format is a bencoded dictionary:
//!
//! ```text
//! d 5:addedi<unix seconds>e 12:download-dir<path> 9:info-hash20:<hash>
//!   [6:magnet<uri>] 6:pausedi<0|1>e 10:prioritiesl<i0..3e>...e 7:versioni1e e
//! ```

use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use dravity_bencode::{Dict, Value};
use dravity_core::{InfoHash, Metainfo};
use dravity_engine::Priority;

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
        insert_download_dir(&mut d, &self.download_dir);
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
        let download_dir = download_dir_from(d)?;
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

/// Stores `path` without rewriting bytes that are not valid Unicode.
///
/// Unix paths are raw bytes. Windows paths that are Unicode stay UTF-8, so
/// older state files still decode. A Windows path that is not Unicode is
/// stored as UTF-16LE under `download-dir-wide` instead of being lossily
/// replaced.
fn insert_download_dir(dict: &mut Dict<'_>, path: &Path) {
    let key = |k: &str| Cow::Owned(k.as_bytes().to_vec());
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        dict.insert(
            key("download-dir"),
            Value::Bytes(Cow::Owned(path.as_os_str().as_bytes().to_vec())),
        );
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        if let Some(text) = path.to_str() {
            dict.insert(
                key("download-dir"),
                Value::Bytes(Cow::Owned(text.as_bytes().to_vec())),
            );
        } else {
            let wide: Vec<u8> = path
                .as_os_str()
                .encode_wide()
                .flat_map(|unit| unit.to_le_bytes())
                .collect();
            dict.insert(key("download-dir-wide"), Value::Bytes(Cow::Owned(wide)));
        }
    }
    #[cfg(not(any(unix, windows)))]
    if let Some(text) = path.to_str() {
        dict.insert(
            key("download-dir"),
            Value::Bytes(Cow::Owned(text.as_bytes().to_vec())),
        );
    }
}

fn download_dir_from(dict: &Dict<'_>) -> Result<PathBuf, RecordError> {
    if let Some(wide) = dict
        .get(b"download-dir-wide".as_slice())
        .and_then(Value::as_bytes)
    {
        return path_from_wide(wide);
    }
    let bytes = dict
        .get(b"download-dir".as_slice())
        .and_then(Value::as_bytes)
        .filter(|bytes| !bytes.is_empty())
        .ok_or(RecordError::Invalid("missing download-dir"))?;
    path_from_bytes(bytes)
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf, RecordError> {
    use std::os::unix::ffi::OsStrExt;
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf, RecordError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| RecordError::Invalid("download-dir is not Unicode"))?;
    Ok(PathBuf::from(text))
}

/// UTF-16LE path from a Windows state file.
fn path_from_wide(bytes: &[u8]) -> Result<PathBuf, RecordError> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(2) {
        return Err(RecordError::Invalid("download-dir-wide is malformed"));
    }
    let (chunks, _) = bytes.as_chunks::<2>();
    let units: Vec<u16> = chunks.iter().copied().map(u16::from_le_bytes).collect();
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        Ok(PathBuf::from(std::ffi::OsString::from_wide(&units)))
    }
    #[cfg(not(windows))]
    {
        let text = String::from_utf16(&units)
            .map_err(|_| RecordError::Invalid("download-dir-wide is not Unicode"))?;
        if text.is_empty() {
            return Err(RecordError::Invalid("missing download-dir"));
        }
        Ok(PathBuf::from(text))
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

/// Writes `bytes` to `path` atomically (temporary file, sync, rename) and
/// syncs the parent directory so a crash cannot lose the directory entry.
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
        fs::rename(&tmp, path)?;
        sync_parent(path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Removes `path`; a missing file is fine. The parent directory is synced
/// after a removal so the deletion survives a crash.
pub(crate) fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        Err(_) => Ok(()),
        Ok(()) => sync_parent(path),
    }
}

fn sync_parent(path: &Path) -> io::Result<()> {
    let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) else {
        return Ok(());
    };
    sync_dir(dir)
}

/// `std::fs::File::open` on a directory works on Unix. Windows needs backup
/// semantics, which that call does not request; the file sync already done
/// is what Windows will persist.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// The latest operation for one state file.
#[derive(Debug)]
enum Kind {
    Write(Vec<u8>),
    Remove,
}

/// Operations waiting for the writer. One entry per path: a later write or
/// removal replaces the earlier one, and a removal after a write (or the
/// reverse) keeps the later intent.
#[derive(Debug, Default)]
struct Mail {
    order: VecDeque<PathBuf>,
    jobs: HashMap<PathBuf, Kind>,
    flushes: Vec<tokio::sync::oneshot::Sender<()>>,
}

impl Mail {
    fn push(&mut self, path: PathBuf, kind: Kind) {
        if !self.jobs.contains_key(&path) {
            self.order.push_back(path.clone());
        }
        self.jobs.insert(path, kind);
    }

    fn drain(&mut self) -> (Vec<(PathBuf, Kind)>, Vec<tokio::sync::oneshot::Sender<()>>) {
        let jobs = self
            .order
            .drain(..)
            .filter_map(|path| self.jobs.remove(&path).map(|kind| (path, kind)))
            .collect();
        (jobs, std::mem::take(&mut self.flushes))
    }
}

struct Shared {
    mail: Mutex<Mail>,
    notify: tokio::sync::Notify,
}

/// Writes session state in a background task. The latest change to each
/// file is kept until it is on disk.
#[derive(Clone)]
pub(crate) struct Persister {
    /// The state directory.
    dir: PathBuf,
    shared: std::sync::Arc<Shared>,
    /// Dropped with the last clone; the writer finishes what is queued and
    /// then stops.
    alive: tokio::sync::mpsc::Sender<()>,
}

impl std::fmt::Debug for Persister {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Persister")
            .field("dir", &self.dir)
            .field("open", &!self.alive.is_closed())
            .finish_non_exhaustive()
    }
}

impl Persister {
    /// Starts the writer task. Must be called within a Tokio runtime.
    pub(crate) fn start(dir: PathBuf) -> Self {
        let shared = std::sync::Arc::new(Shared {
            mail: Mutex::new(Mail::default()),
            notify: tokio::sync::Notify::new(),
        });
        let (alive, mut closed) = tokio::sync::mpsc::channel::<()>(1);
        let worker = std::sync::Arc::clone(&shared);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = closed.recv() => {
                        apply(&worker).await;
                        break;
                    }
                    () = worker.notify.notified() => apply(&worker).await,
                }
            }
        });
        Self { dir, shared, alive }
    }

    fn enqueue(&self, path: PathBuf, kind: Kind) {
        self.shared
            .mail
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(path, kind);
        self.shared.notify.notify_one();
    }

    /// Saves a torrent's record.
    pub(crate) fn save_record(&self, record: &TorrentRecord) {
        let path = path_for(&self.dir, &record.info_hash, "state");
        self.enqueue(path, Kind::Write(record.encode()));
    }

    /// Saves a torrent's metainfo.
    pub(crate) fn save_torrent(&self, metainfo: &Metainfo) {
        let path = path_for(&self.dir, &metainfo.info_hash(), "torrent");
        self.enqueue(path, Kind::Write(torrent_file_bytes(metainfo)));
    }

    /// Forgets a torrent.
    pub(crate) fn remove(&self, info_hash: &InfoHash) {
        for extension in ["state", "torrent"] {
            self.enqueue(path_for(&self.dir, info_hash, extension), Kind::Remove);
        }
    }

    /// Waits until everything submitted so far is on disk.
    pub(crate) async fn flush(&self) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.shared
            .mail
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .flushes
            .push(tx);
        self.shared.notify.notify_one();
        let _ = rx.await;
    }
}

/// Applies every queued operation, then acknowledges flushes that were
/// waiting for those operations. Repeats while more work arrived.
async fn apply(shared: &Shared) {
    loop {
        let (jobs, flushes) = shared
            .mail
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain();
        if jobs.is_empty() && flushes.is_empty() {
            return;
        }
        for (path, kind) in jobs {
            let shown = path.clone();
            let done = tokio::task::spawn_blocking(move || match kind {
                Kind::Write(bytes) => write_atomic(&path, &bytes),
                Kind::Remove => remove_if_exists(&path),
            });
            match done.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!(path = %shown.display(), error = %e, "saving session state failed");
                }
                Err(e) => {
                    tracing::warn!(path = %shown.display(), error = %e, "session state task failed");
                }
            }
        }
        for ack in flushes {
            let _ = ack.send(());
        }
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

    #[test]
    fn the_latest_operation_for_a_path_is_the_one_written() {
        use std::path::PathBuf;

        use super::{Kind, Mail};

        let mut mail = Mail::default();
        let path = PathBuf::from("a.state");
        mail.push(path.clone(), Kind::Write(b"old".to_vec()));
        mail.push(path.clone(), Kind::Write(b"new".to_vec()));
        mail.push(PathBuf::from("b.state"), Kind::Write(b"b".to_vec()));
        mail.push(path.clone(), Kind::Remove);
        let (jobs, flushes) = mail.drain();
        assert!(flushes.is_empty());
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].0, path);
        assert!(matches!(jobs[0].1, Kind::Remove));
        assert_eq!(jobs[1].0, PathBuf::from("b.state"));
        assert!(matches!(jobs[1].1, Kind::Write(ref bytes) if bytes == b"b"));
    }

    #[test]
    fn atomic_write_replaces_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.state");
        super::write_atomic(&path, b"one").unwrap();
        super::write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert!(!dir.path().join("a.state.tmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_download_dir_roundtrips() {
        use std::os::unix::ffi::OsStringExt;

        let mut record = sample();
        record.download_dir = std::ffi::OsString::from_vec(b"/tmp/downl\xffads".to_vec()).into();
        assert_eq!(TorrentRecord::decode(&record.encode()).unwrap(), record);
    }

    #[cfg(windows)]
    #[test]
    fn a_non_unicode_download_dir_roundtrips() {
        use std::os::windows::ffi::OsStringExt;

        let mut record = sample();
        record.download_dir = std::ffi::OsString::from_wide(&[0xD800, 0x20]).into();
        assert_eq!(TorrentRecord::decode(&record.encode()).unwrap(), record);
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
