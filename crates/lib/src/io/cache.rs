// Disk Atlas - A cross-platform filesystem explorer.
//
// Copyright (C) 2026 Julian A. Loy.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
//
// See the LICENSE file for the full license text.

//! Chunk cache, read-ahead and the sequential scan path.
//!
//! One [`ChunkCache`] is shared by the whole app behind an `Arc`. Each base
//! source is wrapped once in a [`CachedSource`], which reads through the
//! cache in [`CHUNK_SIZE`] chunks aligned in the base source's offset space.
//!
//! - The cache has 16 shards. Each is a `Mutex` over a map from
//!   `(source key, chunk index)` to a slot and a CLOCK ring of slots. No lock
//!   is held during inner I/O; a hit clones the chunk's `Arc` under the lock
//!   and copies outside it.
//! - A read that misses a run of consecutive chunks fetches the run with one
//!   inner read.
//! - Read-ahead is synchronous. A miss run that starts exactly at the chunk
//!   after the source's previous fetch is sequential and widens the fetch to
//!   a window of 2 chunks, doubling per sequential miss up to 16 (1 MiB).
//!   Any other miss resets the window to 2.
//! - `read_bulk` bypasses the cache entirely, so a scan does not evict what
//!   the interface is using.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::error::{Error, IoContext, Result};
use crate::io::block::{BlockSource, check_range};

/// Size of one cached chunk in bytes. The last chunk of a source is short.
pub const CHUNK_SIZE: usize = 64 * 1024;

/// Default byte budget of a [`ChunkCache`]: 256 MiB.
pub const DEFAULT_BUDGET: u64 = 256 * 1024 * 1024;

/// [`CHUNK_SIZE`] as a `u64`, for offset arithmetic.
const CHUNK: u64 = 64 * 1024;

/// Number of independently locked shards.
const SHARDS: usize = 16;

/// [`SHARDS`] as a `u64`.
const SHARDS_U64: u64 = 16;

/// Read-ahead window after a non-sequential miss, in chunks.
const MIN_WINDOW: u64 = 2;

/// Largest read-ahead window, in chunks (1 MiB).
const MAX_WINDOW: u64 = 16;

/// "No previous fetch" marker. No chunk index can reach it: the largest
/// index is `u64::MAX / CHUNK`.
const NO_MARK: u64 = u64::MAX;

/// Counters for tests and benchmarks.
///
/// `hits` and `misses` count chunk lookups made on behalf of a `read_at`:
/// a read that touches three chunks makes three lookups. Chunks fetched by
/// read-ahead are not looked up and count as neither. `inner_reads` and
/// `inner_bytes` count completed reads from inner sources through the cache;
/// a failed inner read adds nothing to them. `read_bulk` touches none of
/// the counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheStats {
    /// Chunk lookups that found the chunk.
    pub hits: u64,
    /// Chunk lookups that did not find the chunk.
    pub misses: u64,
    /// Completed inner reads issued by the cache.
    pub inner_reads: u64,
    /// Bytes delivered by those inner reads.
    pub inner_bytes: u64,
}

/// A byte-budgeted cache of 64 KiB chunks shared by any number of
/// [`CachedSource`]s.
///
/// `new` returns the cache by value; wrap it in an `Arc` to share it.
pub struct ChunkCache {
    shards: [Shard; SHARDS],
    next_key: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    inner_reads: AtomicU64,
    inner_bytes: AtomicU64,
}

impl ChunkCache {
    /// A cache that holds about `budget_bytes` of chunk data.
    ///
    /// The budget is split evenly over 16 shards and every slot is charged
    /// a full chunk, short last chunks included, so the budget is an upper
    /// bound on chunk bytes held. Each shard holds at least one chunk, so
    /// even a budget of 0 caches 16 chunks (1 MiB).
    pub fn new(budget_bytes: u64) -> Self {
        let per_shard = budget_bytes / (SHARDS_U64 * CHUNK);
        let capacity = usize::try_from(per_shard).unwrap_or(usize::MAX).max(1);
        Self {
            shards: std::array::from_fn(|_| Shard::new(capacity)),
            next_key: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            inner_reads: AtomicU64::new(0),
            inner_bytes: AtomicU64::new(0),
        }
    }

    /// A snapshot of the counters. Each counter is read on its own, so a
    /// snapshot taken while other threads read may mix moments.
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            inner_reads: self.inner_reads.load(Ordering::Relaxed),
            inner_bytes: self.inner_bytes.load(Ordering::Relaxed),
        }
    }

    /// The shard that holds `(key, chunk)`. Consecutive chunks of one source
    /// land in consecutive shards.
    fn shard(&self, key: u64, chunk: u64) -> &Shard {
        let mixed = key.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(chunk);
        let index = usize::try_from(mixed & (SHARDS_U64 - 1)).unwrap_or(0);
        match self.shards.get(index) {
            Some(shard) => shard,
            None => &self.shards[0],
        }
    }

    /// Looks a chunk up, marks it referenced and counts a hit or a miss.
    fn get(&self, key: u64, chunk: u64) -> Option<Arc<[u8]>> {
        let found = self.shard(key, chunk).lock().get((key, chunk));
        let counter = if found.is_some() {
            &self.hits
        } else {
            &self.misses
        };
        counter.fetch_add(1, Ordering::Relaxed);
        found
    }

    /// True when the chunk is cached. Touches neither the referenced bit
    /// nor the counters.
    fn contains(&self, key: u64, chunk: u64) -> bool {
        self.shard(key, chunk).lock().contains((key, chunk))
    }

    /// Inserts a chunk, replacing any copy already there.
    fn insert(&self, key: u64, chunk: u64, data: Arc<[u8]>) {
        self.shard(key, chunk).lock().insert((key, chunk), data);
    }

    /// Number of chunks held, for tests.
    #[cfg(test)]
    fn cached_chunks(&self) -> usize {
        self.shards.iter().map(|s| s.lock().slots.len()).sum()
    }
}

impl Default for ChunkCache {
    /// A cache with [`DEFAULT_BUDGET`].
    fn default() -> Self {
        Self::new(DEFAULT_BUDGET)
    }
}

/// `(source key, chunk index)`.
type ChunkId = (u64, u64);

/// One lock's worth of the cache.
struct Shard(Mutex<ShardState>);

impl Shard {
    fn new(capacity: usize) -> Self {
        Self(Mutex::new(ShardState {
            map: HashMap::new(),
            slots: Vec::new(),
            hand: 0,
            capacity,
        }))
    }

    /// Locks the shard. A poisoned lock is recovered: every slot is checked
    /// against the map on lookup, so a half-finished update reads as a miss.
    fn lock(&self) -> MutexGuard<'_, ShardState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A cached chunk.
struct Slot {
    id: ChunkId,
    data: Arc<[u8]>,
    referenced: bool,
}

/// A map to slot indexes and a CLOCK ring over the slots.
struct ShardState {
    map: HashMap<ChunkId, usize>,
    /// Grows to `capacity`, then slots are reused in place.
    slots: Vec<Slot>,
    /// The CLOCK hand: the next slot considered for eviction. Always less
    /// than `slots.len()` once the ring is full.
    hand: usize,
    /// Most slots this shard holds; at least 1.
    capacity: usize,
}

impl ShardState {
    fn get(&mut self, id: ChunkId) -> Option<Arc<[u8]>> {
        let index = *self.map.get(&id)?;
        let slot = self.slots.get_mut(index)?;
        if slot.id != id {
            return None;
        }
        slot.referenced = true;
        Some(Arc::clone(&slot.data))
    }

    fn contains(&self, id: ChunkId) -> bool {
        self.map
            .get(&id)
            .and_then(|&index| self.slots.get(index))
            .is_some_and(|slot| slot.id == id)
    }

    fn insert(&mut self, id: ChunkId, data: Arc<[u8]>) {
        if let Some(&index) = self.map.get(&id) {
            if let Some(slot) = self.slots.get_mut(index)
                && slot.id == id
            {
                // Two threads missed the same chunk; the second insert wins.
                slot.data = data;
                return;
            }
            // A stale entry left by a recovered poisoned lock.
            self.map.remove(&id);
        }

        if self.slots.len() < self.capacity {
            self.map.insert(id, self.slots.len());
            self.slots.push(Slot {
                id,
                data,
                referenced: false,
            });
            return;
        }

        let Some(victim) = self.find_victim() else {
            return;
        };
        if let Some(slot) = self.slots.get_mut(victim) {
            let old = slot.id;
            slot.id = id;
            slot.data = data;
            slot.referenced = false;
            if self.map.get(&old) == Some(&victim) {
                self.map.remove(&old);
            }
            self.map.insert(id, victim);
        }
    }

    /// Advances the CLOCK hand to an unreferenced slot, clearing referenced
    /// bits on the way, and returns it. The hand ends just past the victim,
    /// so a new chunk is the last one the hand reaches.
    fn find_victim(&mut self) -> Option<usize> {
        let len = self.slots.len();
        if len == 0 {
            return None;
        }
        if self.hand >= len {
            self.hand = 0;
        }
        // One sweep clears every referenced bit, so the second finds a victim.
        for _ in 0..len.saturating_mul(2) {
            let current = self.hand;
            self.hand = match current.checked_add(1) {
                Some(next) if next < len => next,
                _ => 0,
            };
            let slot = self.slots.get_mut(current)?;
            if slot.referenced {
                slot.referenced = false;
            } else {
                return Some(current);
            }
        }
        Some(self.hand)
    }
}

/// A [`BlockSource`] that reads its inner source through a [`ChunkCache`].
///
/// Put one directly over each base source (a host file, a device, a decoded
/// image) and cut partitions from it with [`Slice`](super::block::Slice).
///
/// Read-ahead extends a sequential fetch up to the window, but stops at the
/// first chunk that is already cached, so a chunk is never fetched twice
/// while it is cached and the fetch stays one contiguous inner read. A hit
/// on the chunk just after the previous fetch moves that mark past it, so a
/// scan that crosses cached chunks stays sequential.
pub struct CachedSource {
    inner: Arc<dyn BlockSource>,
    cache: Arc<ChunkCache>,
    key: u64,
    len: u64,
    /// Number of chunks in the source; the last may be short.
    chunks: u64,
    /// The chunk just after the last fetch, or [`NO_MARK`].
    mark: AtomicU64,
    /// Read-ahead window for the next sequential miss, in chunks.
    window: AtomicU64,
}

impl CachedSource {
    /// Wraps `inner`, giving it a cache key no other source has had.
    ///
    /// Fails with `Error::Unsupported` when the inner sector size is not a
    /// power of two from 512 to 65536: the chunk size must be a multiple of
    /// it.
    pub fn new(inner: Arc<dyn BlockSource>, cache: Arc<ChunkCache>) -> Result<Self> {
        let sector = inner.sector_size();
        if !sector.is_power_of_two() || !(512..=65536).contains(&sector) {
            return Err(Error::unsupported(format!(
                "sector size {sector}: must be a power of two from 512 to 65536"
            )));
        }
        let len = inner.len();
        let key = cache.next_key.fetch_add(1, Ordering::Relaxed);
        Ok(Self {
            inner,
            cache,
            key,
            len,
            chunks: len.div_ceil(CHUNK),
            mark: AtomicU64::new(NO_MARK),
            window: AtomicU64::new(MIN_WINDOW),
        })
    }

    /// Counts a hit on `chunk` for sequential detection.
    fn note_hit(&self, chunk: u64) {
        if self.mark.load(Ordering::Relaxed) == chunk {
            // Losing this race to another reader only costs a window reset.
            let _ = self.mark.compare_exchange(
                chunk,
                chunk.saturating_add(1),
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
    }

    /// Fetches the missed chunks `run_start..run_end` (plus any read-ahead)
    /// in one inner read, copies the requested part into `buf` and caches
    /// every fetched chunk. `offset` is where `buf` starts in the source.
    fn fetch(&self, run_start: u64, run_end: u64, offset: u64, buf: &mut [u8]) -> Result<()> {
        let sequential = self.mark.load(Ordering::Relaxed) == run_start;
        let window = self
            .window
            .load(Ordering::Relaxed)
            .clamp(MIN_WINDOW, MAX_WINDOW);

        let mut fetch_end = run_end;
        if sequential {
            let want = run_start.saturating_add(window).min(self.chunks);
            // Bounded by the window: at most MAX_WINDOW iterations.
            while fetch_end < want && !self.cache.contains(self.key, fetch_end) {
                fetch_end = fetch_end.saturating_add(1);
            }
        }

        let start = run_start.saturating_mul(CHUNK);
        let stop = fetch_end.saturating_mul(CHUNK).min(self.len);
        let fetch_len = stop
            .checked_sub(start)
            .ok_or_else(|| inconsistent(start, 0))?;
        let size = usize::try_from(fetch_len).map_err(|_| inconsistent(start, fetch_len))?;
        let mut data = vec![0u8; size];
        self.inner.read_at(start, &mut data)?;

        self.cache.inner_reads.fetch_add(1, Ordering::Relaxed);
        self.cache
            .inner_bytes
            .fetch_add(fetch_len, Ordering::Relaxed);
        let next_window = if sequential {
            window.saturating_mul(2).min(MAX_WINDOW)
        } else {
            MIN_WINDOW
        };
        self.window.store(next_window, Ordering::Relaxed);
        self.mark.store(fetch_end, Ordering::Relaxed);

        copy_overlap(&data, start, offset, buf)?;

        let mut chunk = run_start;
        for piece in data.chunks(CHUNK_SIZE) {
            self.cache.insert(self.key, chunk, Arc::from(piece));
            chunk = chunk.saturating_add(1);
        }
        Ok(())
    }
}

impl BlockSource for CachedSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(offset, buf.len(), self.len)?;
        if buf.is_empty() {
            return Ok(());
        }
        let req_len = u64::try_from(buf.len()).unwrap_or(u64::MAX);
        let end = offset
            .checked_add(req_len)
            .ok_or_else(|| inconsistent(offset, req_len))?;
        let first = offset / CHUNK;
        let last = end
            .checked_sub(1)
            .ok_or_else(|| inconsistent(offset, req_len))?
            / CHUNK;

        // A hit found while measuring a miss run; it belongs to `chunk`.
        let mut carried: Option<Arc<[u8]>> = None;
        let mut chunk = first;
        // Every pass advances `chunk` by at least one; `last < u64::MAX`.
        while chunk <= last {
            let found = carried.take().or_else(|| self.cache.get(self.key, chunk));
            if let Some(data) = found {
                self.note_hit(chunk);
                copy_overlap(&data, chunk.saturating_mul(CHUNK), offset, buf)?;
                chunk = chunk.saturating_add(1);
                continue;
            }

            let mut run_end = chunk.saturating_add(1);
            while run_end <= last {
                match self.cache.get(self.key, run_end) {
                    Some(data) => {
                        carried = Some(data);
                        break;
                    }
                    None => run_end = run_end.saturating_add(1),
                }
            }
            self.fetch(chunk, run_end, offset, buf)?;
            chunk = run_end;
        }
        Ok(())
    }

    fn read_bulk(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.inner.read_bulk(offset, buf)
    }
}

/// Copies into `buf` (the request at `offset`) the part of `src` (source
/// bytes starting at `src_start`) that overlaps it.
fn copy_overlap(src: &[u8], src_start: u64, offset: u64, buf: &mut [u8]) -> Result<()> {
    let buf_len = u64::try_from(buf.len()).unwrap_or(u64::MAX);
    let src_len = u64::try_from(src.len()).unwrap_or(u64::MAX);
    let bad = || inconsistent(offset, buf_len);
    let req_end = offset.checked_add(buf_len).ok_or_else(bad)?;
    let src_end = src_start.checked_add(src_len).ok_or_else(bad)?;
    let from = offset.max(src_start);
    let to = req_end.min(src_end);
    if from >= to {
        return Ok(());
    }
    let to_index = |value: Option<u64>| value.and_then(|v| usize::try_from(v).ok()).ok_or_else(bad);
    let src_from = to_index(from.checked_sub(src_start))?;
    let src_to = to_index(to.checked_sub(src_start))?;
    let dst_from = to_index(from.checked_sub(offset))?;
    let dst_to = to_index(to.checked_sub(offset))?;
    let piece = src.get(src_from..src_to).ok_or_else(bad)?;
    let dst = buf.get_mut(dst_from..dst_to).ok_or_else(bad)?;
    dst.copy_from_slice(piece);
    Ok(())
}

/// An internal bounds check failed. Unreachable while the cache keeps its
/// invariants; reported as an I/O error rather than a panic.
fn inconsistent(offset: u64, len: u64) -> Error {
    Error::io(
        IoContext::Read { offset, len },
        io::Error::other("chunk cache: inconsistent chunk bounds"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::block::{MemSource, Slice};
    use std::sync::atomic::AtomicBool;

    /// Xorshift64: deterministic test randomness without a dependency.
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed.max(1))
        }

        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        /// Uniform-ish in `0..=max`.
        fn upto(&mut self, max: u64) -> u64 {
            if max == u64::MAX {
                self.next()
            } else {
                self.next() % (max + 1)
            }
        }
    }

    fn bytes(len: usize, seed: u64) -> Vec<u8> {
        let mut rng = Rng::new(seed);
        (0..len).map(|_| rng.next() as u8).collect()
    }

    fn mem(len: usize, seed: u64) -> Arc<MemSource> {
        Arc::new(MemSource::new(bytes(len, seed)))
    }

    fn cached(inner: Arc<dyn BlockSource>, cache: &Arc<ChunkCache>) -> CachedSource {
        CachedSource::new(inner, Arc::clone(cache)).unwrap()
    }

    /// Reads `len` bytes at `offset` from both and checks they agree.
    fn check_same(a: &dyn BlockSource, b: &dyn BlockSource, offset: u64, len: usize) {
        let mut x = vec![0u8; len];
        let mut y = vec![0u8; len];
        let ra = a.read_at(offset, &mut x);
        let rb = b.read_at(offset, &mut y);
        assert_eq!(ra.is_ok(), rb.is_ok(), "offset {offset} len {len}");
        if ra.is_ok() {
            assert!(x == y, "bytes differ at offset {offset} len {len}");
        }
    }

    const C: usize = CHUNK_SIZE;

    #[test]
    fn equivalence_with_mem_source() {
        let sizes = [
            1,
            100,
            C - 1,
            C,
            C + 1,
            3 * C + 12_345,
            16 * C,
            (1 << 20) + 7,
        ];
        for (n, &size) in sizes.iter().enumerate() {
            let mem = mem(size, 0xC0FFEE + n as u64);
            let cache = Arc::new(ChunkCache::new(8 * CHUNK));
            let src = cached(mem.clone(), &cache);
            let len = size as u64;

            // Edges: chunk boundaries, the short last chunk, empty reads.
            let mut edges = vec![(0, 0), (len, 0), (0, size), (len - 1, 1)];
            for k in 1..=(size / C) {
                let b = (k * C) as u64;
                for (o, l) in [(b - 1, 2), (b - 1, 1), (b, 1), (b, 0)] {
                    if o + l as u64 <= len {
                        edges.push((o, l));
                    }
                }
            }
            for (o, l) in edges {
                check_same(&*mem, &src, o, l);
            }

            let mut rng = Rng::new(size as u64 * 31 + 7);
            for _ in 0..400 {
                let offset = rng.upto(len);
                let max_len = (len - offset).min(3 * CHUNK + 100);
                let l = rng.upto(max_len) as usize;
                check_same(&*mem, &src, offset, l);
            }
        }
    }

    #[test]
    fn empty_source() {
        let mem: Arc<MemSource> = Arc::new(MemSource::new(Vec::new()));
        let cache = Arc::new(ChunkCache::default());
        let src = cached(mem, &cache);
        assert_eq!(src.len(), 0);
        assert!(src.is_empty());
        src.read_at(0, &mut []).unwrap();
        assert!(src.read_at(1, &mut []).is_err());
        assert!(src.read_at(0, &mut [0u8; 1]).is_err());
        assert_eq!(cache.stats(), CacheStats::default());
    }

    #[test]
    fn out_of_bounds_reads_nothing() {
        let size = 2 * C + 10;
        let cache = Arc::new(ChunkCache::default());
        let src = cached(mem(size, 1), &cache);
        let len = size as u64;
        for (offset, l) in [
            (len, 1),
            (len + 1, 0),
            (len - 5, 6),
            (0, size + 1),
            (u64::MAX, 1),
            (u64::MAX - 2, 10),
        ] {
            let mut buf = vec![0xAAu8; l];
            let err = src.read_at(offset, &mut buf).unwrap_err();
            assert!(matches!(err, Error::OutOfBounds { .. }), "{err:?}");
            assert!(buf.iter().all(|&b| b == 0xAA));
        }
        assert_eq!(cache.stats(), CacheStats::default());
        assert_eq!(cache.cached_chunks(), 0);
    }

    #[test]
    fn rejects_bad_sector_sizes() {
        let cache = Arc::new(ChunkCache::default());
        for bad in [0, 256, 511, 1000, 3000, 131_072] {
            let inner = Arc::new(MemSource::with_sector_size(vec![0u8; 4096], bad));
            let err = CachedSource::new(inner, Arc::clone(&cache)).err().unwrap();
            assert!(matches!(err, Error::Unsupported(_)), "{bad}: {err:?}");
        }
        for good in [512, 4096, 65_536] {
            let inner = Arc::new(MemSource::with_sector_size(vec![0u8; 4096], good));
            let src = CachedSource::new(inner, Arc::clone(&cache)).unwrap();
            assert_eq!(src.sector_size(), good);
        }
    }

    #[test]
    fn keys_never_repeat() {
        let cache = Arc::new(ChunkCache::default());
        let a = cached(mem(10, 1), &cache).key;
        let b = cached(mem(10, 2), &cache).key;
        let c = cached(mem(10, 3), &cache).key;
        assert!(a != b && b != c && a != c);
    }

    #[test]
    fn tiny_budget_still_correct_and_evicts() {
        let size = 64 * C;
        let mem = mem(size, 9);
        let cache = Arc::new(ChunkCache::new(0));
        let src = cached(mem.clone(), &cache);
        // Every chunk once, out of order so no read-ahead.
        let order: Vec<u64> = (0..64u64).map(|i| (i * 37) % 64).collect();
        for &c in &order {
            check_same(&*mem, &src, c * CHUNK + 3, C - 6);
        }
        assert!(cache.cached_chunks() <= SHARDS);
        let before = cache.stats().inner_reads;
        for &c in &order {
            check_same(&*mem, &src, c * CHUNK + 3, C - 6);
        }
        assert!(cache.stats().inner_reads > before);
        assert!(cache.cached_chunks() <= SHARDS);
    }

    #[test]
    fn hot_range_hits_without_inner_reads() {
        let mem = mem(8 * C, 4);
        let cache = Arc::new(ChunkCache::default());
        let src = cached(mem.clone(), &cache);
        check_same(&*mem, &src, 1000, 2 * C);
        let warm = cache.stats();
        for _ in 0..10 {
            check_same(&*mem, &src, 1000, 2 * C);
        }
        let hot = cache.stats();
        assert_eq!(hot.inner_reads, warm.inner_reads);
        assert_eq!(hot.inner_bytes, warm.inner_bytes);
        assert_eq!(hot.misses, warm.misses);
        // 1000..1000 + 2C touches chunks 0, 1 and 2.
        assert_eq!(hot.hits - warm.hits, 30);
    }

    #[test]
    fn sequential_scan_reads_ahead() {
        let size = 2 << 20; // 32 chunks
        let mem = mem(size, 5);
        let cache = Arc::new(ChunkCache::default());
        let src = cached(mem.clone(), &cache);
        let mut offset = 0;
        while offset < size as u64 {
            check_same(&*mem, &src, offset, 4096);
            offset += 4096;
        }
        let s = cache.stats();
        // Fetch sizes: 1 (first miss, not sequential), then windows of
        // 2, 4, 8 and 16 chunks, then the 1 chunk left: 6 inner reads for
        // 32 chunks, and no chunk read twice.
        assert_eq!(s.inner_reads, 6);
        assert_eq!(s.inner_bytes, size as u64);
        assert_eq!(s.misses, 6);
        assert_eq!(s.hits, 512 - 6);
    }

    #[test]
    fn read_ahead_stops_at_cached_chunk_and_stays_sequential() {
        let size = 2 << 20; // 32 chunks
        let mem = mem(size, 6);
        let cache = Arc::new(ChunkCache::default());
        let src = cached(mem.clone(), &cache);
        check_same(&*mem, &src, 5 * CHUNK, 10);
        let mut offset = 0;
        while offset < size as u64 {
            check_same(&*mem, &src, offset, 4096);
            offset += 4096;
        }
        let s = cache.stats();
        // Chunk 5, then 0, 1..3, 3..5 (stops at cached 5), hit on 5 moves
        // the mark, 6..14, 14..30, 30..32.
        assert_eq!(s.inner_reads, 7);
        assert_eq!(s.inner_bytes, size as u64);
    }

    #[test]
    fn random_miss_resets_window() {
        let size = 64 * C;
        let mem = mem(size, 7);
        let cache = Arc::new(ChunkCache::default());
        let src = cached(mem.clone(), &cache);
        // Sequential misses at 0, 1, 3, 7 grow the window to 16.
        for c in [0u64, 1, 3, 7] {
            check_same(&*mem, &src, c * CHUNK, 1);
        }
        assert_eq!(src.window.load(Ordering::Relaxed), 16);
        // A miss elsewhere fetches only its own chunk and resets the window.
        let before = cache.stats();
        check_same(&*mem, &src, 40 * CHUNK, 1);
        let after = cache.stats();
        assert_eq!(after.inner_bytes - before.inner_bytes, CHUNK);
        assert_eq!(src.window.load(Ordering::Relaxed), MIN_WINDOW);
        // The next sequential miss fetches the reset window of 2 chunks.
        check_same(&*mem, &src, 41 * CHUNK, 1);
        let next = cache.stats();
        assert_eq!(next.inner_bytes - after.inner_bytes, 2 * CHUNK);
        assert_eq!(src.window.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn read_ahead_clamps_to_source_end() {
        let size = 3 * C + 100; // 4 chunks, last is 100 bytes
        let mem = mem(size, 8);
        let cache = Arc::new(ChunkCache::default());
        let src = cached(mem.clone(), &cache);
        check_same(&*mem, &src, 0, 1);
        check_same(&*mem, &src, CHUNK, 1); // window 2: chunks 1..3
        check_same(&*mem, &src, 3 * CHUNK, 1); // window 4, clamped: chunk 3
        let s = cache.stats();
        assert_eq!(s.inner_bytes, size as u64);
        assert_eq!(s.inner_reads, 3);
        check_same(&*mem, &src, 0, size);
        assert_eq!(cache.stats().inner_reads, 3);
    }

    #[test]
    fn miss_run_is_one_inner_read() {
        let mem = mem(10 * C, 10);
        let cache = Arc::new(ChunkCache::default());
        let src = cached(mem.clone(), &cache);
        // Cache chunk 3, then read chunks 1..=6: runs 1..3 and 4..7.
        check_same(&*mem, &src, 3 * CHUNK, 1);
        let before = cache.stats();
        check_same(&*mem, &src, CHUNK + 5, 5 * C);
        let after = cache.stats();
        assert_eq!(after.inner_reads - before.inner_reads, 2);
        assert_eq!(after.hits - before.hits, 1);
        assert_eq!(after.misses - before.misses, 5);
    }

    #[test]
    fn read_bulk_bypasses_cache() {
        let mem = mem(8 * C, 11);
        let cache = Arc::new(ChunkCache::default());
        let src = cached(mem.clone(), &cache);
        let mut a = vec![0u8; 3 * C + 7];
        let mut b = vec![0u8; 3 * C + 7];
        src.read_bulk(100, &mut a).unwrap();
        mem.read_at(100, &mut b).unwrap();
        assert!(a == b);
        assert_eq!(cache.stats(), CacheStats::default());
        assert_eq!(cache.cached_chunks(), 0);
        assert!(src.read_bulk(8 * CHUNK, &mut [0u8; 1]).is_err());
        src.read_at(100, &mut a).unwrap();
        assert!(a == b);
        let s = cache.stats();
        assert_eq!(s.hits, 0);
        assert!(s.misses > 0);
    }

    /// Fails any read that overlaps `fail_from..fail_to` while armed.
    struct Flaky {
        inner: MemSource,
        fail_from: u64,
        fail_to: u64,
        armed: AtomicBool,
    }

    impl BlockSource for Flaky {
        fn len(&self) -> u64 {
            self.inner.len()
        }

        fn sector_size(&self) -> u32 {
            512
        }

        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
            check_range(offset, buf.len(), self.len())?;
            let end = offset + buf.len() as u64;
            if self.armed.load(Ordering::SeqCst) && offset < self.fail_to && end > self.fail_from {
                return Err(Error::io(
                    IoContext::Read {
                        offset,
                        len: buf.len() as u64,
                    },
                    io::Error::other("flaky"),
                ));
            }
            self.inner.read_at(offset, buf)
        }
    }

    #[test]
    fn inner_error_passes_through_and_caches_nothing() {
        let data = bytes(8 * C, 12);
        let flaky = Arc::new(Flaky {
            inner: MemSource::new(data.clone()),
            fail_from: 2 * CHUNK + 10,
            fail_to: 2 * CHUNK + 11,
            armed: AtomicBool::new(true),
        });
        let mem = MemSource::new(data);
        let cache = Arc::new(ChunkCache::default());
        let src = cached(flaky.clone(), &cache);

        check_same(&mem, &src, 0, 100); // chunk 0 fine
        let before = cache.stats();
        let held = cache.cached_chunks();
        let mut buf = vec![0u8; 2 * C];
        let err = src.read_at(CHUNK, &mut buf).unwrap_err();
        match err {
            Error::Io { source, .. } => assert_eq!(source.to_string(), "flaky"),
            other => panic!("expected the inner Io error, got {other:?}"),
        }
        let failed = cache.stats();
        assert_eq!(failed.inner_reads, before.inner_reads);
        assert_eq!(failed.inner_bytes, before.inner_bytes);
        assert_eq!(failed.misses, before.misses + 2);
        assert_eq!(cache.cached_chunks(), held);

        flaky.armed.store(false, Ordering::SeqCst);
        check_same(&mem, &src, CHUNK, 2 * C);
        let ok = cache.stats();
        assert_eq!(ok.inner_reads, failed.inner_reads + 1);
        assert_eq!(ok.misses, failed.misses + 2);
        check_same(&mem, &src, 0, 4 * C);
    }

    #[test]
    fn sources_sharing_a_cache_stay_apart() {
        let a_mem = mem(5 * C + 3, 100);
        let b_mem = mem(5 * C + 3, 200);
        let cache = Arc::new(ChunkCache::new(0));
        let a = cached(a_mem.clone(), &cache);
        let b = cached(b_mem.clone(), &cache);
        let mut rng = Rng::new(42);
        for _ in 0..500 {
            let offset = rng.upto(5 * CHUNK);
            let l = rng.upto(5 * CHUNK + 3 - offset).min(2 * CHUNK) as usize;
            check_same(&*a_mem, &a, offset, l);
            check_same(&*b_mem, &b, offset, l);
        }
    }

    #[test]
    fn concurrent_random_reads() {
        let size = 40 * C + 999;
        let mem = mem(size, 77);
        let cache = Arc::new(ChunkCache::new(4 * CHUNK));
        let src = cached(mem.clone(), &cache);
        let len = size as u64;
        std::thread::scope(|scope| {
            for t in 0..8u64 {
                let src = &src;
                let mem = &mem;
                scope.spawn(move || {
                    let mut rng = Rng::new(t * 1_000_003 + 1);
                    for i in 0..400 {
                        let offset = if i % 5 == 0 {
                            // Sequential stretches exercise read-ahead races.
                            (i as u64 / 5 % 40) * CHUNK
                        } else {
                            rng.upto(len)
                        };
                        let l = rng.upto((len - offset).min(3 * CHUNK)) as usize;
                        check_same(&**mem, src, offset, l);
                    }
                });
            }
        });
        let s = cache.stats();
        assert!(s.hits + s.misses > 0);
        assert!(cache.cached_chunks() <= SHARDS);
    }

    #[test]
    fn slice_over_cached_source() {
        let size = 6 * C + 77;
        let mem = mem(size, 13);
        let cache = Arc::new(ChunkCache::default());
        let src: Arc<dyn BlockSource> = Arc::new(cached(mem.clone(), &cache));
        let start = CHUNK - 300;
        let slen = 4 * CHUNK + 1;
        let slice = Slice::new(src, start, slen).unwrap();
        let mut rng = Rng::new(3);
        for _ in 0..200 {
            let offset = rng.upto(slen);
            let l = rng.upto(slen - offset).min(2 * CHUNK) as usize;
            let mut got = vec![0u8; l];
            let mut want = vec![0u8; l];
            slice.read_at(offset, &mut got).unwrap();
            mem.read_at(start + offset, &mut want).unwrap();
            assert!(got == want);
        }
        assert!(slice.read_at(slen, &mut [0u8; 1]).is_err());
    }

    #[test]
    fn clock_spares_referenced_slots() {
        let mut shard = ShardState {
            map: HashMap::new(),
            slots: Vec::new(),
            hand: 0,
            capacity: 2,
        };
        let chunk = |b: u8| -> Arc<[u8]> { Arc::from(vec![b; 4]) };
        shard.insert((0, 0), chunk(0));
        shard.insert((0, 1), chunk(1));
        assert!(shard.get((0, 0)).is_some()); // referenced
        shard.insert((0, 2), chunk(2)); // evicts chunk 1
        assert!(shard.contains((0, 0)));
        assert!(!shard.contains((0, 1)));
        assert_eq!(&*shard.get((0, 2)).unwrap(), &[2u8; 4]);
        // Re-inserting an existing chunk replaces it in place.
        shard.insert((0, 2), chunk(9));
        assert_eq!(&*shard.get((0, 2)).unwrap(), &[9u8; 4]);
        assert_eq!(shard.slots.len(), 2);
        assert_eq!(shard.map.len(), 2);
    }
}
