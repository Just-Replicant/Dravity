//! A byte-budgeted read cache for seeding, as a [`Storage`] middleware.
//!
//! [`CachedStorage`] wraps any storage and keeps recently read 16 KiB
//! chunks of its content stream in a [`ReadCache`]. Several storages (one
//! per torrent) can share one cache, and with it one byte budget. A popular
//! piece uploaded to many peers is then read from disk once.
//!
//! The cache is a segmented LRU. A chunk enters on probation, and a second
//! hit moves it to the protected segment (at most 80 % of the budget). A
//! recheck or a peer sweeping through the torrent once only cycles the
//! probation segment, so the chunks peers keep asking for survive scans.
//!
//! Writes go straight through and drop the chunks they overlap. A read
//! that raced a write does not insert what it read, so the cache never
//! serves bytes older than the last write.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;

use crate::{FileStamp, Storage, StorageError};

/// Size of a cached chunk (one block).
pub const CACHE_CHUNK: u64 = 16 * 1024;

/// Hit and miss counts of a [`ReadCache`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CacheStats {
    /// Chunks served from memory.
    pub hits: u64,
    /// Chunks read from the wrapped storage.
    pub misses: u64,
    /// Bytes held now.
    pub bytes: u64,
    /// The byte budget.
    pub budget: u64,
}

type Key = (u64, u64);

struct Entry {
    data: Bytes,
    protected: bool,
    tick: u64,
}

#[derive(Default)]
struct Segments {
    entries: HashMap<Key, Entry>,
    probation: BTreeMap<u64, Key>,
    protected: BTreeMap<u64, Key>,
    probation_bytes: usize,
    protected_bytes: usize,
    tick: u64,
}

impl Segments {
    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    fn remove(&mut self, key: Key) -> Option<Entry> {
        let entry = self.entries.remove(&key)?;
        if entry.protected {
            self.protected.remove(&entry.tick);
            self.protected_bytes -= entry.data.len();
        } else {
            self.probation.remove(&entry.tick);
            self.probation_bytes -= entry.data.len();
        }
        Some(entry)
    }

    fn place(&mut self, key: Key, data: Bytes, protected: bool) {
        let tick = self.next_tick();
        if protected {
            self.protected.insert(tick, key);
            self.protected_bytes += data.len();
        } else {
            self.probation.insert(tick, key);
            self.probation_bytes += data.len();
        }
        self.entries.insert(
            key,
            Entry {
                data,
                protected,
                tick,
            },
        );
    }

    fn bytes(&self) -> usize {
        self.probation_bytes + self.protected_bytes
    }
}

/// A shared, byte-budgeted chunk cache. See the [module docs](self).
pub struct ReadCache {
    budget: usize,
    segments: Mutex<Segments>,
    next_id: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl ReadCache {
    /// A cache holding at most `budget` bytes.
    #[must_use]
    pub fn new(budget: usize) -> Arc<Self> {
        Arc::new(Self {
            budget,
            segments: Mutex::new(Segments::default()),
            next_id: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        })
    }

    /// Current counters.
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            bytes: self.lock().bytes() as u64,
            budget: self.budget as u64,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Segments> {
        self.segments.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn protected_cap(&self) -> usize {
        self.budget / 5 * 4
    }

    fn get(&self, key: Key) -> Option<Bytes> {
        let mut s = self.lock();
        let entry = s.remove(key)?;
        let data = entry.data.clone();
        // A second hit (or any hit in the protected segment) protects it.
        s.place(key, entry.data, true);
        while s.protected_bytes > self.protected_cap() {
            let Some((_, oldest)) = s.protected.pop_first() else {
                break;
            };
            if let Some(e) = s.entries.remove(&oldest) {
                s.protected_bytes -= e.data.len();
                s.place(oldest, e.data, false);
            }
        }
        drop(s);
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(data)
    }

    fn insert(&self, key: Key, data: Bytes) {
        if data.len() > self.budget {
            return;
        }
        let mut s = self.lock();
        s.remove(key);
        s.place(key, data, false);
        while s.bytes() > self.budget {
            let oldest = s
                .probation
                .first_key_value()
                .or_else(|| s.protected.first_key_value())
                .map(|(_, k)| *k);
            let Some(oldest) = oldest else { break };
            s.remove(oldest);
        }
    }

    fn invalidate(&self, id: u64, chunks: std::ops::Range<u64>) {
        let mut s = self.lock();
        if chunks.end.saturating_sub(chunks.start) > s.entries.len() as u64 {
            let keys: Vec<Key> = s
                .entries
                .keys()
                .filter(|(i, c)| *i == id && chunks.contains(c))
                .copied()
                .collect();
            for key in keys {
                s.remove(key);
            }
        } else {
            for chunk in chunks {
                s.remove((id, chunk));
            }
        }
    }

    fn forget(&self, id: u64) {
        let mut s = self.lock();
        let keys: Vec<Key> = s
            .entries
            .keys()
            .filter(|(i, _)| *i == id)
            .copied()
            .collect();
        for key in keys {
            s.remove(key);
        }
    }
}

impl fmt::Debug for ReadCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadCache")
            .field("stats", &self.stats())
            .finish()
    }
}

/// A [`Storage`] whose reads go through a shared [`ReadCache`].
pub struct CachedStorage {
    inner: Arc<dyn Storage>,
    cache: Arc<ReadCache>,
    id: u64,
    /// Bumped by every write, so a read that raced one does not cache.
    epoch: AtomicU64,
}

impl CachedStorage {
    /// Wraps `inner`, caching its reads in `cache`.
    #[must_use]
    pub fn new(inner: Arc<dyn Storage>, cache: Arc<ReadCache>) -> Self {
        let id = cache.next_id.fetch_add(1, Ordering::Relaxed);
        Self {
            inner,
            cache,
            id,
            epoch: AtomicU64::new(0),
        }
    }

    /// The wrapped storage.
    #[must_use]
    pub fn inner(&self) -> &Arc<dyn Storage> {
        &self.inner
    }

    fn written(&self, offset: u64, len: usize) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        let end = offset.saturating_add(len as u64);
        self.cache
            .invalidate(self.id, offset / CACHE_CHUNK..end.div_ceil(CACHE_CHUNK));
    }

    /// The chunk-aligned span covering `offset..offset + len`, clipped to
    /// the stream.
    fn span(&self, offset: u64, len: usize) -> (u64, u64) {
        let start = offset / CACHE_CHUNK * CACHE_CHUNK;
        let end = offset
            .saturating_add(len as u64)
            .div_ceil(CACHE_CHUNK)
            .saturating_mul(CACHE_CHUNK)
            .min(self.inner.len());
        (start, end)
    }

    /// The cached chunks covering the span, or `None` if any is missing.
    fn cached(&self, start: u64, end: u64) -> Option<Vec<Bytes>> {
        (start / CACHE_CHUNK..end.div_ceil(CACHE_CHUNK))
            .map(|chunk| self.cache.get((self.id, chunk)))
            .collect()
    }

    /// Splits `data` (the span from `start`) into chunks and caches them,
    /// unless a write happened since `epoch`.
    fn fill(&self, start: u64, data: &Bytes, epoch: u64) {
        if self.epoch.load(Ordering::Acquire) != epoch {
            return;
        }
        let chunk = usize::try_from(CACHE_CHUNK).unwrap_or(usize::MAX);
        for (i, part) in (0u64..).zip(data.chunks(chunk)) {
            let at = start / CACHE_CHUNK + i;
            let begin = usize::try_from(i).unwrap_or(0) * chunk;
            self.cache
                .insert((self.id, at), data.slice(begin..begin + part.len()));
        }
    }
}

/// Copies `offset..offset + buf.len()` out of `chunks` (starting at
/// `start`).
fn copy_out(chunks: &[Bytes], start: u64, offset: u64, buf: &mut [u8]) {
    let mut skip = usize::try_from(offset - start).unwrap_or(0);
    let mut written = 0;
    for chunk in chunks {
        if written == buf.len() {
            break;
        }
        if skip >= chunk.len() {
            skip -= chunk.len();
            continue;
        }
        let take = (chunk.len() - skip).min(buf.len() - written);
        if let (Some(dst), Some(src)) = (
            buf.get_mut(written..written + take),
            chunk.get(skip..skip + take),
        ) {
            dst.copy_from_slice(src);
        }
        written += take;
        skip = 0;
    }
}

impl Storage for CachedStorage {
    fn len(&self) -> u64 {
        self.inner.len()
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), StorageError> {
        let result = self.inner.write_at(offset, data);
        self.written(offset, data.len());
        result
    }

    fn write_bytes(&self, offset: u64, data: Bytes) -> Result<(), StorageError> {
        let len = data.len();
        let result = self.inner.write_bytes(offset, data);
        self.written(offset, len);
        result
    }

    fn write_many(&self, writes: Vec<(u64, Bytes)>) -> Result<(), StorageError> {
        let spans: Vec<(u64, usize)> = writes.iter().map(|(o, d)| (*o, d.len())).collect();
        let result = self.inner.write_many(writes);
        for (offset, len) in spans {
            self.written(offset, len);
        }
        result
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), StorageError> {
        crate::check_range(offset, buf.len(), self.len())?;
        if buf.is_empty() {
            return Ok(());
        }
        let (start, end) = self.span(offset, buf.len());
        if let Some(chunks) = self.cached(start, end) {
            copy_out(&chunks, start, offset, buf);
            return Ok(());
        }
        let n = (end - start).div_ceil(CACHE_CHUNK);
        self.cache.misses.fetch_add(n, Ordering::Relaxed);
        let epoch = self.epoch.load(Ordering::Acquire);
        let span_len = usize::try_from(end - start).map_err(|_| StorageError::OutOfBounds {
            offset,
            len: buf.len(),
        })?;
        let data = Bytes::from(self.inner.read_vec(start, span_len)?);
        copy_out(std::slice::from_ref(&data), start, offset, buf);
        self.fill(start, &data, epoch);
        Ok(())
    }

    fn read_many(&self, reads: &[(u64, usize)]) -> Result<Vec<Bytes>, StorageError> {
        let mut out: Vec<Option<Bytes>> = Vec::with_capacity(reads.len());
        let mut misses = Vec::new();
        let mut spans = Vec::new();
        for &(offset, len) in reads {
            crate::check_range(offset, len, self.len())?;
            let (start, end) = self.span(offset, len);
            match self.cached(start, end) {
                Some(chunks) => {
                    let mut buf = vec![0; len];
                    copy_out(&chunks, start, offset, &mut buf);
                    out.push(Some(Bytes::from(buf)));
                }
                None => {
                    misses.push((out.len(), offset, len, start));
                    spans.push((start, usize::try_from(end - start).unwrap_or(usize::MAX)));
                    out.push(None);
                }
            }
        }
        if !spans.is_empty() {
            let chunks: u64 = spans
                .iter()
                .map(|&(_, l)| (l as u64).div_ceil(CACHE_CHUNK))
                .sum();
            self.cache.misses.fetch_add(chunks, Ordering::Relaxed);
            let epoch = self.epoch.load(Ordering::Acquire);
            let read = self.inner.read_many(&spans)?;
            for ((slot, offset, len, start), data) in misses.into_iter().zip(read) {
                let mut buf = vec![0; len];
                copy_out(std::slice::from_ref(&data), start, offset, &mut buf);
                self.fill(start, &data, epoch);
                if let Some(o) = out.get_mut(slot) {
                    *o = Some(Bytes::from(buf));
                }
            }
        }
        out.into_iter()
            .map(|b| b.ok_or(StorageError::OutOfBounds { offset: 0, len: 0 }))
            .collect()
    }

    fn flush(&self) -> Result<(), StorageError> {
        self.inner.flush()
    }

    fn sync_range(&self, offset: u64, len: u64) -> Result<(), StorageError> {
        self.inner.sync_range(offset, len)
    }

    fn stamps(&self) -> Result<Option<Vec<FileStamp>>, StorageError> {
        self.inner.stamps()
    }
}

impl Drop for CachedStorage {
    fn drop(&mut self) {
        self.cache.forget(self.id);
    }
}

impl fmt::Debug for CachedStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CachedStorage")
            .field("inner", &self.inner)
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryStorage;

    /// Memory storage counting read calls.
    #[derive(Debug)]
    struct Counting(MemoryStorage, AtomicU64);

    impl Storage for Counting {
        fn len(&self) -> u64 {
            self.0.len()
        }
        fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), StorageError> {
            self.0.write_at(offset, data)
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), StorageError> {
            self.1.fetch_add(1, Ordering::Relaxed);
            self.0.read_at(offset, buf)
        }
        fn flush(&self) -> Result<(), StorageError> {
            Ok(())
        }
    }

    fn setup(len: u64, budget: usize) -> (Arc<Counting>, CachedStorage, Vec<u8>) {
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let inner = Arc::new(Counting(MemoryStorage::new(len), AtomicU64::new(0)));
        inner.write_at(0, &data).unwrap();
        let cached = CachedStorage::new(inner.clone(), ReadCache::new(budget));
        (inner, cached, data)
    }

    #[test]
    fn repeated_reads_hit_memory() {
        let (inner, cached, data) = setup(100_000, 1 << 20);
        for _ in 0..3 {
            assert_eq!(
                cached.read_vec(20_000, 16_384).unwrap(),
                &data[20_000..36_384]
            );
        }
        assert_eq!(inner.1.load(Ordering::Relaxed), 1);
        // The short last chunk too.
        assert_eq!(cached.read_vec(99_990, 10).unwrap(), &data[99_990..]);
        assert_eq!(cached.read_vec(98_304, 1_696).unwrap(), &data[98_304..]);
        assert_eq!(inner.1.load(Ordering::Relaxed), 2);
        assert!(cached.read_vec(99_990, 11).is_err());
        let stats = cached.cache.stats();
        assert!(stats.hits >= 3 && stats.misses >= 2, "{stats:?}");
    }

    #[test]
    fn writes_invalidate() {
        let (_inner, cached, _) = setup(50_000, 1 << 20);
        let _ = cached.read_vec(0, 50_000).unwrap();
        cached.write_at(17_000, &[9, 9, 9]).unwrap();
        assert_eq!(cached.read_vec(17_000, 3).unwrap(), [9, 9, 9]);
        cached
            .write_many(vec![(40_000, Bytes::from_static(&[7; 4]))])
            .unwrap();
        assert_eq!(cached.read_vec(40_000, 4).unwrap(), [7; 4]);
        let many = cached.read_many(&[(17_000, 3), (40_000, 4)]).unwrap();
        assert_eq!(&many[0][..], [9, 9, 9]);
        assert_eq!(&many[1][..], [7; 4]);
    }

    #[test]
    fn the_budget_holds_and_hot_chunks_survive_a_scan() {
        // Room for 4 chunks.
        let (inner, cached, data) = setup(40 * CACHE_CHUNK, 4 * 16_384);
        let chunk = 16_384;
        // A hot chunk, hit twice: protected.
        let _ = cached.read_vec(0, chunk).unwrap();
        let _ = cached.read_vec(0, chunk).unwrap();
        // A scan over the rest.
        for i in 1..40 {
            let _ = cached.read_vec(i * CACHE_CHUNK, chunk).unwrap();
        }
        assert!(cached.cache.stats().bytes <= 4 * CACHE_CHUNK);
        let before = inner.1.load(Ordering::Relaxed);
        assert_eq!(cached.read_vec(0, chunk).unwrap(), &data[..chunk]);
        assert_eq!(inner.1.load(Ordering::Relaxed), before, "still cached");
    }

    #[test]
    fn storages_share_the_budget_and_clean_up() {
        let cache = ReadCache::new(1 << 20);
        let a = CachedStorage::new(Arc::new(MemoryStorage::new(32_768)), cache.clone());
        let b = CachedStorage::new(Arc::new(MemoryStorage::new(32_768)), cache.clone());
        a.write_at(0, &[1; 32_768]).unwrap();
        b.write_at(0, &[2; 32_768]).unwrap();
        assert_eq!(a.read_vec(0, 4).unwrap(), [1; 4]);
        assert_eq!(b.read_vec(0, 4).unwrap(), [2; 4], "separate keys");
        assert_eq!(cache.stats().bytes, 2 * CACHE_CHUNK);
        drop(a);
        assert_eq!(cache.stats().bytes, CACHE_CHUNK);
        drop(b);
        assert_eq!(cache.stats().bytes, 0);
    }
}
