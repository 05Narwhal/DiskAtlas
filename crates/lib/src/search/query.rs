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

//! Queries over an index: parallel scan, filters, ranking.
//!
//! [`Index::search`] runs on every keystroke. The query text is folded and
//! split into [`terms`]; every term must occur in a name. The longest term is
//! searched with one shared [`Finder`] over the whole folded name buffer, in
//! chunks of about 1 MiB scanned in parallel. A hit is mapped to its entry by
//! galloping forward from the previous one, the entry is tested against the
//! other terms and the filters, ranked into a 64-bit key, and the search
//! resumes at the next name, so each name is examined at most once.
//!
//! When the new query only narrows the previous one, the previous hits are
//! tested instead of the whole range.
//!
//! Each parallel piece also puts its own [`HEAD`] smallest keys in order;
//! the head of the result is the merge of those, so no step is serial over
//! all hits (see `Hits::from_chunks`).
//!
//! Ranking (ascending key is result order):
//!
//! | bits  | meaning |
//! |-------|---------|
//! | 60–63 | class of the first term: 0 whole name, 1 prefix, 2 word start, 3 other |
//! | 59    | 0 for a folder, 1 for anything else |
//! | 48–58 | name length in bytes, saturated at 2047 |
//! | 32–47 | reserved, 0 |
//! | 0–31  | entry number (volume order, which is path order) |

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ops::{Range, RangeInclusive};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use memchr::memmem::Finder;
use rayon::prelude::*;

use crate::search::fold::{fold, fold_into};
use crate::search::index::{Index, IndexFlags, NO_SIZE, NO_TIME, NUL_REPLACEMENT};
use crate::search::kind::KindSet;

/// How many hits [`Index::search`] puts in final order before returning;
/// the rest are ordered by [`Hits::sort_all`].
pub const HEAD: usize = 4096;

/// Bytes of `folded` per parallel piece of a term scan.
const CHUNK_BYTES: usize = 1 << 20;
/// Entries per parallel piece when only filters are tested.
const FILTER_CHUNK: usize = 65_536;
/// Candidates per parallel piece when narrowing.
const NARROW_CHUNK: usize = 2_048;
/// Workers look at the cancel flag once per this many hits or entries
/// (a power of two minus one, used as a mask).
const CANCEL_MASK: usize = 4095;

/// Low 32 bits of a key: the entry.
const ENTRY_MASK: u64 = 0xFFFF_FFFF;
/// Largest name length stored in a key.
const MAX_KEY_LEN: u64 = 0x7FF;
const CLASS_SHIFT: u32 = 60;
const FILE_SHIFT: u32 = 59;
const LEN_SHIFT: u32 = 48;
/// Rank class when nothing better applies, and for filter-only queries.
const CLASS_OTHER: u64 = 3;

/// What to search for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    /// The text as typed; see [`terms`].
    pub text: String,
    /// A folder entry: search inside it only. A number that is not a folder
    /// gives no hits.
    pub scope: Option<u32>,
    /// Kind classes to admit; empty admits any.
    pub kinds: KindSet,
    /// Data size range in bytes. Folders and unknown sizes fail it.
    pub size: Option<RangeInclusive<u64>>,
    /// Modification time range in Unix nanoseconds. Unknown dates fail it.
    pub modified: Option<RangeInclusive<i64>>,
    /// Also admit hidden entries.
    pub include_hidden: bool,
    /// Also admit housekeeping items and everything inside them (entries
    /// with [`IndexFlags::SYSTEM_ITEM`], such as `.DS_Store` or the content
    /// of `.Spotlight-V100`). Independent of `include_hidden`: an entry that
    /// is both hidden and a housekeeping item needs both.
    pub include_system: bool,
}

impl Query {
    /// True when something other than the text restricts the results.
    /// `include_hidden` and `include_system` only widen them, so they are
    /// not filters.
    fn has_filters(&self) -> bool {
        self.scope.is_some()
            || !self.kinds.is_empty()
            || self.size.is_some()
            || self.modified.is_some()
    }

    /// True when both queries admit the same entries apart from the text.
    fn same_filters(&self, other: &Query) -> bool {
        self.scope == other.scope
            && self.kinds == other.kinds
            && self.size == other.size
            && self.modified == other.modified
            && self.include_hidden == other.include_hidden
            && self.include_system == other.include_system
    }
}

/// The result of one search: the hits, ranked.
///
/// `Clone`, `Send` and `Sync`, so a copy can be sorted on another thread.
#[derive(Debug, Clone, Default)]
pub struct Hits {
    /// Rank keys (see the module docs); the low 32 bits are the entry.
    keys: Vec<u64>,
    /// `keys[..sorted]` are in final order, and none after is smaller.
    sorted: usize,
    /// The same hits ascending by entry, for narrowing and column orders.
    by_entry: Vec<u32>,
    /// True when these hits came from a real plan (terms or filters). The
    /// empty `Hits` of a query with neither is never a narrowing base.
    planned: bool,
    /// Time from the start of [`Index::search`] to its return.
    pub elapsed: Duration,
}

impl Hits {
    /// Hits from the pieces the workers return (each ascending by entry, in
    /// entry order), with the head put in final order.
    ///
    /// Every piece is handled in parallel: its entries are written to its
    /// own part of `by_entry`, and its [`HEAD`] smallest keys are selected
    /// and sorted at its front. The global head is the k-way merge of those
    /// sorted fronts (every key of the global head is among the `HEAD`
    /// smallest of its own piece); the rest of every piece is then copied
    /// behind it in parallel. Keys hold their entry number, so no two are
    /// equal and the head is the same as a serial select of all keys gives.
    /// The tail holds the same set in another arrangement, which the type
    /// leaves unspecified.
    fn from_chunks(mut chunks: Vec<Vec<u64>>) -> Self {
        let lens: Vec<usize> = chunks.iter().map(Vec::len).collect();
        let total = lens.iter().fold(0usize, |sum, &l| sum.saturating_add(l));
        let mut by_entry = vec![0u32; total];
        split_lengths(&mut by_entry, &lens)
            .into_par_iter()
            .zip(chunks.par_iter_mut())
            .for_each(|(out, chunk)| {
                for (slot, &key) in out.iter_mut().zip(chunk.iter()) {
                    *slot = entry_of(key);
                }
                if total > HEAD {
                    sort_front(chunk);
                }
            });
        let (keys, sorted) = if total <= HEAD {
            serial_keys(&chunks)
        } else {
            merged_keys(&chunks, total).unwrap_or_else(|| serial_keys(&chunks))
        };
        Self {
            keys,
            sorted,
            by_entry,
            planned: true,
            elapsed: Duration::ZERO,
        }
    }

    /// Number of hits.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// True when there is no hit.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The entry at `position` in result order. Positions from
    /// [`sorted_len`](Self::sorted_len) on hold the right set of hits but
    /// not yet in order. `None` past the end.
    pub fn entry(&self, position: usize) -> Option<u32> {
        self.keys.get(position).map(|&k| entry_of(k))
    }

    /// How many leading positions are in final order.
    pub fn sorted_len(&self) -> usize {
        self.sorted
    }

    /// Sorts everything. Call on a background thread when the view nears
    /// [`sorted_len`](Self::sorted_len).
    pub fn sort_all(&mut self) {
        if let Some(tail) = self.keys.get_mut(self.sorted..) {
            // Every key in the tail is larger than every key in the head.
            tail.par_sort_unstable();
        }
        self.sorted = self.keys.len();
    }

    /// The entries in result order: the listing's `order`.
    pub fn entries_in_order(&self) -> Arc<[u32]> {
        self.keys.iter().map(|&k| entry_of(k)).collect()
    }

    /// The same hits ascending by entry number.
    pub fn by_entry(&self) -> &[u32] {
        &self.by_entry
    }
}

/// The entry number in a rank key.
fn entry_of(key: u64) -> u32 {
    u32::try_from(key & ENTRY_MASK).unwrap_or(u32::MAX)
}

/// `buf` cut into consecutive pieces of `lens`. A length past what is left
/// gets what is left, so the pieces never overrun.
fn split_lengths<'a, T>(mut buf: &'a mut [T], lens: &[usize]) -> Vec<&'a mut [T]> {
    let mut out = Vec::with_capacity(lens.len());
    for &len in lens {
        let rest = std::mem::take(&mut buf);
        let (piece, after) = rest.split_at_mut(len.min(rest.len()));
        out.push(piece);
        buf = after;
    }
    out
}

/// Puts the [`HEAD`] smallest keys of `piece` (all of them when it has no
/// more) at its front, ascending; the rest follow in no order.
fn sort_front(piece: &mut [u64]) {
    if piece.len() > HEAD {
        piece.select_nth_unstable(HEAD);
    }
    let front = piece.len().min(HEAD);
    if let Some(front) = piece.get_mut(..front) {
        front.sort_unstable();
    }
}

/// The reference arrangement: all keys in piece order, the head selected
/// and sorted serially. Used when the hits fit in the head, and as the
/// fallback of [`merged_keys`].
fn serial_keys(chunks: &[Vec<u64>]) -> (Vec<u64>, usize) {
    let total = chunks
        .iter()
        .fold(0usize, |sum, c| sum.saturating_add(c.len()));
    let mut keys = Vec::with_capacity(total);
    for chunk in chunks {
        keys.extend_from_slice(chunk);
    }
    let sorted = if keys.len() <= HEAD {
        keys.sort_unstable();
        keys.len()
    } else {
        keys.select_nth_unstable(HEAD);
        if let Some(head) = keys.get_mut(..HEAD) {
            head.sort_unstable();
        }
        HEAD
    };
    (keys, sorted)
}

/// All keys with the global head first, for pieces whose front was put in
/// order by [`sort_front`] and whose lengths sum to `total`, more than
/// [`HEAD`].
///
/// The head is a k-way merge of the sorted fronts; equal keys (which keys
/// holding their entry never are) come from the lower piece first. Then the
/// rest of each piece is copied behind the head, every piece into its own
/// part in parallel. Every key behind the head is at least the head's last:
/// a key a piece did not give to the merge is not smaller than the one the
/// merge stopped at in that piece.
///
/// `None` when the fronts cannot fill the head or the pieces do not add up
/// to `total`, which cannot happen for consistent input; the caller then
/// falls back to [`serial_keys`].
fn merged_keys(chunks: &[Vec<u64>], total: usize) -> Option<(Vec<u64>, usize)> {
    let mut keys = vec![0u64; total];
    let (head, rest) = keys.split_at_mut_checked(HEAD)?;
    // How many keys of each piece's front went to the head.
    let mut taken = vec![0usize; chunks.len()];
    let mut heap: BinaryHeap<Reverse<(u64, usize)>> = chunks
        .iter()
        .enumerate()
        .filter_map(|(i, c)| c.first().map(|&k| Reverse((k, i))))
        .collect();
    for slot in head.iter_mut() {
        let Reverse((key, i)) = heap.pop()?;
        *slot = key;
        let (Some(chunk), Some(next)) = (chunks.get(i), taken.get_mut(i)) else {
            return None;
        };
        *next = next.saturating_add(1);
        if *next < chunk.len().min(HEAD)
            && let Some(&k) = chunk.get(*next)
        {
            heap.push(Reverse((k, i)));
        }
    }
    let rest_lens: Vec<usize> = chunks
        .iter()
        .zip(&taken)
        .map(|(chunk, &t)| chunk.len().saturating_sub(t))
        .collect();
    let filled = rest_lens.iter().fold(HEAD, |sum, &l| sum.saturating_add(l));
    if filled != total {
        return None;
    }
    split_lengths(rest, &rest_lens)
        .into_par_iter()
        .zip(chunks.par_iter().zip(taken.par_iter()))
        .for_each(|(out, (chunk, &t))| {
            if let Some(src) = chunk.get(t..)
                && src.len() == out.len()
            {
                out.copy_from_slice(src);
            }
        });
    Some((keys, HEAD))
}

/// The query text folded ([`fold`]), split on whitespace (Unicode
/// `White_Space`), empty pieces removed, duplicates removed keeping the
/// first. A NUL becomes [`NUL_REPLACEMENT`], as it is stored in names, so a
/// term never holds the 0 byte that separates names.
pub fn terms(text: &str) -> Vec<String> {
    let folded = fold(text);
    let mut out: Vec<String> = Vec::new();
    for word in folded.split_whitespace() {
        let word = word.replace('\0', &char::from(NUL_REPLACEMENT).to_string());
        if !word.is_empty() && !out.contains(&word) {
            out.push(word);
        }
    }
    out
}

/// Clears `out`, then fills it with the byte ranges of `name` where any of
/// `terms` (already folded, as [`terms`] gives them) occurs, every
/// occurrence of every term, sorted, overlapping and touching ranges
/// merged. Every range lies on character boundaries of `name`. For bold
/// text in a visible row; not for the scan.
pub fn match_ranges(name: &str, terms: &[String], out: &mut Vec<Range<usize>>) {
    out.clear();
    let mut folded = Vec::with_capacity(name.len());
    fold_into(name, &mut folded);
    if folded.len() != name.len() {
        return;
    }
    for term in terms {
        let needle = term.as_bytes();
        if needle.is_empty() {
            continue;
        }
        let finder = Finder::new(needle);
        let mut pos = 0usize;
        while let Some(rest) = folded.get(pos..) {
            let Some(found) = finder.find(rest) else {
                break;
            };
            let start = pos.saturating_add(found);
            let end = start.saturating_add(needle.len());
            if name.is_char_boundary(start) && name.is_char_boundary(end) {
                out.push(start..end);
            }
            pos = start.saturating_add(1);
        }
    }
    out.sort_unstable_by_key(|r| (r.start, r.end));
    // `later` is removed when it overlaps or touches the kept `earlier`.
    out.dedup_by(|later, earlier| {
        if later.start <= earlier.end {
            earlier.end = earlier.end.max(later.end);
            true
        } else {
            false
        }
    });
}

/// True when the search should stop.
fn cancelled(cancel: &AtomicBool) -> bool {
    cancel.load(Ordering::Relaxed)
}

/// Marker for a worker that saw the cancel flag.
struct Cancelled;

/// Everything a worker needs to test and rank an entry. Shared by
/// reference across workers.
struct Scan<'a> {
    names: &'a [u8],
    folded: &'a [u8],
    offsets: &'a [u32],
    flags: &'a [u16],
    sizes: &'a [u64],
    times: &'a [i64],
    /// Flag bits that keep an entry out: [`IndexFlags::HIDDEN`] and
    /// [`IndexFlags::SYSTEM_ITEM`] unless the query admits them.
    excluded: u16,
    kinds: KindSet,
    size: Option<(u64, u64)>,
    modified: Option<(i64, i64)>,
    /// Terms the entry must still contain (all but the scanned one, or all
    /// of them when narrowing).
    check: Vec<Finder<'a>>,
    /// The first term as typed, for the rank class.
    rank: Option<Finder<'a>>,
    cancel: &'a AtomicBool,
}

impl Scan<'_> {
    /// Byte range of name `e` in `names` and `folded`, without separator.
    fn name_range(&self, e: usize) -> Option<Range<usize>> {
        let start = *self.offsets.get(e)? as usize;
        let end = (*self.offsets.get(e.checked_add(1)?)? as usize).checked_sub(1)?;
        (start <= end).then_some(start..end)
    }

    /// Step 5 and 6: the rank key of entry `e` when it passes the filters
    /// and contains every term in `check`; `None` otherwise.
    fn admit(&self, e: usize) -> Option<u64> {
        let f = *self.flags.get(e)?;
        if f & self.excluded != 0 {
            return None;
        }
        if !self.kinds.is_empty() && !self.kinds.admits(IndexFlags::from_bits(f).kind_class()) {
            return None;
        }
        if let Some((lo, hi)) = self.size {
            let s = *self.sizes.get(e)?;
            if s == NO_SIZE || s < lo || s > hi {
                return None;
            }
        }
        if let Some((lo, hi)) = self.modified {
            let t = *self.times.get(e)?;
            if t == NO_TIME || t < lo || t > hi {
                return None;
            }
        }
        let range = self.name_range(e)?;
        let start = range.start;
        let name = self.folded.get(range)?;
        for term in &self.check {
            term.find(name)?;
        }
        let class = self.rank_class(start, name);
        let is_file = u64::from(f & IndexFlags::KIND_MASK != IndexFlags::KIND_FOLDER);
        let len = (name.len() as u64).min(MAX_KEY_LEN);
        let entry = (e as u64) & ENTRY_MASK;
        Some(
            class.wrapping_shl(CLASS_SHIFT)
                | is_file.wrapping_shl(FILE_SHIFT)
                | len.wrapping_shl(LEN_SHIFT)
                | entry,
        )
    }

    /// The best class of the rank term over its occurrences in `name` (the
    /// folded name that starts at byte `start`). No allocation.
    fn rank_class(&self, start: usize, name: &[u8]) -> u64 {
        let Some(finder) = &self.rank else {
            return CLASS_OTHER;
        };
        let term = finder.needle();
        if name == term {
            return 0;
        }
        if name.starts_with(term) {
            return 1;
        }
        // No occurrence at 0 (else the name would start with the term).
        let mut pos = 1usize;
        while let Some(rest) = name.get(pos..) {
            let Some(found) = finder.find(rest) else {
                break;
            };
            let p = pos.saturating_add(found);
            if self.word_start(start, name, p) {
                return 2;
            }
            pos = p.saturating_add(1);
        }
        CLASS_OTHER
    }

    /// True when byte `p > 0` of the name at `start` begins a word: the byte
    /// before is ASCII and not alphanumeric, or the original name steps
    /// from an ASCII lowercase letter to an ASCII uppercase one there.
    fn word_start(&self, start: usize, name: &[u8], p: usize) -> bool {
        let Some(before) = p.checked_sub(1) else {
            return true;
        };
        let Some(&prev) = name.get(before) else {
            return false;
        };
        if prev < 0x80 && !prev.is_ascii_alphanumeric() {
            return true;
        }
        let original = |at: usize| start.checked_add(at).and_then(|i| self.names.get(i));
        matches!(
            (original(before), original(p)),
            (Some(a), Some(b)) if a.is_ascii_lowercase() && b.is_ascii_uppercase()
        )
    }

    /// Step 4 for one chunk of entries: every entry whose folded name holds
    /// `term`, tested and ranked, ascending by entry.
    fn scan_chunk(&self, term: &Finder<'_>, entries: Range<usize>) -> Result<Vec<u64>, Cancelled> {
        if cancelled(self.cancel) {
            return Err(Cancelled);
        }
        let mut out = Vec::new();
        let (Some(&base), Some(&end)) = (
            self.offsets.get(entries.start),
            self.offsets.get(entries.end),
        ) else {
            return Ok(out);
        };
        let (base, end) = (base as usize, end as usize);
        let Some(hay) = self.folded.get(base..end) else {
            return Ok(out);
        };
        let mut e = entries.start;
        let mut at = 0usize;
        let mut seen = 0usize;
        while e < entries.end {
            let Some(rest) = hay.get(at..) else {
                break;
            };
            let Some(found) = term.find(rest) else {
                break;
            };
            // A term holds no 0 byte, so the match lies inside one name.
            let p = base.saturating_add(at).saturating_add(found);
            e = entry_at(self.offsets, e, entries.end, p);
            if let Some(key) = self.admit(e) {
                out.push(key);
            }
            // Resume at the next name: each name is examined once.
            e = e.saturating_add(1);
            let Some(&next) = self.offsets.get(e) else {
                break;
            };
            at = (next as usize).saturating_sub(base);
            seen = seen.wrapping_add(1);
            if seen & CANCEL_MASK == 0 && cancelled(self.cancel) {
                return Err(Cancelled);
            }
        }
        Ok(out)
    }

    /// Step 4 without terms: test the filters of every entry in `entries`.
    fn filter_chunk(&self, entries: Range<usize>) -> Result<Vec<u64>, Cancelled> {
        let mut out = Vec::new();
        for e in entries {
            if e & CANCEL_MASK == 0 && cancelled(self.cancel) {
                return Err(Cancelled);
            }
            if let Some(key) = self.admit(e) {
                out.push(key);
            }
        }
        if cancelled(self.cancel) {
            return Err(Cancelled);
        }
        Ok(out)
    }

    /// Step 3: test each candidate with every term and filter.
    fn narrow_chunk(&self, candidates: &[u32]) -> Result<Vec<u64>, Cancelled> {
        if cancelled(self.cancel) {
            return Err(Cancelled);
        }
        let mut out = Vec::new();
        for (k, &e) in candidates.iter().enumerate() {
            if k & CANCEL_MASK == CANCEL_MASK && cancelled(self.cancel) {
                return Err(Cancelled);
            }
            if let Some(key) = self.admit(e as usize) {
                out.push(key);
            }
        }
        Ok(out)
    }
}

/// The entry containing byte `p`: the largest `j` in `lo..hi` with
/// `offsets[j] <= p`, given `offsets[lo] <= p`. Gallops forward from `lo`
/// (doubling the step), then searches the last step by bisection, so the
/// cost is logarithmic in the distance moved, not in the chunk.
fn entry_at(offsets: &[u32], lo: usize, hi: usize, p: usize) -> usize {
    let at_most_p = |j: usize| offsets.get(j).is_some_and(|&o| o as usize <= p);
    let mut lo = lo;
    let mut step = 1usize;
    loop {
        let next = lo.saturating_add(step);
        if next >= hi || !at_most_p(next) {
            break;
        }
        lo = next;
        step = step.saturating_mul(2);
    }
    let top = lo.saturating_add(step).min(hi);
    let window = offsets.get(lo.saturating_add(1)..top).unwrap_or(&[]);
    lo.saturating_add(window.partition_point(|&o| o as usize <= p))
}

/// Entry ranges of `range` that each cover about `CHUNK_BYTES` of names.
fn byte_chunks(offsets: &[u32], range: Range<usize>) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut a = range.start;
    while a < range.end {
        let Some(&start) = offsets.get(a) else {
            break;
        };
        let want = (start as usize).saturating_add(CHUNK_BYTES);
        let after = a.saturating_add(1);
        let rest = offsets.get(after..range.end).unwrap_or(&[]);
        let b = after.saturating_add(rest.partition_point(|&o| (o as usize) < want));
        out.push(a..b);
        a = b;
    }
    out
}

/// Index of the longest term; the first of equal ones.
fn primary_term(terms: &[String]) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None;
    for (i, t) in terms.iter().enumerate() {
        if best.is_none_or(|(_, len)| t.len() > len) {
            best = Some((i, t.len()));
        }
    }
    best.map(|(i, _)| i)
}

/// The flag bits whose entries `query` leaves out.
fn excluded_flags(query: &Query) -> u16 {
    let mut bits = 0;
    if !query.include_hidden {
        bits |= IndexFlags::HIDDEN;
    }
    if !query.include_system {
        bits |= IndexFlags::SYSTEM_ITEM;
    }
    bits
}

/// Step 3's condition for using `previous` as the candidate set.
fn can_narrow(
    query: &Query,
    terms: &[String],
    previous: (&Query, &Hits),
    range_len: usize,
) -> bool {
    let (prev_query, prev_hits) = previous;
    prev_hits.planned
        && query.same_filters(prev_query)
        && prev_hits.len().saturating_mul(4) < range_len
        && self::terms(&prev_query.text)
            .iter()
            .all(|p| terms.iter().any(|t| t.contains(p.as_str())))
}

impl Index {
    /// Searches the index. `previous` is the last query and its hits on
    /// this same index; when the new query only narrows it, its hits are
    /// tested instead of the whole volume.
    ///
    /// A query with no terms and no filter gives empty hits (the app shows
    /// the folder instead). The root and the scope folder are never hits.
    ///
    /// `None` when `cancel` was set before the scan finished.
    pub fn search(
        &self,
        query: &Query,
        previous: Option<(&Query, &Hits)>,
        cancel: &AtomicBool,
    ) -> Option<Hits> {
        let started = Instant::now();
        if cancelled(cancel) {
            return None;
        }
        let terms = terms(&query.text);
        if terms.is_empty() && !query.has_filters() {
            return Some(Hits {
                elapsed: started.elapsed(),
                ..Hits::default()
            });
        }
        let range = self.query_range(query.scope);
        let base = previous.filter(|&p| can_narrow(query, &terms, p, range.len()));
        let primary = primary_term(&terms);

        let finders: Vec<Finder<'_>> = terms.iter().map(|t| Finder::new(t.as_bytes())).collect();
        let check: Vec<Finder<'_>> = match (base, primary) {
            (None, Some(p)) => finders
                .iter()
                .enumerate()
                .filter(|&(i, _)| i != p)
                .map(|(_, f)| f.clone())
                .collect(),
            _ => finders.clone(),
        };
        let scan = Scan {
            names: self.names_bytes(),
            folded: self.folded_bytes(),
            offsets: self.offsets(),
            flags: self.flag_words(),
            sizes: self.sizes(),
            times: self.modified_times(),
            excluded: excluded_flags(query),
            kinds: query.kinds,
            size: query.size.as_ref().map(|r| (*r.start(), *r.end())),
            modified: query.modified.as_ref().map(|r| (*r.start(), *r.end())),
            check,
            rank: finders.first().cloned(),
            cancel,
        };

        let chunks: Result<Vec<Vec<u64>>, Cancelled> = match (base, primary) {
            (Some((_, prev)), _) => {
                let by = prev.by_entry();
                let lo = by.partition_point(|&e| (e as usize) < range.start);
                let hi = by.partition_point(|&e| (e as usize) < range.end);
                let candidates = by.get(lo..hi).unwrap_or(&[]);
                candidates
                    .par_chunks(NARROW_CHUNK)
                    .map(|c| scan.narrow_chunk(c))
                    .collect()
            }
            (None, Some(p)) => {
                let term = finders.get(p)?;
                byte_chunks(scan.offsets, range)
                    .into_par_iter()
                    .map(|c| scan.scan_chunk(term, c))
                    .collect()
            }
            (None, None) => {
                let starts: Vec<usize> = range.clone().step_by(FILTER_CHUNK).collect();
                starts
                    .into_par_iter()
                    .map(|s| scan.filter_chunk(s..s.saturating_add(FILTER_CHUNK).min(range.end)))
                    .collect()
            }
        };
        let mut hits = Hits::from_chunks(chunks.ok()?);
        if cancelled(cancel) {
            return None;
        }
        hits.elapsed = started.elapsed();
        Some(hits)
    }

    /// Step 2: the entries a query looks at. The whole index but the root,
    /// or the subtree of the scope folder without the folder; empty for a
    /// scope that is not a folder.
    fn query_range(&self, scope: Option<u32>) -> Range<usize> {
        let len = self.len() as usize;
        match scope {
            None => 1.min(len)..len,
            Some(s) if self.is_folder(s) => {
                let r = self.subtree(s);
                let end = r.end as usize;
                (r.start as usize).saturating_add(1).min(end)..end
            }
            Some(_) => 0..0,
        }
    }
}

/// Small and generated indexes for the tests of this module and `order.rs`.
#[cfg(test)]
pub(super) mod fixtures {
    use crate::fs::{EntryFlags, NodeKind, ScanSummary};
    use crate::mac::system_item;
    use crate::search::index::{Index, IndexFlags, IndexParts, NO_RSIZE, NO_SIZE, NO_TIME};
    use crate::search::kind::classify_entry;

    /// One entry of a test index, in pre-order.
    #[derive(Debug, Clone)]
    pub struct Spec {
        pub name: String,
        pub parent: u32,
        pub kind: NodeKind,
        pub size: Option<u64>,
        pub modified: Option<i64>,
        pub rsize: Option<u32>,
        pub flags: EntryFlags,
    }

    impl Spec {
        pub fn size(mut self, s: u64) -> Self {
            self.size = Some(s);
            self
        }
        pub fn no_size(mut self) -> Self {
            self.size = None;
            self
        }
        pub fn time(mut self, t: i64) -> Self {
            self.modified = Some(t);
            self
        }
        pub fn rsize(mut self, r: u32) -> Self {
            self.rsize = Some(r);
            self
        }
        pub fn hidden(mut self) -> Self {
            self.flags = self.flags.with(EntryFlags::HIDDEN);
            self
        }
        pub fn links_to_folder(mut self) -> Self {
            self.flags = self.flags.with(EntryFlags::LINKS_TO_DIRECTORY);
            self
        }
    }

    pub fn root(name: &str) -> Spec {
        dir(name, 0)
    }

    pub fn dir(name: &str, parent: u32) -> Spec {
        Spec {
            name: name.to_owned(),
            parent,
            kind: NodeKind::Directory,
            size: None,
            modified: None,
            rsize: None,
            flags: EntryFlags::EMPTY,
        }
    }

    pub fn file(name: &str, parent: u32) -> Spec {
        Spec {
            kind: NodeKind::File,
            size: Some(10),
            ..dir(name, parent)
        }
    }

    pub fn link(name: &str, parent: u32) -> Spec {
        Spec {
            kind: NodeKind::Symlink,
            ..dir(name, parent)
        }
    }

    /// An index of `specs`; entry 0 is the root.
    pub fn index(specs: &[Spec]) -> Index {
        let mut names = String::new();
        let mut offsets = vec![0u32];
        for s in specs {
            names.push_str(&s.name.replace('\0', "\u{1a}"));
            names.push('\0');
            offsets.push(names.len() as u32);
        }
        let n = specs.len();
        Index::from_parts(IndexParts {
            names: names.into_boxed_str(),
            offsets: offsets.into(),
            parent: specs.iter().map(|s| s.parent).collect(),
            node: (0..n as u64).map(|i| 100 + i).collect(),
            size: specs
                .iter()
                .map(|s| match s.kind {
                    NodeKind::Directory => NO_SIZE,
                    _ => s.size.unwrap_or(NO_SIZE),
                })
                .collect(),
            modified: specs
                .iter()
                .map(|s| s.modified.unwrap_or(NO_TIME))
                .collect(),
            rsize: specs.iter().map(|s| s.rsize.unwrap_or(NO_RSIZE)).collect(),
            flags: flags(specs).into(),
            identity: None,
            summary: ScanSummary::default(),
        })
    }

    /// The flag words of `specs`, bit 6 set as the build sets it: on a
    /// housekeeping item (a direct child of entry 0 is at the root) and on
    /// everything inside one.
    fn flags(specs: &[Spec]) -> Vec<u16> {
        let mut out: Vec<u16> = specs
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let class = classify_entry(&s.name, s.kind == NodeKind::Directory, None);
                let flags = IndexFlags::new(s.kind, s.flags, s.rsize.is_some_and(|r| r > 0), class);
                let at_root = i != 0 && s.parent == 0;
                let hidden = s.flags.contains(EntryFlags::HIDDEN);
                if i != 0 && system_item(&s.name, s.kind, at_root, hidden).is_some() {
                    flags.with_system_item().bits()
                } else {
                    flags.bits()
                }
            })
            .collect();
        for i in 1..out.len() {
            let p = specs[i].parent as usize;
            if p < i && out[p] & IndexFlags::SYSTEM_ITEM != 0 {
                out[i] |= IndexFlags::SYSTEM_ITEM;
            }
        }
        out
    }

    pub struct Rng(pub u64);

    impl Rng {
        pub fn next(&mut self) -> u64 {
            // splitmix64
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        pub fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
            &items[self.below(items.len() as u64) as usize]
        }
    }

    /// The word list of build.rs's generator, plus one rare word.
    pub const WORDS: [&str; 24] = [
        "alpha",
        "Budget",
        "photo",
        "Report",
        "draft",
        "MUSIC",
        "notes",
        "Archive",
        "caf\u{e9}",
        "projet",
        "\u{3a3}\u{3b7}\u{3bc}\u{3b5}\u{3af}\u{3c9}\u{3c3}\u{3b7}",
        "\u{444}\u{430}\u{439}\u{43b}",
        "\u{65e5}\u{672c}",
        "final",
        "copy",
        "summer",
        "invoice",
        "scan",
        "backup",
        "old",
        "New",
        "data",
        "img",
        "track",
    ];
    /// One name in this many holds [`RARE`].
    pub const RARE_EVERY: u64 = 20_000;
    pub const RARE: &str = "Zeppelin";
    pub const EXTS: [&str; 12] = [
        "txt", "pdf", "png", "rs", "mp3", "mov", "zip", "dmg", "ttf", "c", "JPG", "doc",
    ];

    /// A pre-order tree of `n + 1` entries in the manner of build.rs's
    /// generator: folders of 1-200 entries, depth up to 12, names of 1-4
    /// words, a serial number and an extension; sizes, dates, hidden flags
    /// at random.
    pub fn generated(n: usize, seed: u64) -> Vec<Spec> {
        let mut rng = Rng(seed);
        let mut specs = vec![root("Generated")];
        // (folder entry, depth, children still to emit)
        let mut stack: Vec<(u32, u32, u64)> = Vec::new();
        let mut name = String::new();
        let mut serial = 0u64;
        while specs.len() <= n {
            if stack.is_empty() {
                stack.push((0, 0, 1 + rng.below(200)));
            }
            let top = stack.last_mut().unwrap();
            if top.2 == 0 {
                stack.pop();
                continue;
            }
            top.2 -= 1;
            let (parent, depth) = (top.0, top.1);
            let is_folder = depth < 12 && rng.below(6) == 0;
            name.clear();
            for w in 0..=rng.below(4) {
                if w > 0 {
                    name.push(' ');
                }
                if rng.below(RARE_EVERY) == 0 {
                    name.push_str(RARE);
                } else {
                    name.push_str(rng.pick(&WORDS));
                }
            }
            serial += 1;
            name.push(' ');
            name.push_str(&serial.to_string());
            if !is_folder {
                name.push('.');
                name.push_str(rng.pick(&EXTS));
            }
            let mut spec = if is_folder {
                dir(&name, parent)
            } else {
                file(&name, parent).size(rng.below(1 << 32))
            };
            if !is_folder && rng.below(10) == 0 {
                spec = spec.no_size();
            }
            if rng.below(8) != 0 {
                spec = spec.time((rng.next() >> 2) as i64);
            }
            if rng.below(20) == 0 {
                spec = spec.hidden();
            }
            let entry = specs.len() as u32;
            specs.push(spec);
            if is_folder {
                stack.push((entry, depth + 1, 1 + rng.below(200)));
            }
        }
        specs
    }

    /// The folder whose subtree size is nearest `share` of all entries.
    pub fn folder_near(index: &Index, share: f64) -> u32 {
        let n = index.len() as usize;
        let mut size = vec![1usize; n];
        for i in (1..n).rev() {
            let p = index.parent(i as u32).unwrap() as usize;
            size[p] += size[i];
        }
        let want = (n as f64 * share) as usize;
        (1..n)
            .filter(|&i| index.is_folder(i as u32))
            .min_by_key(|&i| size[i].abs_diff(want))
            .unwrap_or(0) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use crate::search::kind::{KindClass, KindSet};

    fn q(text: &str) -> Query {
        Query {
            text: text.to_owned(),
            ..Query::default()
        }
    }

    fn run(index: &Index, query: &Query) -> Vec<u32> {
        let never = AtomicBool::new(false);
        let mut hits = index.search(query, None, &never).unwrap();
        hits.sort_all();
        hits.entries_in_order().to_vec()
    }

    fn names(index: &Index, entries: &[u32]) -> Vec<String> {
        entries.iter().map(|&e| index.name(e).to_owned()).collect()
    }

    /// The reference: every entry of the range, the name folded and tested
    /// with `contains`, filters as written, the key computed naively.
    fn naive(index: &Index, query: &Query) -> Vec<u32> {
        let terms = terms(&query.text);
        if terms.is_empty()
            && query.scope.is_none()
            && query.kinds.is_empty()
            && query.size.is_none()
            && query.modified.is_none()
        {
            return Vec::new();
        }
        let len = index.len();
        let range = match query.scope {
            None => 1..len,
            Some(s) if index.is_folder(s) => {
                let r = index.subtree(s);
                r.start + 1..r.end
            }
            Some(_) => 0..0,
        };
        let mut keys = Vec::new();
        for e in range {
            let entry = index.entry(e).unwrap();
            let flags = index.flags(e).unwrap();
            if flags.is_hidden() && !query.include_hidden {
                continue;
            }
            if flags.is_system_item() && !query.include_system {
                continue;
            }
            if !query.kinds.admits(flags.kind_class()) {
                continue;
            }
            if let Some(r) = &query.size
                && !entry.size.is_some_and(|s| r.contains(&s))
            {
                continue;
            }
            if let Some(r) = &query.modified
                && !entry.modified.is_some_and(|t| r.contains(&t.unix_nanos()))
            {
                continue;
            }
            let name = entry.name;
            let folded = fold(name);
            if !terms.iter().all(|t| folded.contains(t.as_str())) {
                continue;
            }
            let class = match terms.first() {
                None => 3,
                Some(t) if folded == *t => 0,
                Some(t) if folded.starts_with(t.as_str()) => 1,
                Some(t) => {
                    let (fb, nb) = (folded.as_bytes(), name.as_bytes());
                    let word = (1..fb.len()).any(|p| {
                        fb[p..].starts_with(t.as_bytes())
                            && ((fb[p - 1] < 0x80 && !fb[p - 1].is_ascii_alphanumeric())
                                || (nb[p - 1].is_ascii_lowercase() && nb[p].is_ascii_uppercase()))
                    });
                    if word { 2 } else { 3 }
                }
            };
            let is_file = u64::from(!flags.is_folder());
            let key = (class << 60)
                | (is_file << 59)
                | ((name.len() as u64).min(0x7FF) << 48)
                | u64::from(e);
            keys.push(key);
        }
        keys.sort_unstable();
        keys.iter().map(|&k| k as u32).collect()
    }

    fn sample() -> Index {
        // 0 vol
        // 1   Photos/            2 photo.png  3 Holiday photo.jpg  4 MyPhoto.png
        // 5   aphotob.txt
        // 6   photo              (folder, exact)
        // 7   photo              (file, exact)
        // 8   photography.doc
        // 9   x_photo
        index(&[
            root("vol"),
            dir("Photos", 0),
            file("photo.png", 1),
            file("Holiday photo.jpg", 1),
            file("MyPhoto.png", 1),
            file("aphotob.txt", 0),
            dir("photo", 0),
            file("photo", 0),
            file("photography.doc", 0),
            file("x_photo", 0),
        ])
    }

    #[test]
    fn hits_are_send_sync_clone() {
        fn check<T: Send + Sync + Clone>() {}
        check::<Hits>();
        check::<Query>();
    }

    #[test]
    fn rank_classes() {
        let index = sample();
        let order = run(&index, &q("photo"));
        // class 0: folder "photo" (6), then file "photo" (7)
        // class 1: folder Photos (1); files photo.png (2, len 9), photography.doc (8, len 15)
        // class 2: x_photo (9, len 7, '_' boundary), MyPhoto.png (4, len 11, camelCase),
        //          Holiday photo.jpg (3, len 17, space)
        // class 3: aphotob.txt (5)
        assert_eq!(order, vec![6, 7, 1, 2, 8, 9, 4, 3, 5]);
        assert_eq!(order, naive(&index, &q("photo")));
    }

    #[test]
    fn rank_uses_first_term_and_best_occurrence() {
        let index = index(&[
            root("vol"),
            file("abab", 0),
            file("xab ab", 0),
            file("xabab", 0),
            file("a.a.a", 0),
        ]);
        // "ab" in "xab ab": first occurrence class 3, second at a word start.
        assert_eq!(run(&index, &q("ab")), vec![1, 2, 3]);
        // Overlapping occurrences: "a.a" at 2 in "a.a.a" is a prefix anyway;
        // "xa.a.a" style: the occurrence at 1 is inside, at 3 after a dot.
        let index2 = index_with(&["xa.a.a", "xa.a"]);
        assert_eq!(run(&index2, &q("a.a")), vec![1, 2]);
        assert_eq!(run(&index2, &q("a.a")), naive(&index2, &q("a.a")));
        // The first term as typed ranks, not the longest: "b" is a prefix of
        // "b photo" while "photo" is only a word start.
        let index3 = index_with(&["photo xb", "b photo"]);
        assert_eq!(run(&index3, &q("b photo")), vec![2, 1]);
        assert_eq!(run(&index3, &q("photo b")), vec![1, 2]);
    }

    fn index_with(files: &[&str]) -> Index {
        let mut specs = vec![root("vol")];
        specs.extend(files.iter().map(|n| file(n, 0)));
        index(&specs)
    }

    #[test]
    fn shorter_names_then_entry_order() {
        let index = index_with(&["zz ab two", "zz ab", "yy ab", "zz ab one"]);
        // All class 2, files: length, then entry.
        assert_eq!(run(&index, &q("ab")), vec![2, 3, 1, 4]);
    }

    #[test]
    fn camel_case_needs_original_case() {
        let index = index_with(&["myphoto", "myPhoto", "MYPHOTO"]);
        // Only "myPhoto" has a lowercase-to-uppercase step before "p".
        assert_eq!(run(&index, &q("photo")), vec![2, 1, 3]);
    }

    #[test]
    fn non_ascii_before_is_not_a_boundary() {
        let index = index_with(&["\u{e9}photo", "-photo", "photo"]);
        assert_eq!(run(&index, &q("PHOTO")), vec![3, 2, 1]);
    }

    #[test]
    fn multi_term() {
        let index = index_with(&[
            "summer photo 2024.jpg",
            "photo summer.png",
            "summer.txt",
            "photo.txt",
            "photosummer",
        ]);
        // Ranked by the first term as typed.
        let got = run(&index, &q("photo summer"));
        assert_eq!(
            names(&index, &got),
            ["photosummer", "photo summer.png", "summer photo 2024.jpg"]
        );
        let got = run(&index, &q("summer photo"));
        assert_eq!(
            names(&index, &got),
            ["summer photo 2024.jpg", "photo summer.png", "photosummer"]
        );
        // A term that occurs only in another entry does not match.
        assert!(run(&index, &q("txt summer photo")).is_empty());
        assert_eq!(
            run(&index, &q("photo summer")),
            naive(&index, &q("photo summer"))
        );
        // Duplicate and contained terms.
        assert_eq!(run(&index, &q("photo photo")), run(&index, &q("photo")));
        assert_eq!(run(&index, &q("ph photo")), naive(&index, &q("ph photo")));
    }

    fn filter_index() -> Index {
        index(&[
            root("vol"),
            dir("docs", 0),                                        // 1
            file("a.txt", 1).size(100).time(1_000),                // 2
            file("b.txt", 1).size(5_000).time(2_000),              // 3
            file("c.png", 1).no_size(),                            // 4 unknown size and date
            file(".hidden.txt", 1).size(100).time(1_500).hidden(), // 5
            dir("pics", 0).time(1_000),                            // 6
            file("d.png", 6).size(300).time(3_000),                // 7
        ])
    }

    #[test]
    fn filters_alone_and_combined() {
        let index = filter_index();
        let docs = KindSet::EMPTY.with(KindClass::Document);
        let images = KindSet::EMPTY.with(KindClass::Image);
        let folders = KindSet::EMPTY.with(KindClass::Folder);
        let cases: Vec<(Query, Vec<u32>)> = vec![
            (
                Query {
                    kinds: docs,
                    ..q("")
                },
                vec![2, 3],
            ),
            (
                Query {
                    kinds: images,
                    ..q("")
                },
                vec![4, 7],
            ),
            (
                Query {
                    kinds: folders,
                    ..q("")
                },
                vec![1, 6],
            ),
            (
                Query {
                    kinds: docs.with(KindClass::Image),
                    ..q("png")
                },
                vec![4, 7],
            ),
            // Unknown size fails; folders have no size.
            (
                Query {
                    size: Some(0..=u64::MAX - 1),
                    ..q("")
                },
                vec![2, 7, 3],
            ),
            (
                Query {
                    size: Some(100..=300),
                    ..q("")
                },
                vec![2, 7],
            ),
            // Unknown date fails.
            (
                Query {
                    modified: Some(1_000..=2_000),
                    ..q("")
                },
                vec![6, 2, 3],
            ),
            (
                Query {
                    modified: Some(i64::MIN..=i64::MAX),
                    ..q("")
                },
                vec![6, 2, 7, 3],
            ),
            (
                Query {
                    size: Some(100..=300),
                    modified: Some(2_500..=4_000),
                    ..q("")
                },
                vec![7],
            ),
            (
                Query {
                    include_hidden: true,
                    size: Some(100..=100),
                    ..q("")
                },
                vec![2, 5],
            ),
            (
                Query {
                    include_hidden: true,
                    ..q("hidden")
                },
                vec![5],
            ),
            (q("hidden"), vec![]),
            (
                Query {
                    size: Some(RangeInclusive::new(10, 1)),
                    ..q("")
                },
                vec![],
            ),
        ];
        for (query, want) in cases {
            let mut got = run(&index, &query);
            let mut want_sorted = want.clone();
            got.sort_unstable();
            want_sorted.sort_unstable();
            assert_eq!(got, want_sorted, "{query:?}");
            assert_eq!(run(&index, &query), naive(&index, &query), "{query:?}");
        }
    }

    #[test]
    fn include_hidden_alone_is_not_a_filter() {
        let index = filter_index();
        let never = AtomicBool::new(false);
        let hits = index
            .search(
                &Query {
                    include_hidden: true,
                    ..q("")
                },
                None,
                &never,
            )
            .unwrap();
        assert!(hits.is_empty());
        assert!(!hits.planned);
    }

    /// Every name holds a `t`. 1 plain, 2 a hidden dot file, 3 a
    /// `.DS_Store` the volume does not flag hidden, 4 a root
    /// `.Spotlight-V100` and 5 a file inside it (inherits bit 6), 6 a hidden
    /// AppleDouble file (hidden and housekeeping).
    fn system_index() -> Index {
        index(&[
            root("Vol"),
            file("notes.txt", 0),
            file(".gitignore", 0).hidden(),
            file(".DS_Store", 0),
            dir(".Spotlight-V100", 0),
            file("store.db", 4),
            file("._notes.txt", 0).hidden(),
        ])
    }

    fn with_switches(text: &str, hidden: bool, system: bool) -> Query {
        Query {
            include_hidden: hidden,
            include_system: system,
            ..q(text)
        }
    }

    #[test]
    fn system_items_follow_their_own_switch() {
        let index = system_index();
        let system: Vec<bool> = (0..7)
            .map(|e| index.flags(e).unwrap().is_system_item())
            .collect();
        assert_eq!(system, [false, false, false, true, true, true, true]);
        let cases: [(bool, bool, &[u32]); 4] = [
            (false, false, &[1]),
            // `.gitignore` shows, `.DS_Store` does not.
            (true, false, &[1, 2]),
            // Housekeeping shows; the hidden AppleDouble file stays hidden.
            (false, true, &[1, 3, 4, 5]),
            (true, true, &[1, 2, 3, 4, 5, 6]),
        ];
        for (hidden, system, want) in cases {
            let query = with_switches("t", hidden, system);
            let mut got = run(&index, &query);
            got.sort_unstable();
            assert_eq!(got, want, "{query:?}");
            assert_eq!(run(&index, &query), naive(&index, &query), "{query:?}");
            // The filter-only path (no terms) applies the switches too.
            let scoped = Query {
                scope: Some(0),
                ..with_switches("", hidden, system)
            };
            let mut got = run(&index, &scoped);
            got.sort_unstable();
            assert_eq!(got, want, "{scoped:?}");
        }
    }

    #[test]
    fn include_system_alone_is_not_a_filter() {
        let index = system_index();
        let never = AtomicBool::new(false);
        let hits = index
            .search(&with_switches("", true, true), None, &never)
            .unwrap();
        assert!(hits.is_empty());
        assert!(!hits.planned);
    }

    #[test]
    fn toggling_include_system_does_not_narrow() {
        let index = system_index();
        let never = AtomicBool::new(false);
        let len = index.len() as usize;
        let before = q("t");
        let prev = index.search(&before, None, &never).unwrap();
        assert_eq!(prev.len(), 1);
        // Same switches: a longer term narrows the one hit.
        assert!(can_narrow(&q("to"), &terms("to"), (&before, &prev), len));
        // Switched on: the previous hits miss what it admits, so no narrowing.
        let after = with_switches("to", false, true);
        assert!(!can_narrow(&after, &terms("to"), (&before, &prev), len));
        let mut got = index
            .search(&after, Some((&before, &prev)), &never)
            .unwrap();
        got.sort_all();
        let mut entries = got.entries_in_order().to_vec();
        entries.sort_unstable();
        // `.DS_Store` and `store.db`.
        assert_eq!(entries, [3, 5]);
        assert_eq!(got.entries_in_order().to_vec(), naive(&index, &after));
        // And back off again.
        let on = with_switches("t", false, true);
        let on_hits = index.search(&on, None, &never).unwrap();
        let off = q("to");
        assert!(!can_narrow(&off, &terms("to"), (&on, &on_hits), len));
        let got = index.search(&off, Some((&on, &on_hits)), &never).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn no_terms_no_filters_is_empty() {
        let index = filter_index();
        let never = AtomicBool::new(false);
        for text in ["", "   ", "\u{3000}\t"] {
            let hits = index.search(&q(text), None, &never).unwrap();
            assert!(hits.is_empty() && hits.sorted_len() == 0);
            assert_eq!(hits.entry(0), None);
            assert!(hits.entries_in_order().is_empty());
        }
    }

    #[test]
    fn scope() {
        let index = filter_index();
        let in_docs = |text: &str| Query {
            scope: Some(1),
            ..q(text)
        };
        let mut got = run(&index, &in_docs("txt"));
        got.sort_unstable();
        assert_eq!(got, vec![2, 3]);
        // Filters-only by scope: the subtree, without the folder.
        let mut got = run(&index, &in_docs(""));
        got.sort_unstable();
        assert_eq!(got, vec![2, 3, 4]);
        // The scope folder's own name never matches.
        assert!(run(&index, &in_docs("docs")).is_empty());
        // The root as scope: everything.
        assert_eq!(
            run(
                &index,
                &Query {
                    scope: Some(0),
                    ..q("png")
                }
            ),
            run(&index, &q("png"))
        );
        // A file, an empty folder, an out-of-range number: nothing.
        assert!(
            run(
                &index,
                &Query {
                    scope: Some(2),
                    ..q("a")
                }
            )
            .is_empty()
        );
        assert!(
            run(
                &index,
                &Query {
                    scope: Some(99),
                    ..q("")
                }
            )
            .is_empty()
        );
        assert!(
            run(
                &index,
                &Query {
                    scope: Some(u32::MAX),
                    ..q("a")
                }
            )
            .is_empty()
        );
        for s in 0..9 {
            for text in ["", "a", "png", "t"] {
                let query = Query {
                    scope: Some(s),
                    ..q(text)
                };
                assert_eq!(run(&index, &query), naive(&index, &query));
            }
        }
    }

    #[test]
    fn narrowing_equals_fresh_search() {
        let index = index(&generated(20_000, 7));
        let never = AtomicBool::new(false);
        let steps = ["p", "ph", "pho", "phot", "photo", "photo 2", "photo 2 s"];
        let mut previous: Option<(Query, Hits)> = None;
        let mut narrowed = 0;
        for text in steps {
            let query = q(text);
            let fresh = run(&index, &query);
            let prev = previous.as_ref().map(|(pq, ph)| (pq, ph));
            if let Some(p) = prev
                && can_narrow(&query, &terms(text), p, index.len() as usize - 1)
            {
                narrowed += 1;
            }
            let mut hits = index.search(&query, prev, &never).unwrap();
            let mut sorted = hits.clone();
            sorted.sort_all();
            assert_eq!(sorted.entries_in_order().to_vec(), fresh, "{text}");
            assert_eq!(fresh, naive(&index, &query), "{text}");
            let mut by = fresh.clone();
            by.sort_unstable();
            assert_eq!(hits.by_entry(), &by[..]);
            hits.sort_all();
            previous = Some((query, hits));
        }
        assert!(narrowed >= 3, "narrowing was used {narrowed} times");

        // A changed filter: no narrowing, results still right.
        let (pq, ph) = previous.as_ref().unwrap();
        let changed = Query {
            include_hidden: true,
            ..q("photo 2 s")
        };
        assert!(!can_narrow(
            &changed,
            &terms(&changed.text),
            (pq, ph),
            index.len() as usize
        ));
        let got = index.search(&changed, Some((pq, ph)), &never).unwrap();
        let mut got = got;
        got.sort_all();
        assert_eq!(got.entries_in_order().to_vec(), naive(&index, &changed));
        let kinds = Query {
            kinds: KindSet::EMPTY.with(KindClass::Image),
            ..q("photo 2 s")
        };
        let mut got = index.search(&kinds, Some((pq, ph)), &never).unwrap();
        got.sort_all();
        assert_eq!(got.entries_in_order().to_vec(), naive(&index, &kinds));
        // A term that does not extend the previous one: no narrowing.
        assert!(!can_narrow(
            &q("budget"),
            &terms("budget"),
            (pq, ph),
            index.len() as usize
        ));
    }

    #[test]
    fn empty_plan_is_never_a_narrowing_base() {
        let index = index_with(&["photo", "other"]);
        let never = AtomicBool::new(false);
        let empty_query = q("");
        let empty = index.search(&empty_query, None, &never).unwrap();
        assert!(!empty.planned);
        let next = q("photo");
        assert!(!can_narrow(
            &next,
            &terms("photo"),
            (&empty_query, &empty),
            1_000_000
        ));
        let got = index
            .search(&next, Some((&empty_query, &empty)), &never)
            .unwrap();
        assert_eq!(got.entries_in_order().to_vec(), vec![1]);
        // A planned empty result is a valid base: narrowing gives nothing,
        // which is right.
        let none = index.search(&q("zzz"), None, &never).unwrap();
        assert!(none.planned);
    }

    #[test]
    fn cancelled_before_the_call() {
        let index = sample();
        let cancel = AtomicBool::new(true);
        assert!(index.search(&q("photo"), None, &cancel).is_none());
        assert!(index.search(&q(""), None, &cancel).is_none());
        let filters = Query {
            size: Some(0..=10),
            ..q("")
        };
        assert!(index.search(&filters, None, &cancel).is_none());
    }

    #[test]
    fn cancelled_mid_search() {
        let index = index(&generated(300_000, 11));
        let fresh = run(&index, &q("a"));
        for _ in 0..3 {
            let cancel = AtomicBool::new(false);
            let result = std::thread::scope(|s| {
                let handle = s.spawn(|| index.search(&q("a"), None, &cancel));
                std::thread::sleep(Duration::from_micros(200));
                cancel.store(true, Ordering::Relaxed);
                handle.join().unwrap()
            });
            if let Some(mut hits) = result {
                hits.sort_all();
                assert_eq!(hits.entries_in_order().to_vec(), fresh);
            }
        }
    }

    #[test]
    fn head_is_sorted_first() {
        let mut specs = vec![root("vol")];
        let mut rng = Rng(3);
        for i in 0..10_000 {
            let name = format!("{} file {}", rng.pick(&["x", "file", "afile", "a_file"]), i);
            specs.push(if i % 7 == 0 {
                dir(&name, 0)
            } else {
                file(&name, 0)
            });
        }
        let index = index(&specs);
        let never = AtomicBool::new(false);
        let hits = index.search(&q("file"), None, &never).unwrap();
        assert_eq!(hits.len(), 10_000);
        assert_eq!(hits.sorted_len(), HEAD);
        let full = naive(&index, &q("file"));
        let head: Vec<u32> = (0..HEAD).map(|p| hits.entry(p).unwrap()).collect();
        assert_eq!(head, full[..HEAD]);
        // The tail holds exactly the rest.
        let mut tail: Vec<u32> = (HEAD..hits.len()).map(|p| hits.entry(p).unwrap()).collect();
        let mut want_tail = full[HEAD..].to_vec();
        tail.sort_unstable();
        want_tail.sort_unstable();
        assert_eq!(tail, want_tail);
        assert_eq!(hits.entries_in_order().len(), 10_000);
        assert_eq!(&hits.entries_in_order()[..HEAD], &full[..HEAD]);
        let mut all = hits.clone();
        all.sort_all();
        assert_eq!(all.sorted_len(), 10_000);
        assert_eq!(all.entries_in_order().to_vec(), full);
        assert_eq!(hits.entry(10_000), None);
    }

    /// The arrangement before per-piece heads: everything concatenated, the
    /// head selected serially.
    fn serial_reference(chunks: &[Vec<u64>]) -> (Vec<u64>, usize, Vec<u32>) {
        let (keys, sorted) = serial_keys(chunks);
        let by_entry = chunks.iter().flatten().map(|&k| entry_of(k)).collect();
        (keys, sorted, by_entry)
    }

    /// `Hits::from_chunks` against the serial reference: same count, same
    /// head in the same order, same set behind it, same `by_entry`, the
    /// tail never below the head, and the same order once all is sorted.
    fn assert_same_as_serial(chunks: Vec<Vec<u64>>, label: &str) {
        let (want_keys, want_sorted, want_by) = serial_reference(&chunks);
        let hits = Hits::from_chunks(chunks);
        assert_eq!(hits.len(), want_keys.len(), "{label}: len");
        assert_eq!(hits.sorted_len(), want_sorted, "{label}: sorted_len");
        assert_eq!(hits.by_entry(), &want_by[..], "{label}: by_entry");
        let s = hits.sorted_len();
        assert_eq!(hits.keys[..s], want_keys[..s], "{label}: head");
        let mut tail = hits.keys[s..].to_vec();
        let mut want_tail = want_keys[s..].to_vec();
        tail.sort_unstable();
        want_tail.sort_unstable();
        assert_eq!(tail, want_tail, "{label}: tail set");
        if let (Some(last), Some(first)) = (hits.keys[..s].last(), tail.first()) {
            assert!(last <= first, "{label}: tail below head");
        }
        let mut all = hits.clone();
        all.sort_all();
        let mut want_all = want_keys;
        want_all.sort_unstable();
        assert_eq!(all.keys, want_all, "{label}: sorted");
    }

    /// A key as `admit` makes them: class, file bit, length, entry.
    fn key(class: u64, file: u64, len: u64, entry: u64) -> u64 {
        (class << CLASS_SHIFT) | (file << FILE_SHIFT) | (len.min(MAX_KEY_LEN) << LEN_SHIFT) | entry
    }

    /// Pieces of `lens` with realistic keys: entries ascending across the
    /// pieces, few distinct classes and lengths (many ties on everything
    /// but the entry).
    fn pieces(rng: &mut Rng, lens: &[usize]) -> Vec<Vec<u64>> {
        let mut entry = 1u64;
        lens.iter()
            .map(|&n| {
                (0..n)
                    .map(|_| {
                        entry += 1 + rng.below(3);
                        key(rng.below(4), rng.below(2), 5 + rng.below(4), entry)
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn piece_heads_equal_the_serial_select() {
        let mut rng = Rng(0x5EED);
        let fixed: Vec<(&str, Vec<usize>)> = vec![
            ("no pieces", vec![]),
            ("empty pieces", vec![0, 0, 0]),
            ("one small piece", vec![10]),
            ("fewer than the head", vec![1000, 0, 2000, 1095]),
            ("exactly the head, one piece", vec![HEAD]),
            (
                "exactly the head, many pieces",
                vec![1000, 1000, 1000, 1096],
            ),
            ("one past the head", vec![HEAD, 1]),
            ("one past the head, tiny pieces", vec![1; HEAD + 1]),
            ("one piece past the head", vec![HEAD + 1]),
            ("one big piece among small", vec![3, 50_000, 0, 7]),
            ("many pieces of 2048", vec![2048; 40]),
            ("pieces of exactly the head", vec![HEAD; 5]),
        ];
        for (label, lens) in fixed {
            assert_same_as_serial(pieces(&mut rng, &lens), label);
        }
        for round in 0..40 {
            let count = rng.below(30) as usize;
            let lens: Vec<usize> = (0..count)
                .map(|_| match rng.below(4) {
                    0 => 0,
                    1 => rng.below(64) as usize,
                    2 => rng.below(HEAD as u64 * 2) as usize,
                    _ => rng.below(20_000) as usize,
                })
                .collect();
            assert_same_as_serial(pieces(&mut rng, &lens), &format!("random {round}"));
        }
    }

    #[test]
    fn piece_heads_with_adversarial_keys() {
        let mut rng = Rng(77);
        // Equal keys everywhere (not possible from a scan, which puts the
        // entry in the key, but the merge must still pick the same multiset).
        assert_same_as_serial(vec![vec![42; 3000], vec![42; 3000]], "all equal");
        assert_same_as_serial(
            (0..9)
                .map(|_| (0..1500).map(|_| rng.below(3)).collect())
                .collect(),
            "three values",
        );
        // The whole head in the last piece, then in the first.
        let mut late = vec![vec![u64::MAX - 1; 5000]; 4];
        late.push((0..5000).collect());
        assert_same_as_serial(late, "head in the last piece");
        let mut early = vec![(0..5000).collect::<Vec<u64>>()];
        early.extend(vec![vec![u64::MAX; 5000]; 4]);
        assert_same_as_serial(early, "head in the first piece");
        // Descending pieces, extreme values.
        assert_same_as_serial(
            vec![
                (0..6000u64).rev().collect(),
                vec![u64::MAX, 0, u64::MAX, 0],
                (0..6000u64).map(|i| u64::MAX - i).collect(),
            ],
            "descending and extremes",
        );
        // The head split evenly over many pieces.
        assert_same_as_serial(
            (0..64u64)
                .map(|p| (0..200u64).map(|i| i * 64 + p).collect())
                .collect(),
            "interleaved",
        );
    }

    #[test]
    fn split_lengths_never_overruns() {
        let mut buf = [0u8; 5];
        let parts = split_lengths(&mut buf, &[2, 0, 9, 4]);
        let lens: Vec<usize> = parts.iter().map(|p| p.len()).collect();
        assert_eq!(lens, vec![2, 0, 3, 0]);
        assert!(split_lengths::<u8>(&mut [], &[]).is_empty());
    }

    #[test]
    fn merged_keys_refuses_fronts_that_cannot_fill_the_head() {
        // Fronts not put in order first still give the right multiset or
        // `None`; never a head with holes.
        let chunks = vec![vec![1u64; 10]];
        assert!(merged_keys(&chunks, 10).is_none());
        // Lengths that disagree with the pieces.
        let chunks = vec![vec![1u64; HEAD + 5]];
        assert!(merged_keys(&chunks, HEAD + 9).is_none());
    }

    /// Searches whose hits span several pieces of every kind (byte chunks,
    /// filter chunks, narrowing chunks), with exactly as many hits as the
    /// head, one fewer, one more, none and many; ranks tie on class and
    /// length, so the entry decides.
    #[test]
    fn search_across_pieces_matches_the_reference() {
        const COUNTS: [usize; 7] = [0, 1, HEAD - 1, HEAD, HEAD + 1, 9_000, 20_000];
        const N: usize = 150_000;
        let mut rng = Rng(4242);
        let mut marks: Vec<Vec<usize>> = vec![Vec::new(); N + 1];
        for (m, &count) in COUNTS.iter().enumerate() {
            let mut chosen = std::collections::HashSet::new();
            while chosen.len() < count {
                chosen.insert(1 + rng.below(N as u64) as usize);
            }
            for e in chosen {
                marks[e].push(m);
            }
        }
        let mut specs = vec![root("vol")];
        for (e, marks) in marks.iter().enumerate().skip(1) {
            let mut name = format!("name number {e:07}");
            for m in marks {
                name.push_str(&format!(" m{m}x"));
            }
            let size = if marks.contains(&3) { 7 } else { 8 };
            specs.push(if e % 7 == 0 {
                dir(&name, 0)
            } else {
                file(&name, 0).size(size)
            });
        }
        let index = index(&specs);
        assert!(
            byte_chunks(index.offsets(), 1..N + 1).len() >= 3,
            "the scan must span several pieces"
        );
        let never = AtomicBool::new(false);
        let check = |query: &Query, previous: Option<(&Query, &Hits)>, want_len: usize| {
            let label = format!("{query:?}");
            let want = naive(&index, query);
            assert_eq!(want.len(), want_len, "{label}");
            let hits = index.search(query, previous, &never).unwrap();
            assert_eq!(hits.len(), want.len(), "{label}");
            assert_eq!(hits.sorted_len(), want.len().min(HEAD), "{label}");
            let head: Vec<u32> = (0..hits.sorted_len())
                .map(|p| hits.entry(p).unwrap())
                .collect();
            assert_eq!(head, want[..hits.sorted_len()], "{label}");
            let mut by = want.clone();
            by.sort_unstable();
            assert_eq!(hits.by_entry(), &by[..], "{label}");
            let mut all = hits.clone();
            all.sort_all();
            assert_eq!(all.entries_in_order().to_vec(), want, "{label}");
            hits
        };
        for (m, &count) in COUNTS.iter().enumerate() {
            let term = format!("m{m}x");
            check(&q(&term), None, count);
            // Narrowing from a shorter term: candidates in chunks of 2048.
            let shorter = q(&format!("m{m}"));
            let prev = index.search(&shorter, None, &never).unwrap();
            check(&q(&term), Some((&shorter, &prev)), count);
        }
        // Filters only: chunks of entries. Folders have no size.
        let files_with_mark = (1..=N)
            .filter(|&e| e % 7 != 0 && marks[e].contains(&3))
            .count();
        check(
            &Query {
                size: Some(7..=7),
                ..q("")
            },
            None,
            files_with_mark,
        );
        // Most of the volume: the one-letter case.
        let all_n = naive(&index, &q("n")).len();
        assert!(all_n > N / 2);
        check(&q("n"), None, all_n);
    }

    #[test]
    fn randomized_against_reference() {
        const PIECES: [&str; 16] = [
            "aa",
            "banana",
            "Photo",
            "photo",
            "MyPhoto",
            "x",
            "2",
            "10",
            "caf\u{e9}",
            "CAF\u{c9}",
            "\u{3a3}\u{3b9}",
            "\u{3c3}\u{3b9}",
            "_",
            "-",
            ".",
            "aaa",
        ];
        const QUERIES: [&str; 22] = [
            "a",
            "aa",
            "aaa",
            "an",
            "ana",
            "photo",
            "PHOTO",
            "my",
            "o",
            "2",
            "10",
            "0",
            "caf\u{e9}",
            "\u{c9}",
            "\u{3c3}",
            "\u{3a3}\u{3b9}",
            "a b",
            "photo 2",
            "_p",
            ".a",
            "banana aa",
            "-",
        ];
        let mut rng = Rng(99);
        let mut specs = vec![root("Rand")];
        let mut folders = vec![0u32];
        for _ in 0..4_000 {
            let mut name = String::new();
            for _ in 0..=rng.below(4) {
                name.push_str(rng.pick(&PIECES));
                if rng.below(3) == 0 {
                    name.push(' ');
                }
            }
            if name.trim().is_empty() {
                name.push('n');
            }
            // Pre-order: the parent is one of the folders still open, the
            // most recent ones (the stack).
            let keep = 1 + rng.below(folders.len() as u64) as usize;
            folders.truncate(keep);
            let parent = *folders.last().unwrap();
            let entry = specs.len() as u32;
            let folder = rng.below(5) == 0;
            // Now and then a housekeeping name (some count only at the
            // root, `Trash` only when hidden).
            if rng.below(15) == 0 {
                name = if folder {
                    rng.pick(&["__MACOSX", ".Spotlight-V100", "Trash", "aa"])
                } else {
                    rng.pick(&[".DS_Store", "._aa", "Thumbs.db", "Icon\r", "._"])
                }
                .to_string();
            }
            let mut spec = if folder {
                dir(&name, parent)
            } else {
                file(&name, parent).size(rng.below(1000))
            };
            if !folder && rng.below(10) == 0 {
                spec = spec.no_size();
            }
            if rng.below(4) != 0 {
                spec = spec.time(rng.below(1000) as i64);
            }
            if rng.below(10) == 0 {
                spec = spec.hidden();
            }
            specs.push(spec);
            if folder {
                folders.push(entry);
            }
        }
        let index = index(&specs);
        let system = (0..index.len())
            .filter(|&e| index.flags(e).is_some_and(|f| f.is_system_item()))
            .count();
        assert!(system > 50, "{system} housekeeping entries");
        let never = AtomicBool::new(false);
        for round in 0..300 {
            let mut query = q(rng.pick(&QUERIES));
            if rng.below(3) == 0 {
                query.text.push(' ');
                query.text.push_str(rng.pick(&QUERIES));
            }
            match rng.below(8) {
                0 => query.size = Some(100..=600),
                1 => query.modified = Some(200..=700),
                2 => query.kinds = KindSet::EMPTY.with(KindClass::Folder),
                3 => query.scope = Some(rng.below(index.len() as u64 + 2) as u32),
                4 => query.include_hidden = true,
                5 => query.include_system = true,
                6 => {
                    query.include_hidden = true;
                    query.include_system = true;
                }
                _ => {}
            }
            let want = naive(&index, &query);
            assert_eq!(run(&index, &query), want, "round {round}: {query:?}");
            // Narrowing from a shorter prefix of the first term.
            let mut shorter = query.clone();
            let t = terms(&query.text);
            if let Some(first) = t.first() {
                let cut = first.char_indices().nth(1).map_or(first.len(), |(i, _)| i);
                shorter.text = first[..cut].to_owned();
                let prev = index.search(&shorter, None, &never).unwrap();
                let mut got = index
                    .search(&query, Some((&shorter, &prev)), &never)
                    .unwrap();
                got.sort_all();
                assert_eq!(
                    got.entries_in_order().to_vec(),
                    want,
                    "narrowed {round}: {query:?}"
                );
                // The previous search had the other `include_system`.
                let mut toggled = shorter.clone();
                toggled.include_system = !toggled.include_system;
                let prev = index.search(&toggled, None, &never).unwrap();
                let mut got = index
                    .search(&query, Some((&toggled, &prev)), &never)
                    .unwrap();
                got.sort_all();
                assert_eq!(
                    got.entries_in_order().to_vec(),
                    want,
                    "toggled {round}: {query:?}"
                );
            }
        }
    }

    #[test]
    fn primary_is_longest_first() {
        let t = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(primary_term(&t(&["ab", "abc", "xyz"])), Some(1));
        assert_eq!(primary_term(&t(&["ab", "cd"])), Some(0));
        assert_eq!(primary_term(&[]), None);
    }

    #[test]
    fn gallop_finds_the_entry() {
        let offsets = [0u32, 3, 4, 10, 11, 12, 20, 30, 31];
        for p in 0..31usize {
            let want = offsets.iter().rposition(|&o| o as usize <= p).unwrap();
            for lo in 0..=want {
                assert_eq!(
                    entry_at(&offsets, lo, offsets.len() - 1, p),
                    want,
                    "p {p} lo {lo}"
                );
            }
        }
    }

    #[test]
    fn byte_chunks_cover_the_range() {
        let offsets: Vec<u32> = (0..=5000u32).map(|i| i * 700).collect();
        let chunks = byte_chunks(&offsets, 3..4990);
        assert_eq!(chunks.first().unwrap().start, 3);
        assert_eq!(chunks.last().unwrap().end, 4990);
        for w in chunks.windows(2) {
            assert_eq!(w[0].end, w[1].start);
            assert!(w[0].start < w[0].end);
        }
        assert!(byte_chunks(&offsets, 5..5).is_empty());
        // Offsets that end early: one chunk per entry that has an offset,
        // then the walk stops.
        assert_eq!(byte_chunks(&offsets[..3], 0..10), vec![0..1, 1..2, 2..3]);
    }

    #[test]
    fn terms_fold_split_dedup() {
        assert_eq!(terms("Photo  SUMMER\tphoto"), ["photo", "summer"]);
        assert_eq!(terms("  "), Vec::<String>::new());
        assert_eq!(terms(""), Vec::<String>::new());
        // Non-ASCII whitespace: ideographic space, no-break space, line separator.
        assert_eq!(terms("a\u{3000}b\u{a0}c\u{2028}d"), ["a", "b", "c", "d"]);
        // Non-ASCII folding.
        assert_eq!(
            terms("CAF\u{c9} \u{3a3}\u{399}"),
            ["caf\u{e9}", "\u{3c3}\u{3b9}"]
        );
        assert_eq!(terms("\u{130}"), ["\u{130}"]);
        // NUL becomes the stored replacement.
        assert_eq!(terms("a\0b"), ["a\u{1a}b"]);
    }

    #[test]
    fn nul_in_query_never_spans_names() {
        let index = index_with(&["ab", "c\0d"]);
        assert!(run(&index, &q("b\0c")).is_empty());
        assert_eq!(run(&index, &q("c\0d")), vec![2]);
    }

    #[test]
    fn match_ranges_merges_and_clears() {
        let mut out = vec![0..1, 5..9];
        let t = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        match_ranges("banana", &t(&["ana"]), &mut out);
        assert_eq!(out, vec![1..6]); // 1..4 and 3..6 overlap
        match_ranges("abab", &t(&["ab"]), &mut out);
        assert_eq!(out, vec![0..4]); // 0..2 and 2..4 touch
        match_ranges("ab xab", &t(&["ab", "x"]), &mut out);
        assert_eq!(out, vec![0..2, 3..6]);
        match_ranges("photo", &t(&["zzz"]), &mut out);
        assert!(out.is_empty());
        match_ranges("photo", &[], &mut out);
        assert!(out.is_empty());
        match_ranges("aaaa", &t(&["aa"]), &mut out);
        assert_eq!(out, vec![0..4]);
    }

    #[test]
    fn match_ranges_non_ascii_on_char_boundaries() {
        let mut out = Vec::new();
        let names = [
            "Caf\u{c9} caf\u{e9}",
            "\u{3a3}\u{3b9}\u{3c3}\u{3b9}",
            "\u{65e5}\u{672c}\u{65e5}",
            "\u{130}stanbul \u{130}",
            "\u{212a}elvin k",
        ];
        let queries = [
            "caf\u{e9}",
            "\u{3c3}\u{3b9}",
            "\u{65e5}",
            "\u{130}",
            "k",
            "\u{e9} c",
            "s",
        ];
        for name in names {
            for query in queries {
                let t = terms(query);
                match_ranges(name, &t, &mut out);
                for r in &out {
                    assert!(
                        name.is_char_boundary(r.start) && name.is_char_boundary(r.end),
                        "{name} {r:?}"
                    );
                    let piece = fold(&name[r.clone()]);
                    assert!(t.iter().any(|t| piece.contains(t.as_str())), "{name} {r:?}");
                }
                for w in out.windows(2) {
                    assert!(w[0].end < w[1].start);
                }
            }
        }
        match_ranges("Caf\u{c9} caf\u{e9}", &terms("CAF\u{c9}"), &mut out);
        assert_eq!(out, vec![0..5, 6..11]);
        match_ranges(
            "\u{3a3}\u{3b9}\u{3c3}\u{3b9}",
            &terms("\u{3c3}\u{3b9}"),
            &mut out,
        );
        assert_eq!(out, vec![0..8]);
        // KELVIN SIGN folds to itself, so "k" does not light it.
        match_ranges("\u{212a}elvin k", &terms("k"), &mut out);
        assert_eq!(out, vec![9..10]);
    }

    /// Median of `runs` timings of `f`, in milliseconds.
    fn median_ms(runs: usize, mut f: impl FnMut()) -> f64 {
        let mut times: Vec<f64> = (0..runs)
            .map(|_| {
                let t = Instant::now();
                f();
                t.elapsed().as_secs_f64() * 1e3
            })
            .collect();
        times.sort_by(f64::total_cmp);
        times[times.len() / 2]
    }

    #[test]
    #[ignore = "timing; run in release with --ignored --nocapture"]
    #[allow(clippy::print_stdout)]
    fn query_throughput() {
        const N: usize = 1_000_000;
        let made = Instant::now();
        let index = index(&generated(N, 42));
        let built = made.elapsed();
        let never = AtomicBool::new(false);
        let cores = std::thread::available_parallelism().map_or(0, |n| n.get());
        let folder = fixtures::folder_near(&index, 0.10);
        let scoped_len = index.subtree(folder).len();
        println!(
            "query_throughput: {} entries, {} cores, {:.1} MB folded, generated in {:.0} ms",
            index.len(),
            cores,
            index.folded_bytes().len() as f64 / 1e6,
            built.as_secs_f64() * 1e3
        );
        let measure = |label: &str, query: &Query| {
            let mut hits = 0;
            let ms = median_ms(20, || {
                hits = index.search(query, None, &never).unwrap().len();
            });
            println!("  {label:<28} {:>8.2} ms median of 20, {hits} hits", ms);
        };
        let rare = RARE.to_lowercase();
        measure(&format!("rare 8 letters ({rare})"), &q(&rare));
        measure("common 3 letters (img)", &q("img"));
        measure("one letter (a)", &q("a"));
        measure("one letter (e)", &q("e"));
        measure("two terms (photo final)", &q("photo final"));
        let scoped = Query {
            scope: Some(folder),
            ..q("img")
        };
        measure(&format!("scoped img ({scoped_len} entries)"), &scoped);
        measure(
            "filters only (size)",
            &Query {
                size: Some(0..=1 << 20),
                ..q("")
            },
        );

        measure("fresh photo (no narrowing)", &q("photo"));
        let from = q("phot");
        let to = q("photo");
        let prev = index.search(&from, None, &never).unwrap();
        let mut narrowed = 0;
        let ms = median_ms(20, || {
            narrowed = index
                .search(&to, Some((&from, &prev)), &never)
                .unwrap()
                .len();
        });
        println!(
            "  {:<28} {:>8.2} ms median of 20, {narrowed} hits from {}",
            "narrow phot -> photo",
            ms,
            prev.len()
        );
        let mut sorted = 0.0;
        let hits = index.search(&q("a"), None, &never).unwrap();
        let ms = median_ms(5, || {
            let mut h = hits.clone();
            let t = Instant::now();
            h.sort_all();
            sorted = t.elapsed().as_secs_f64() * 1e3;
        });
        println!(
            "  {:<28} {:>8.2} ms median of 5 (sort {sorted:.2} ms), {} hits",
            "sort_all after 'a'",
            ms,
            hits.len()
        );
    }
}
