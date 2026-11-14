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

//! Folder totals and bytes per kind for a volume Disk Atlas reads, from its
//! search [`Index`] (design `r10-everyday.md` §3.1).
//!
//! # What the two numbers mean
//!
//! - **A folder's total is what copying it out would give you.** Every name
//!   inside counts: a file with three names in the folder counts three
//!   times, and a folder reached through a directory hard link counts its
//!   target's contents.
//! - **The volume's bytes per kind count each file once** (by node), so the
//!   storage bar never claims more than the disk holds.
//!
//! Sizes are logical bytes: data fork plus resource fork.
//!
//! # How it is built
//!
//! A few linear passes over the index's arrays, plus a sort of the folders
//! by node. Per entry only a 4-byte scratch table (entry to folder rank)
//! lives during the build; what is kept is 24 bytes per folder plus 12 per
//! folder node.
//!
//! 1. **Private stores**: the root's direct children that
//!    [`system_item`] classes [`SystemItem::HfsPrivate`]. Their totals are
//!    computed but not added to the root. Today's HFS+ scan does not report
//!    them (the first report of a linked folder is the link itself, which
//!    holds the children), so on most indexes there are none.
//! 2. **`end`** of every folder, by one reverse pass.
//! 3. **Own totals and `volume`**, by a forward pass over the entries outside
//!    the private stores, then over those inside, so a hard-linked file is
//!    classed by a name the user gave it and not by `iNode<n>`.
//! 4. **Subtree totals**, by a reverse pass over the folders.
//! 5. **Directory links**: for a folder node with several entries, the entry
//!    that has children is the *target*, every other entry a *link*. A
//!    link's total becomes its target's (with the links inside the target
//!    followed, at most [`MAX_LINK_NESTING`] deep, a cycle ending at 0), and
//!    step 4 runs again with those in place.
//! 6. **`by_node`**: every folder node, sorted, with its target entry.
//!
//! The index may come from a saved file. Every array access is checked,
//! every sum saturates, every loop is bounded by the entry count, and
//! nothing here panics; damage shows as [`IndexSizes::is_lower_bound`].

use std::sync::atomic::{AtomicBool, Ordering};

use crate::fs::NodeId;
use crate::mac::conventions::{SystemItem, system_item};
use crate::search::index::{Index, IndexFlags, MAX_DEPTH, MAX_ENTRIES, NO_RSIZE, NO_SIZE};
use crate::search::kind::KindClass;

/// Largest file node for which "seen" is a dense bit set over node numbers
/// (32 MiB of bits at most). Above it, a sorted copy of the file nodes is
/// searched instead.
pub const DENSE_NODE_LIMIT: u64 = 1 << 28;

/// How deep directory links are followed inside link targets. A chain
/// longer than this, or a cycle, ends at 0 and sets the lower-bound flag.
pub const MAX_LINK_NESTING: u32 = 64;

/// `cancel` is checked when a pass's counter is a multiple of 65,536.
const CANCEL_MASK: usize = 0xFFFF;

/// "Not a folder" in the rank table.
const NONE: u32 = u32::MAX;

/// Most private stores kept; more are counted as ordinary folders and set
/// the lower-bound flag (HFS+ has two, `.vol` makes three).
const MAX_STORES: usize = 8;

/// Slots of [`ClassBytes::by_class`].
const CLASS_SLOTS: usize = 16;

// Every class has a slot.
const _: () = assert!(KindClass::ALL.len() <= CLASS_SLOTS);

/// What is inside one folder, everything below it included.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    /// Data forks plus resource forks, in bytes. Saturating.
    pub bytes: u64,
    /// Files and links.
    pub files: u64,
    /// Folders inside, this one not counted.
    pub folders: u64,
}

/// Bytes per kind class, indexed by `KindClass as usize`, plus housekeeping.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClassBytes {
    /// Bytes per [`KindClass`], at [`KindClass::bits`].
    pub by_class: [u64; 16],
    /// Bytes of housekeeping items and of everything inside them.
    pub system: u64,
}

impl ClassBytes {
    /// The bytes of `class`.
    pub fn of(&self, class: KindClass) -> u64 {
        self.by_class
            .get(usize::from(class.bits()))
            .copied()
            .unwrap_or(0)
    }

    /// Every class and `system` together. Saturating.
    pub fn total(&self) -> u64 {
        self.by_class
            .iter()
            .fold(self.system, |sum, &b| sum.saturating_add(b))
    }

    fn add(&mut self, system: bool, class: KindClass, bytes: u64, clamped: &mut bool) {
        let slot = if system {
            Some(&mut self.system)
        } else {
            self.by_class.get_mut(usize::from(class.bits()))
        };
        if let Some(slot) = slot {
            add_u64(slot, bytes, clamped);
        }
    }
}

/// Folder totals and the volume's bytes per kind for one [`Index`].
///
/// Built once per index by [`IndexSizes::build`] on a background thread;
/// `Send + Sync`, every query takes `&self`.
#[derive(Debug, Clone)]
pub struct IndexSizes {
    /// Per folder, in entry order: its entry number.
    entry: Box<[u32]>,
    /// Per folder: its total bytes.
    bytes: Box<[u64]>,
    /// Per folder: files and links inside, saturated at `u32::MAX`.
    files: Box<[u32]>,
    /// Per folder: folders inside, saturated at `u32::MAX`.
    folders: Box<[u32]>,
    /// Per folder: the first entry after its subtree.
    end: Box<[u32]>,
    /// Every folder node, ascending (`by_node` of the design, as two arrays
    /// so it costs 12 bytes per node and not 16).
    node_keys: Box<[u64]>,
    /// The entry of each node in `node_keys`: the target when the node has
    /// several.
    node_entries: Box<[u32]>,
    /// Entry numbers of the private stores.
    stores: Box<[u32]>,
    volume: ClassBytes,
    lower_bound: bool,
    overcommitted: bool,
}

impl IndexSizes {
    /// A few passes over `index`. `None` when `cancel` was set (checked
    /// every 65,536 entries).
    ///
    /// Blocking and CPU-bound: call it on a background thread.
    pub fn build(index: &Index, cancel: &AtomicBool) -> Option<Self> {
        let is_store = |e: u32| {
            index.flags(e).is_some_and(|f| {
                system_item(index.name(e), f.kind(), true, f.is_hidden())
                    == Some(SystemItem::HfsPrivate)
            })
        };
        let mut sizes = build_from(Arrays::of(index), &is_store, cancel)?;
        if index.summary().skipped > 0 {
            sizes.lower_bound = true;
        }
        Some(sizes)
    }

    /// `self` with the overcommitted flag set when the volume's bytes per
    /// kind add up to more than `total_bytes` (`VolumeInfo::total_bytes`):
    /// a damaged catalog can claim anything. `None` clears it.
    #[must_use]
    pub fn with_volume_total(mut self, total_bytes: Option<u64>) -> Self {
        self.overcommitted = total_bytes.is_some_and(|t| self.volume.total() > t);
        self
    }

    /// `None` when `entry` is not a folder. O(log folders).
    pub fn totals(&self, entry: u32) -> Option<Totals> {
        let r = self.rank(entry)?;
        Some(Totals {
            bytes: *self.bytes.get(r)?,
            files: u64::from(*self.files.get(r)?),
            folders: u64::from(*self.folders.get(r)?),
        })
    }

    /// The entry number of a folder, by its node: the entry that holds the
    /// children when a folder was reported more than once. Binary search.
    pub fn folder_entry(&self, node: NodeId) -> Option<u32> {
        let at = self.node_keys.binary_search(&node.0).ok()?;
        self.node_entries.get(at).copied()
    }

    /// The direct children of a folder, in index order; empty when `entry`
    /// is not a folder. O(children), plus a binary search per sub-folder.
    pub fn children(&self, entry: u32) -> impl Iterator<Item = u32> + '_ {
        let (next, stop, cursor) = match self.rank(entry) {
            Some(r) => (
                entry.saturating_add(1),
                self.end.get(r).copied().unwrap_or(0),
                r.saturating_add(1),
            ),
            None => (0, 0, 0),
        };
        Children {
            entry: &self.entry,
            end: &self.end,
            next,
            stop,
            cursor,
        }
    }

    /// Bytes per kind for the whole volume, each file once. O(1).
    pub fn volume_classes(&self) -> ClassBytes {
        self.volume
    }

    /// Bytes per kind inside one folder: every name counts, as in
    /// [`totals`](Self::totals), classed as for the volume (housekeeping,
    /// then the outermost package, then the file's own class). Directory
    /// links inside are not followed, so with links the sum can be lower
    /// than the folder's total. `index` must be the one this was built from.
    ///
    /// O(subtree): call it off the UI thread.
    pub fn classes_of(&self, index: &Index, entry: u32) -> ClassBytes {
        self.classes_in(Arrays::of(index), entry)
    }

    /// True when some sizes were clamped, the arrays were inconsistent, a
    /// directory link chain was cut, or the scan skipped structures.
    pub fn is_lower_bound(&self) -> bool {
        self.lower_bound
    }

    /// True when the volume's files claim more bytes than the volume has
    /// (see [`with_volume_total`](Self::with_volume_total)).
    pub fn is_overcommitted(&self) -> bool {
        self.overcommitted
    }

    /// Bytes held by the arrays.
    pub fn heap_bytes(&self) -> usize {
        use std::mem::size_of_val;
        [
            size_of_val(&*self.entry),
            size_of_val(&*self.bytes),
            size_of_val(&*self.files),
            size_of_val(&*self.folders),
            size_of_val(&*self.end),
            size_of_val(&*self.node_keys),
            size_of_val(&*self.node_entries),
            size_of_val(&*self.stores),
        ]
        .iter()
        .fold(0usize, |sum, &b| sum.saturating_add(b))
    }

    /// Folder rank of `entry`.
    fn rank(&self, entry: u32) -> Option<usize> {
        self.entry.binary_search(&entry).ok()
    }

    fn classes_in(&self, a: Arrays<'_>, entry: u32) -> ClassBytes {
        let mut out = ClassBytes::default();
        let Some(r) = self.rank(entry) else {
            return out;
        };
        let (n, _) = a.len();
        let stop = (self.end.get(r).copied().unwrap_or(0) as usize).min(n);
        let mut clamped = false;

        // The outermost package among `entry` and its ancestors covers the
        // whole range.
        let mut package = None;
        let mut e = entry;
        for _ in 0..MAX_DEPTH {
            let f = a.flags(e as usize);
            if f.is_folder() && f.is_package() {
                package = Some((u32::MAX, f.kind_class()));
            }
            match a.parent.get(e as usize) {
                Some(&p) if p < e => e = p,
                _ => break,
            }
        }

        let mut i = (entry as usize).saturating_add(1);
        // Bounded: `i` grows every turn.
        for _ in 0..n {
            if i >= stop {
                break;
            }
            let at = idx(i);
            let step = i.saturating_add(1);
            if package.is_some_and(|(pe, _)| at >= pe) {
                package = None;
            }
            let f = a.flags(i);
            if f.is_folder() {
                if self.stores.contains(&at) {
                    // Counted under the names that link to it.
                    let skip_to = self.rank(at).and_then(|r| self.end.get(r)).copied();
                    i = skip_to.map_or(step, |e| (e as usize).max(step));
                    continue;
                }
                if package.is_none() && f.is_package() {
                    let pe = self.rank(at).and_then(|r| self.end.get(r)).copied();
                    package = Some((pe.unwrap_or(at), f.kind_class()));
                }
            } else {
                let own = a.own_bytes(i, &mut clamped);
                let class = package.map_or(f.kind_class(), |(_, c)| c);
                out.add(f.is_system_item(), class, own, &mut clamped);
            }
            i = step;
        }
        out
    }
}

/// The direct children of a folder, from the folder ranks and ends alone.
struct Children<'a> {
    entry: &'a [u32],
    end: &'a [u32],
    next: u32,
    stop: u32,
    /// A folder rank at or before the first folder at or after `next`.
    cursor: usize,
}

impl Iterator for Children<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        if self.next >= self.stop {
            return None;
        }
        let j = self.next;
        if self.entry.get(self.cursor).is_some_and(|&e| e < j) {
            self.cursor = self.entry.partition_point(|&e| e < j);
        }
        let step = j.saturating_add(1);
        self.next = match self.entry.get(self.cursor) {
            // A sub-folder: the next sibling is after its subtree.
            Some(&e) if e == j => self.end.get(self.cursor).map_or(step, |&e| e.max(step)),
            _ => step,
        };
        Some(j)
    }
}

/// The index arrays the build reads.
#[derive(Clone, Copy)]
struct Arrays<'a> {
    parent: &'a [u32],
    node: &'a [u64],
    size: &'a [u64],
    rsize: &'a [u32],
    flags: &'a [u16],
}

impl<'a> Arrays<'a> {
    fn of(index: &'a Index) -> Self {
        Self {
            parent: index.parents(),
            node: index.nodes(),
            size: index.sizes(),
            rsize: index.rsizes(),
            flags: index.flag_words(),
        }
    }

    /// Entries every array has (at most [`MAX_ENTRIES`]), and whether all
    /// the arrays have exactly that many.
    fn len(&self) -> (usize, bool) {
        let lens = [
            self.parent.len(),
            self.node.len(),
            self.size.len(),
            self.rsize.len(),
            self.flags.len(),
        ];
        let max = usize::try_from(MAX_ENTRIES).unwrap_or(usize::MAX);
        let n = lens.iter().copied().min().unwrap_or(0).min(max);
        (n, lens.iter().all(|&l| l == n))
    }

    fn flags(&self, i: usize) -> IndexFlags {
        IndexFlags::from_bits(self.flags.get(i).copied().unwrap_or(0))
    }

    /// Data plus resource fork of entry `i`; unknown sizes count 0.
    fn own_bytes(&self, i: usize, clamped: &mut bool) -> u64 {
        let size = match self.size.get(i) {
            Some(&s) if s != NO_SIZE => s,
            _ => 0,
        };
        let rsize = match self.rsize.get(i) {
            Some(&r) if r != NO_RSIZE => u64::from(r),
            _ => 0,
        };
        size.checked_add(rsize).unwrap_or_else(|| {
            *clamped = true;
            u64::MAX
        })
    }
}

/// An entry number from a position below [`MAX_ENTRIES`].
fn idx(i: usize) -> u32 {
    u32::try_from(i).unwrap_or(u32::MAX)
}

fn add_u64(slot: &mut u64, value: u64, clamped: &mut bool) {
    match slot.checked_add(value) {
        Some(v) => *slot = v,
        None => {
            *slot = u64::MAX;
            *clamped = true;
        }
    }
}

fn add_u32(slot: &mut u32, value: u64, clamped: &mut bool) {
    match u32::try_from(u64::from(*slot).saturating_add(value)) {
        Ok(v) => *slot = v,
        Err(_) => {
            *slot = u32::MAX;
            *clamped = true;
        }
    }
}

/// A folder total while links are resolved.
#[derive(Debug, Clone, Copy, Default)]
struct Sum {
    bytes: u64,
    files: u64,
    folders: u64,
}

impl Sum {
    fn plus(self, other: Self, clamped: &mut bool) -> Self {
        let mut out = self;
        add_u64(&mut out.bytes, other.bytes, clamped);
        add_u64(&mut out.files, other.files, clamped);
        add_u64(&mut out.folders, other.folders, clamped);
        out
    }
}

/// Which nodes' bytes `volume` already holds.
enum Seen {
    /// One bit per node number.
    Dense(Vec<u64>),
    /// One bit per entry of `keys`, the sorted distinct file nodes.
    Sparse { keys: Vec<u64>, bits: Vec<u64> },
}

impl Seen {
    fn new(a: Arrays<'_>, n: usize, max_node: u64) -> Self {
        if max_node < DENSE_NODE_LIMIT {
            let words = usize::try_from(max_node / 64).map_or(0, |w| w.saturating_add(1));
            return Self::Dense(vec![0; words]);
        }
        let mut keys: Vec<u64> = a
            .flags
            .iter()
            .zip(a.node)
            .take(n)
            .filter(|&(&f, _)| !IndexFlags::from_bits(f).is_folder())
            .map(|(_, &node)| node)
            .collect();
        keys.sort_unstable();
        keys.dedup();
        let bits = vec![0; keys.len().div_ceil(64)];
        Self::Sparse { keys, bits }
    }

    /// Marks `node` seen. True when it was seen before.
    fn mark(&mut self, node: u64) -> bool {
        let (bits, slot) = match self {
            Self::Dense(bits) => (bits, node),
            Self::Sparse { keys, bits } => match keys.binary_search(&node) {
                Ok(at) => (bits, at as u64),
                Err(_) => return false,
            },
        };
        let Some(word) = usize::try_from(slot / 64)
            .ok()
            .and_then(|w| bits.get_mut(w))
        else {
            return false;
        };
        let bit = 1u64.wrapping_shl(u32::try_from(slot % 64).unwrap_or(0));
        let seen = *word & bit != 0;
        *word |= bit;
        seen
    }
}

/// State of a link target while links are resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Visit {
    New,
    InProgress,
    Done,
}

#[derive(Debug, Clone, Copy)]
struct Target {
    rank: u32,
    visit: Visit,
    total: Sum,
}

#[derive(Debug, Clone, Copy)]
struct Link {
    rank: u32,
    entry: u32,
    /// Index into the targets.
    target: u32,
}

/// Step 5: the total of every link, from its target's step 4 total plus the
/// links inside the target, memoized per link and per target.
struct Resolver<'a> {
    links: &'a [Link],
    targets: Vec<Target>,
    values: Vec<Option<Sum>>,
    entry: &'a [u32],
    end: &'a [u32],
    bytes: &'a [u64],
    files: &'a [u32],
    folders: &'a [u32],
    /// Link visits so far, and the most allowed: every link once per level
    /// of nesting. Past it, step 5 is given up.
    work: u64,
    budget: u64,
    given_up: bool,
    lower: bool,
    cancel: &'a AtomicBool,
    cancelled: bool,
}

impl Resolver<'_> {
    /// Every link's total, or `None` when cancelled. Empty when the work
    /// budget ran out.
    fn run(mut self) -> Option<(Vec<(u32, Sum)>, bool)> {
        for k in 0..self.links.len() {
            if k & CANCEL_MASK == 0 && self.cancel.load(Ordering::Relaxed) {
                return None;
            }
            self.value(k, 0);
            if self.cancelled {
                return None;
            }
            if self.given_up {
                return Some((Vec::new(), true));
            }
        }
        let out = self
            .links
            .iter()
            .zip(&self.values)
            .map(|(l, v)| (l.rank, v.unwrap_or_default()))
            .collect();
        Some((out, self.lower))
    }

    fn stop(&self) -> bool {
        self.given_up || self.cancelled
    }

    /// The total of link `k`, decided once.
    fn value(&mut self, k: usize, depth: u32) -> Sum {
        if let Some(Some(v)) = self.values.get(k) {
            return *v;
        }
        let Some(target) = self.links.get(k).map(|l| l.target as usize) else {
            return Sum::default();
        };
        let v = self.evaluate(target, depth.saturating_add(1));
        if self.stop() {
            return Sum::default();
        }
        match self.values.get_mut(k) {
            // Decided inside that evaluation: the link closes a cycle.
            Some(Some(decided)) => *decided,
            Some(slot) => {
                *slot = Some(v);
                v
            }
            None => v,
        }
    }

    /// The total of target `t`: its step 4 total plus every link inside it.
    fn evaluate(&mut self, t: usize, depth: u32) -> Sum {
        let Some(&target) = self.targets.get(t) else {
            return Sum::default();
        };
        match target.visit {
            Visit::Done => return target.total,
            // A folder that links into itself: the link ends at 0.
            Visit::InProgress => {
                self.lower = true;
                return Sum::default();
            }
            Visit::New => {}
        }
        if depth > MAX_LINK_NESTING {
            self.lower = true;
            return Sum::default();
        }
        let r = target.rank as usize;
        let mut sum = Sum {
            bytes: self.bytes.get(r).copied().unwrap_or(0),
            files: self.files.get(r).copied().map_or(0, u64::from),
            folders: self.folders.get(r).copied().map_or(0, u64::from),
        };
        let first = self.entry.get(r).copied().unwrap_or(0);
        let stop = self.end.get(r).copied().unwrap_or(first);
        let lo = self.links.partition_point(|l| l.entry <= first);
        let hi = self.links.partition_point(|l| l.entry < stop).max(lo);
        if let Some(slot) = self.targets.get_mut(t) {
            slot.visit = Visit::InProgress;
        }
        for k in lo..hi {
            self.work = self.work.saturating_add(1);
            if self.work > self.budget {
                self.given_up = true;
            } else if self.work & (CANCEL_MASK as u64) == 0 && self.cancel.load(Ordering::Relaxed) {
                self.cancelled = true;
            }
            if self.stop() {
                return Sum::default();
            }
            let v = self.value(k, depth);
            if self.stop() {
                return Sum::default();
            }
            let mut clamped = false;
            sum = sum.plus(v, &mut clamped);
            self.lower |= clamped;
        }
        if let Some(slot) = self.targets.get_mut(t) {
            slot.visit = Visit::Done;
            slot.total = sum;
        }
        sum
    }
}

/// The build's working state.
struct Builder<'a> {
    a: Arrays<'a>,
    n: usize,
    cancel: &'a AtomicBool,
    /// Per entry: its folder rank, or [`NONE`].
    rank: Vec<u32>,
    entry: Vec<u32>,
    end: Vec<u32>,
    bytes: Vec<u64>,
    files: Vec<u32>,
    folders: Vec<u32>,
    stores: Vec<u32>,
    volume: ClassBytes,
    lower: bool,
}

/// The build on raw arrays; `is_store` says whether a direct child of the
/// root (a folder) is a private store.
fn build_from(
    a: Arrays<'_>,
    is_store: &dyn Fn(u32) -> bool,
    cancel: &AtomicBool,
) -> Option<IndexSizes> {
    let (n, consistent) = a.len();
    let mut b = Builder {
        a,
        n,
        cancel,
        rank: Vec::new(),
        entry: Vec::new(),
        end: Vec::new(),
        bytes: Vec::new(),
        files: Vec::new(),
        folders: Vec::new(),
        stores: Vec::new(),
        volume: ClassBytes::default(),
        lower: !consistent,
    };
    let max_node = b.ranks()?;
    b.ends()?;
    b.find_stores(is_store);
    b.own_totals(max_node)?;
    let (node_keys, node_entries, links, targets) = b.groups()?;
    if links.is_empty() {
        b.accumulate(&[])?;
    } else {
        let own = (b.bytes.clone(), b.files.clone(), b.folders.clone());
        b.accumulate(&[])?;
        let budget = (links.len() as u64)
            .saturating_mul(u64::from(MAX_LINK_NESTING))
            .saturating_add(n as u64);
        let resolver = Resolver {
            links: &links,
            values: vec![None; links.len()],
            targets,
            entry: &b.entry,
            end: &b.end,
            bytes: &b.bytes,
            files: &b.files,
            folders: &b.folders,
            work: 0,
            budget,
            given_up: false,
            lower: false,
            cancel,
            cancelled: false,
        };
        let (overrides, lower) = resolver.run()?;
        b.lower |= lower;
        if !overrides.is_empty() {
            (b.bytes, b.files, b.folders) = own;
            b.accumulate(&overrides)?;
        }
    }
    let Builder {
        entry,
        end,
        bytes,
        files,
        folders,
        stores,
        volume,
        lower,
        ..
    } = b;
    Some(IndexSizes {
        entry: entry.into(),
        bytes: bytes.into(),
        files: files.into(),
        folders: folders.into(),
        end: end.into(),
        node_keys: node_keys.into(),
        node_entries: node_entries.into(),
        stores: stores.into(),
        volume,
        lower_bound: lower,
        overcommitted: false,
    })
}

impl Builder<'_> {
    fn cancelled(&self, counter: usize) -> bool {
        counter & CANCEL_MASK == 0 && self.cancel.load(Ordering::Relaxed)
    }

    fn rank_of(&self, e: u32) -> Option<usize> {
        match self.rank.get(e as usize) {
            Some(&r) if r != NONE => Some(r as usize),
            _ => None,
        }
    }

    /// The rank of entry `i`'s parent, when it is a folder before `i`.
    fn parent_rank(&self, i: usize) -> Option<usize> {
        let p = *self.a.parent.get(i)?;
        if p as usize >= i {
            return None;
        }
        self.rank_of(p)
    }

    fn end_of(&self, e: u32) -> u32 {
        self.rank_of(e)
            .and_then(|r| self.end.get(r))
            .copied()
            .unwrap_or_else(|| e.saturating_add(1))
    }

    /// The rank table and the folder list. Returns the largest file node.
    fn ranks(&mut self) -> Option<u64> {
        let mut max_node = 0u64;
        self.rank = Vec::with_capacity(self.n);
        for (i, (&f, &node)) in self
            .a
            .flags
            .iter()
            .zip(self.a.node)
            .take(self.n)
            .enumerate()
        {
            if self.cancelled(i) {
                return None;
            }
            if IndexFlags::from_bits(f).is_folder() {
                self.rank.push(idx(self.entry.len()));
                self.entry.push(idx(i));
            } else {
                self.rank.push(NONE);
                max_node = max_node.max(node);
            }
        }
        let folders = self.entry.len();
        self.bytes = vec![0; folders];
        self.files = vec![0; folders];
        self.folders = vec![0; folders];
        Some(max_node)
    }

    /// Step 2: `end` of every folder, bottom-up.
    fn ends(&mut self) -> Option<()> {
        self.end = self.entry.iter().map(|&e| e.saturating_add(1)).collect();
        for i in (1..self.n).rev() {
            if self.cancelled(i) {
                return None;
            }
            let here = self.end_of(idx(i));
            if let Some(pr) = self.parent_rank(i)
                && let Some(e) = self.end.get_mut(pr)
            {
                *e = (*e).max(here);
            }
        }
        Some(())
    }

    /// Step 1: the private stores among the root's direct children.
    fn find_stores(&mut self, is_store: &dyn Fn(u32) -> bool) {
        if self.rank_of(0) != Some(0) {
            return;
        }
        let stop = self.end_of(0);
        let mut j = 1u32;
        // Bounded: `j` grows every turn.
        for _ in 0..self.n {
            if j >= stop {
                break;
            }
            let step = j.saturating_add(1);
            if self.rank_of(j).is_none() {
                j = step;
                continue;
            }
            if self.a.parent.get(j as usize) == Some(&0) && is_store(j) {
                if self.stores.len() < MAX_STORES {
                    self.stores.push(j);
                } else {
                    self.lower = true;
                }
            }
            j = self.end_of(j).max(step);
        }
    }

    /// Step 3: own totals of each folder, and `volume`: the entries outside
    /// the private stores first, then those inside.
    fn own_totals(&mut self, max_node: u64) -> Option<()> {
        let n = idx(self.n);
        let mut seen = Seen::new(self.a, self.n, max_node);
        let mut outside = Vec::with_capacity(self.stores.len().saturating_add(1));
        let mut inside = Vec::with_capacity(self.stores.len());
        let mut at = 0u32;
        for &s in &self.stores {
            let e = self.end_of(s).min(n);
            outside.push((at, s.max(at), None));
            inside.push((s, e, Some(s)));
            at = e.max(at);
        }
        outside.push((at, n, None));
        for (start, stop, store) in outside.into_iter().chain(inside) {
            self.visit(start, stop, store, &mut seen)?;
        }
        Some(())
    }

    /// Step 3 over entries `start..stop`; `store` is the private store they
    /// are in.
    fn visit(&mut self, start: u32, stop: u32, store: Option<u32>, seen: &mut Seen) -> Option<()> {
        let mut package: Option<(u32, KindClass)> = None;
        let mut clamped = false;
        for i in (start as usize)..(stop as usize) {
            if self.cancelled(i) {
                return None;
            }
            let at = idx(i);
            // The root, and a store's own entry: neither is inside a folder
            // that counts it.
            if i == 0 || store == Some(at) {
                continue;
            }
            if package.is_some_and(|(pe, _)| at >= pe) {
                package = None;
            }
            let f = self.a.flags(i);
            let pr = self.parent_rank(i);
            if pr.is_none() {
                self.lower = true;
            }
            if f.is_folder() {
                if let Some(slot) = pr.and_then(|pr| self.folders.get_mut(pr)) {
                    add_u32(slot, 1, &mut clamped);
                }
                if package.is_none() && f.is_package() {
                    package = Some((self.end_of(at), f.kind_class()));
                }
                continue;
            }
            let own = self.a.own_bytes(i, &mut clamped);
            if let Some(pr) = pr {
                if let Some(slot) = self.bytes.get_mut(pr) {
                    add_u64(slot, own, &mut clamped);
                }
                if let Some(slot) = self.files.get_mut(pr) {
                    add_u32(slot, 1, &mut clamped);
                }
            }
            if seen.mark(self.a.node.get(i).copied().unwrap_or(0)) {
                continue;
            }
            // Directly inside a store and claimed by no link: an orphan.
            let orphan = store.is_some_and(|s| self.a.parent.get(i) == Some(&s));
            let class = package.map_or(f.kind_class(), |(_, c)| c);
            self.volume
                .add(f.is_system_item() || orphan, class, own, &mut clamped);
        }
        self.lower |= clamped;
        Some(())
    }

    /// Step 4: each folder's totals added to its parent's, bottom-up. A
    /// folder in `overrides` (ascending by rank) takes the total given there
    /// instead of its own. A private store is not added to the root.
    fn accumulate(&mut self, overrides: &[(u32, Sum)]) -> Option<()> {
        let mut clamped = false;
        let mut cursor = overrides.len();
        for r in (1..self.entry.len()).rev() {
            if self.cancelled(r) {
                return None;
            }
            while let Some(k) = cursor.checked_sub(1)
                && overrides.get(k).is_some_and(|&(lr, _)| lr as usize > r)
            {
                cursor = k;
            }
            if let Some(k) = cursor.checked_sub(1)
                && let Some(&(lr, v)) = overrides.get(k)
                && lr as usize == r
            {
                cursor = k;
                if let Some(s) = self.bytes.get_mut(r) {
                    *s = v.bytes;
                }
                if let Some(s) = self.files.get_mut(r) {
                    *s = 0;
                    add_u32(s, v.files, &mut clamped);
                }
                if let Some(s) = self.folders.get_mut(r) {
                    *s = 0;
                    add_u32(s, v.folders, &mut clamped);
                }
            }
            let Some(&e) = self.entry.get(r) else {
                continue;
            };
            let Some(pr) = self.parent_rank(e as usize) else {
                continue;
            };
            if pr == 0 && self.stores.contains(&e) {
                continue;
            }
            let bytes = self.bytes.get(r).copied().unwrap_or(0);
            let files = self.files.get(r).copied().map_or(0, u64::from);
            let folders = self.folders.get(r).copied().map_or(0, u64::from);
            if let Some(s) = self.bytes.get_mut(pr) {
                add_u64(s, bytes, &mut clamped);
            }
            if let Some(s) = self.files.get_mut(pr) {
                add_u32(s, files, &mut clamped);
            }
            if let Some(s) = self.folders.get_mut(pr) {
                add_u32(s, folders, &mut clamped);
            }
        }
        self.lower |= clamped;
        Some(())
    }

    fn has_children(&self, r: usize) -> bool {
        match (self.entry.get(r), self.end.get(r)) {
            (Some(&e), Some(&end)) => end > e.saturating_add(1),
            _ => false,
        }
    }

    /// Step 6, and the links and targets of step 5: the folders sorted by
    /// node; for a node with several entries, the one with children is the
    /// target and the others are links.
    #[allow(clippy::type_complexity)]
    fn groups(&mut self) -> Option<(Vec<u64>, Vec<u32>, Vec<Link>, Vec<Target>)> {
        let mut pairs: Vec<(u64, u32)> = Vec::with_capacity(self.entry.len());
        for (r, &e) in self.entry.iter().enumerate() {
            if self.cancelled(r) {
                return None;
            }
            pairs.push((self.a.node.get(e as usize).copied().unwrap_or(0), idx(r)));
        }
        pairs.sort_unstable();
        let mut keys = Vec::with_capacity(pairs.len());
        let mut entries = Vec::with_capacity(pairs.len());
        let mut links = Vec::new();
        let mut targets = Vec::new();
        let mut lower = false;
        let entry_of = |r: u32| self.entry.get(r as usize).copied().unwrap_or(0);
        for (count, group) in pairs.chunk_by(|x, y| x.0 == y.0).enumerate() {
            if self.cancelled(count) {
                return None;
            }
            let Some(&(node, first)) = group.first() else {
                continue;
            };
            let target = if group.len() > 1 {
                group
                    .iter()
                    .map(|&(_, r)| r)
                    .find(|&r| self.has_children(r as usize))
            } else {
                None
            };
            keys.push(node);
            let Some(target) = target else {
                entries.push(entry_of(first));
                continue;
            };
            entries.push(entry_of(target));
            let slot = idx(targets.len());
            targets.push(Target {
                rank: target,
                visit: Visit::New,
                total: Sum::default(),
            });
            for &(_, r) in group {
                // The root is never a link.
                if r == target || r == 0 {
                    continue;
                }
                if self.has_children(r as usize) {
                    // Two entries of one folder with children: the build
                    // never makes this; the second one's own children are
                    // replaced by the target's.
                    lower = true;
                }
                links.push(Link {
                    rank: r,
                    entry: entry_of(r),
                    target: slot,
                });
            }
        }
        self.lower |= lower;
        links.sort_unstable_by_key(|l| l.rank);
        Some((keys, entries, links, targets))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{EntryFlags, NodeKind, ScanSummary};
    use crate::search::index::IndexParts;
    use crate::search::kind::classify_entry;

    static NO_CANCEL: AtomicBool = AtomicBool::new(false);

    /// One entry of a hand-built index, in pre-order.
    struct Spec {
        name: String,
        parent: u32,
        kind: NodeKind,
        node: u64,
        size: Option<u64>,
        rsize: Option<u32>,
        hidden: bool,
        package: bool,
        no_system_bit: bool,
    }

    /// A hand-built index. Entry 0 is the root (node 2). The test adds
    /// entries in pre-order: a folder's subtree right after it.
    struct Tree(Vec<Spec>);

    impl Tree {
        fn new() -> Self {
            Self(vec![Spec {
                name: "Volume".to_owned(),
                parent: 0,
                kind: NodeKind::Directory,
                node: 2,
                size: None,
                rsize: None,
                hidden: false,
                package: false,
                no_system_bit: false,
            }])
        }

        fn push(
            &mut self,
            parent: u32,
            name: &str,
            kind: NodeKind,
            node: u64,
            size: Option<u64>,
        ) -> u32 {
            self.0.push(Spec {
                name: name.to_owned(),
                parent,
                kind,
                node,
                size,
                rsize: None,
                hidden: false,
                package: false,
                no_system_bit: false,
            });
            (self.0.len() - 1) as u32
        }

        fn dir(&mut self, parent: u32, name: &str, node: u64) -> u32 {
            self.push(parent, name, NodeKind::Directory, node, None)
        }

        fn file(&mut self, parent: u32, name: &str, node: u64, size: u64) -> u32 {
            self.push(parent, name, NodeKind::File, node, Some(size))
        }

        fn last(&mut self) -> &mut Spec {
            self.0.last_mut().unwrap()
        }

        fn index(&self) -> Index {
            let mut names = String::new();
            let mut offsets = vec![0u32];
            for s in &self.0 {
                names.push_str(&s.name.replace('\0', "\u{1a}"));
                names.push('\0');
                offsets.push(names.len() as u32);
            }
            let mut flags: Vec<u16> = Vec::new();
            for (i, s) in self.0.iter().enumerate() {
                let mut ef = EntryFlags::EMPTY;
                if s.hidden {
                    ef = ef.with(EntryFlags::HIDDEN);
                }
                if s.package {
                    ef = ef.with(EntryFlags::PACKAGE);
                }
                let folder = s.kind == NodeKind::Directory;
                let class = classify_entry(&s.name, folder, None);
                let mut f = IndexFlags::new(s.kind, ef, s.rsize.is_some_and(|r| r > 0), class);
                let own = i > 0 && system_item(&s.name, s.kind, s.parent == 0, s.hidden).is_some();
                let inherited =
                    i > 0 && IndexFlags::from_bits(flags[s.parent as usize]).is_system_item();
                if (own || inherited) && !s.no_system_bit {
                    f = f.with_system_item();
                }
                flags.push(f.bits());
            }
            Index::from_parts(IndexParts {
                names: names.into_boxed_str(),
                offsets: offsets.into(),
                parent: self.0.iter().map(|s| s.parent).collect(),
                node: self.0.iter().map(|s| s.node).collect(),
                size: self
                    .0
                    .iter()
                    .map(|s| match s.kind {
                        NodeKind::Directory => NO_SIZE,
                        _ => s.size.unwrap_or(NO_SIZE),
                    })
                    .collect(),
                modified: vec![crate::search::index::NO_TIME; self.0.len()].into(),
                rsize: self.0.iter().map(|s| s.rsize.unwrap_or(NO_RSIZE)).collect(),
                flags: flags.into(),
                identity: None,
                summary: ScanSummary::default(),
            })
        }
    }

    fn sizes(index: &Index) -> IndexSizes {
        IndexSizes::build(index, &NO_CANCEL).unwrap()
    }

    fn t(bytes: u64, files: u64, folders: u64) -> Option<Totals> {
        Some(Totals {
            bytes,
            files,
            folders,
        })
    }

    const FILE_STORE: &str = "\0\0\0\0HFS+ Private Data";
    const DIR_STORE: &str = ".HFS+ Private Directory Data\r";

    #[test]
    fn known_totals_children_and_folder_entry() {
        let mut tree = Tree::new();
        let a = tree.dir(0, "A", 10);
        let a1 = tree.file(a, "a1.txt", 11, 100);
        let b = tree.dir(a, "B", 12);
        let b1 = tree.file(b, "b1.png", 13, 50);
        tree.last().rsize = Some(5);
        let c = tree.file(0, "c.txt", 14, 7);
        let e = tree.dir(0, "E", 15);
        let s = tree.push(0, "s", NodeKind::Symlink, 16, None);
        let index = tree.index();
        let sz = sizes(&index);

        assert_eq!(sz.totals(0), t(162, 4, 3));
        assert_eq!(sz.totals(a), t(155, 2, 1));
        assert_eq!(sz.totals(b), t(55, 1, 0));
        assert_eq!(sz.totals(e), t(0, 0, 0));
        assert_eq!(sz.totals(a1), None);
        assert_eq!(sz.totals(99), None);
        assert!(!sz.is_lower_bound());

        assert_eq!(sz.children(0).collect::<Vec<_>>(), vec![a, c, e, s]);
        assert_eq!(sz.children(a).collect::<Vec<_>>(), vec![a1, b]);
        assert_eq!(sz.children(b).collect::<Vec<_>>(), vec![b1]);
        assert_eq!(sz.children(e).count(), 0);
        assert_eq!(sz.children(a1).count(), 0);
        assert_eq!(sz.children(u32::MAX).count(), 0);

        assert_eq!(sz.folder_entry(NodeId(12)), Some(b));
        assert_eq!(sz.folder_entry(NodeId(2)), Some(0));
        assert_eq!(sz.folder_entry(NodeId(11)), None);
        assert_eq!(sz.folder_entry(NodeId(999)), None);

        let volume = sz.volume_classes();
        assert_eq!(volume.total(), 162);
        assert_eq!(volume.of(KindClass::Image), 55);
        assert_eq!(volume.of(KindClass::Document), 107);
        assert_eq!(volume.system, 0);
        assert_eq!(sz.classes_of(&index, 0), volume);
        assert_eq!(sz.classes_of(&index, a).total(), 155);
        assert_eq!(sz.classes_of(&index, a1), ClassBytes::default());
        assert!(sz.heap_bytes() > 0);
    }

    /// A file with three visible names and its `iNode` in the private store,
    /// and an `iNode` no name links to.
    fn three_names() -> (Tree, [u32; 4]) {
        let mut tree = Tree::new();
        let store = tree.dir(0, FILE_STORE, 18);
        tree.last().hidden = true;
        tree.file(store, "iNode7", 31, 77);
        tree.file(store, "iNode8", 32, 9);
        let docs = tree.dir(0, "Docs", 16);
        tree.file(docs, "photo.jpg", 31, 77);
        tree.file(docs, "copy.jpg", 31, 77);
        tree.file(0, "third.jpg", 31, 77);
        (tree, [0, store, docs, 0])
    }

    #[test]
    fn three_names_count_three_times_and_once_in_volume() {
        let (tree, [root, store, docs, _]) = three_names();
        let index = tree.index();
        let sz = sizes(&index);
        assert_eq!(sz.totals(docs), t(154, 2, 0));
        // The store is not in the root's total.
        assert_eq!(sz.totals(root), t(231, 3, 1));
        assert_eq!(sz.totals(store), t(86, 2, 0));
        let v = sz.volume_classes();
        // Classed by its visible name, not by `iNode7`.
        assert_eq!(v.of(KindClass::Image), 77);
        assert_eq!(v.of(KindClass::Other), 0);
        // The orphan.
        assert_eq!(v.system, 9);
        assert_eq!(v.total(), 86);
        assert!(!sz.is_lower_bound());
        // Nor in the root's kinds.
        assert_eq!(sz.classes_of(&index, root).total(), 231);
    }

    #[test]
    fn orphan_in_a_store_is_system_without_the_housekeeping_bit() {
        let (mut tree, _) = three_names();
        for s in &mut tree.0 {
            s.no_system_bit = true;
        }
        let index = tree.index();
        assert!(!index.flags(2).unwrap().is_system_item());
        let v = sizes(&index).volume_classes();
        assert_eq!(v.system, 9);
        assert_eq!(v.of(KindClass::Image), 77);
        assert_eq!(v.of(KindClass::Other), 0);
    }

    #[test]
    fn directory_link_into_the_private_store() {
        let mut tree = Tree::new();
        let store = tree.dir(0, DIR_STORE, 19);
        let target = tree.dir(store, "dir_9", 41);
        tree.file(target, "deep.txt", 42, 5);
        tree.file(target, "inner.txt", 43, 3);
        let docs = tree.dir(0, "Docs", 16);
        let again = tree.dir(docs, "again", 41);
        tree.file(docs, "x.txt", 44, 10);
        let linked = tree.dir(0, "linked dir", 41);
        let index = tree.index();
        let sz = sizes(&index);

        assert_eq!(sz.totals(linked), t(8, 2, 0));
        assert_eq!(sz.totals(again), t(8, 2, 0));
        assert_eq!(sz.totals(target), t(8, 2, 0));
        assert_eq!(sz.totals(docs), t(18, 3, 1));
        assert_eq!(sz.totals(0), t(26, 5, 3));
        assert_eq!(sz.folder_entry(NodeId(41)), Some(target));
        assert_eq!(sz.children(linked).count(), 0);
        let v = sz.volume_classes();
        // Each file once. What sits only in the store has bit 6 (passed
        // down from the store), so it is housekeeping.
        assert_eq!(v.total(), 18);
        assert_eq!(v.of(KindClass::Document), 10);
        assert_eq!(v.system, 8);
        assert!(!sz.is_lower_bound());
    }

    #[test]
    fn directory_link_as_the_scan_reports_it() {
        // No private store; the first report holds the children.
        let mut tree = Tree::new();
        let linked = tree.dir(0, "linked dir", 41);
        tree.file(linked, "deep.txt", 42, 5);
        let sub = tree.dir(linked, "sub", 50);
        tree.file(sub, "s.txt", 51, 4);
        let docs = tree.dir(0, "Docs", 16);
        let again = tree.dir(docs, "again", 41);
        let index = tree.index();
        let sz = sizes(&index);

        assert_eq!(sz.totals(linked), t(9, 2, 1));
        assert_eq!(sz.totals(again), t(9, 2, 1));
        assert_eq!(sz.totals(docs), t(9, 2, 2));
        assert_eq!(sz.totals(0), t(18, 4, 5));
        assert_eq!(sz.folder_entry(NodeId(41)), Some(linked));
        assert_eq!(sz.volume_classes().total(), 9);
        assert!(!sz.is_lower_bound());
    }

    #[test]
    fn link_inside_a_linked_folder() {
        let mut tree = Tree::new();
        let a = tree.dir(0, "A", 60);
        tree.file(a, "a.txt", 61, 1);
        let link_b = tree.dir(a, "to B", 70);
        let b = tree.dir(0, "B", 70);
        tree.file(b, "b.txt", 71, 2);
        let link_a = tree.dir(0, "to A", 60);
        let index = tree.index();
        let sz = sizes(&index);

        assert_eq!(sz.totals(link_b), t(2, 1, 0));
        assert_eq!(sz.totals(b), t(2, 1, 0));
        assert_eq!(sz.totals(a), t(3, 2, 1));
        assert_eq!(sz.totals(link_a), t(3, 2, 1));
        assert_eq!(sz.totals(0), t(8, 5, 5));
        assert_eq!(sz.volume_classes().total(), 3);
        assert!(!sz.is_lower_bound());
    }

    #[test]
    fn folder_that_links_into_itself_ends_at_zero() {
        let mut tree = Tree::new();
        let a = tree.dir(0, "A", 60);
        tree.file(a, "a.txt", 61, 4);
        let itself = tree.dir(a, "itself", 60);
        let link_a = tree.dir(0, "to A", 60);
        let index = tree.index();
        let sz = sizes(&index);

        assert_eq!(sz.totals(itself), t(0, 0, 0));
        assert_eq!(sz.totals(a), t(4, 1, 1));
        assert_eq!(sz.totals(link_a), t(4, 1, 1));
        assert_eq!(sz.totals(0), t(8, 2, 4));
        assert!(sz.is_lower_bound());
    }

    #[test]
    fn two_folders_linking_into_each_other_end() {
        let mut tree = Tree::new();
        let a = tree.dir(0, "A", 60);
        tree.file(a, "a.txt", 61, 1);
        tree.dir(a, "to B", 70);
        let b = tree.dir(0, "B", 70);
        tree.file(b, "b.txt", 71, 2);
        tree.dir(b, "to A", 60);
        let index = tree.index();
        let sz = sizes(&index);
        assert!(sz.is_lower_bound());
        assert!(sz.totals(a).is_some() && sz.totals(b).is_some());
    }

    #[test]
    fn chain_longer_than_the_nesting_limit_is_cut() {
        // Folder k holds a link to folder k + 1; 100 of them.
        let mut tree = Tree::new();
        let mut firsts = Vec::new();
        for k in 0..100u64 {
            let f = tree.dir(0, &format!("F{k}"), 1000 + k);
            tree.file(f, "x.txt", 5000 + k, 1);
            tree.dir(f, "next", 1001 + k);
            firsts.push(f);
        }
        // The last link's target has no entry with children: an empty link.
        let index = tree.index();
        let sz = sizes(&index);
        assert!(sz.is_lower_bound());
        // The last folders are near the end of the chain and exact.
        assert_eq!(sz.totals(firsts[99]), t(1, 1, 1));
        assert_eq!(sz.totals(firsts[98]), t(2, 2, 2));
    }

    #[test]
    fn package_bytes_go_to_its_class_outermost_wins() {
        let mut tree = Tree::new();
        let app = tree.dir(0, "Foo.app", 10);
        tree.last().package = true;
        let contents = tree.dir(app, "Contents", 11);
        let inner = tree.dir(contents, "data.bundle", 12);
        tree.last().package = true;
        tree.file(inner, "x.png", 13, 30);
        tree.file(contents, "readme.txt", 14, 20);
        tree.file(0, "pic.png", 15, 5);
        let lib = tree.dir(0, "Lib.bundle", 16);
        tree.last().package = true;
        tree.file(lib, "y.png", 17, 6);
        let index = tree.index();
        assert!(index.flags(app).unwrap().is_package());
        let sz = sizes(&index);
        let v = sz.volume_classes();
        assert_eq!(v.of(KindClass::Application), 50);
        assert_eq!(v.of(KindClass::Image), 5);
        assert_eq!(v.of(KindClass::Folder), 6);
        assert_eq!(v.of(KindClass::Document), 0);
        // Inside the package, from a folder below it.
        let below = sz.classes_of(&index, inner);
        assert_eq!(below.of(KindClass::Application), 30);
        assert_eq!(below.of(KindClass::Image), 0);
        assert_eq!(sz.classes_of(&index, contents).total(), 50);
    }

    #[test]
    fn housekeeping_bytes_go_to_system() {
        let mut tree = Tree::new();
        let spot = tree.dir(0, ".Spotlight-V100", 10);
        let store = tree.dir(spot, "Store", 11);
        tree.file(store, "db.dat", 12, 100);
        tree.file(0, ".DS_Store", 13, 6);
        tree.file(0, "a.txt", 14, 1);
        let index = tree.index();
        let sz = sizes(&index);
        let v = sz.volume_classes();
        assert_eq!(v.system, 106);
        assert_eq!(v.of(KindClass::Document), 1);
        assert_eq!(v.of(KindClass::Other), 0);
        // Totals still count hidden and housekeeping bytes.
        assert_eq!(sz.totals(0), t(107, 3, 2));
        assert_eq!(sz.classes_of(&index, 0).system, 106);
        assert_eq!(sz.classes_of(&index, store).system, 100);
    }

    #[test]
    fn overcommitted_flag() {
        let mut tree = Tree::new();
        tree.file(0, "a.txt", 10, 100);
        let index = tree.index();
        let sz = sizes(&index);
        assert!(!sz.is_overcommitted());
        assert!(sz.clone().with_volume_total(Some(99)).is_overcommitted());
        assert!(!sz.clone().with_volume_total(Some(100)).is_overcommitted());
        let sz = sz.with_volume_total(Some(1));
        assert!(sz.is_overcommitted());
        assert!(!sz.with_volume_total(None).is_overcommitted());
    }

    #[test]
    fn cancel_returns_none() {
        let mut tree = Tree::new();
        let a = tree.dir(0, "A", 60);
        tree.file(a, "a.txt", 61, 4);
        tree.dir(0, "to A", 60);
        let index = tree.index();
        assert!(IndexSizes::build(&index, &AtomicBool::new(true)).is_none());
    }

    #[test]
    fn huge_sizes_saturate() {
        let mut tree = Tree::new();
        let a = tree.dir(0, "A", 10);
        tree.file(a, "big1", 11, u64::MAX - 1);
        tree.last().rsize = Some(10);
        tree.file(a, "big2", 12, u64::MAX - 1);
        tree.file(0, "small", 13, 3);
        let index = tree.index();
        let sz = sizes(&index);
        assert_eq!(sz.totals(a).unwrap().bytes, u64::MAX);
        assert_eq!(sz.totals(0).unwrap().bytes, u64::MAX);
        assert_eq!(sz.volume_classes().total(), u64::MAX);
        assert!(sz.is_lower_bound());
        assert!(sz.with_volume_total(Some(u64::MAX - 1)).is_overcommitted());
    }

    fn never_store(_: u32) -> bool {
        false
    }

    struct Raw {
        parent: Vec<u32>,
        node: Vec<u64>,
        size: Vec<u64>,
        rsize: Vec<u32>,
        flags: Vec<u16>,
    }

    impl Raw {
        fn arrays(&self) -> Arrays<'_> {
            Arrays {
                parent: &self.parent,
                node: &self.node,
                size: &self.size,
                rsize: &self.rsize,
                flags: &self.flags,
            }
        }

        fn sizes(&self) -> IndexSizes {
            build_from(self.arrays(), &never_store, &NO_CANCEL).unwrap()
        }
    }

    /// Every query on `sz`, over every entry and a few out of range.
    fn query_everything(sz: &IndexSizes, raw: &Raw) {
        let n = raw.parent.len() as u32;
        for e in (0..n.saturating_add(3)).chain([u32::MAX - 1, u32::MAX]) {
            let _ = sz.totals(e);
            assert!(sz.children(e).take(1 << 20).count() <= n as usize);
            let _ = sz.classes_in(raw.arrays(), e);
            let _ = sz.folder_entry(NodeId(u64::from(e)));
        }
        let _ = sz.volume_classes().total();
        let _ = sz.heap_bytes();
    }

    const D: u16 = IndexFlags::KIND_FOLDER;
    const F: u16 = IndexFlags::KIND_FILE;

    #[test]
    fn hostile_parents_and_lengths_never_panic() {
        let raw = Raw {
            // 2 points forward, 3 at itself, 4 out of range, 5 at a file.
            parent: vec![0, 0, 5, 3, 99, 1, 0],
            node: vec![2, 3, 4, 5, 6, 7, 3],
            size: vec![NO_SIZE, 10, u64::MAX - 1, 4, 5, 6, NO_SIZE],
            rsize: vec![NO_RSIZE, 1, 7, NO_RSIZE, NO_RSIZE, NO_RSIZE, NO_RSIZE],
            flags: vec![D, F, D, D, F, F, D],
        };
        let sz = raw.sizes();
        assert!(sz.is_lower_bound());
        query_everything(&sz, &raw);

        // Arrays of different lengths.
        let short = Raw {
            parent: raw.parent.clone(),
            node: vec![2],
            size: raw.size.clone(),
            rsize: raw.rsize.clone(),
            flags: raw.flags.clone(),
        };
        let sz = short.sizes();
        assert!(sz.is_lower_bound());
        query_everything(&sz, &short);

        // No entry at all, and a root that is a file.
        let empty = Raw {
            parent: vec![],
            node: vec![],
            size: vec![],
            rsize: vec![],
            flags: vec![],
        };
        let sz = empty.sizes();
        assert_eq!(sz.totals(0), None);
        query_everything(&sz, &empty);
        let file_root = Raw {
            parent: vec![0, 0, 0],
            node: vec![2, 3, 4],
            size: vec![1, 2, 3],
            rsize: vec![NO_RSIZE; 3],
            flags: vec![F, F, D],
        };
        let sz = file_root.sizes();
        assert!(sz.is_lower_bound());
        query_everything(&sz, &file_root);
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u64) -> u64 {
            if n == 0 { 0 } else { self.next() % n }
        }
    }

    #[test]
    fn random_arrays_never_panic() {
        let mut rng = Rng(7);
        for round in 0..2_000 {
            let n = rng.below(40) as usize;
            let len = |rng: &mut Rng| {
                if rng.below(10) == 0 {
                    rng.below(40) as usize
                } else {
                    n
                }
            };
            let node_pool = if round % 2 == 0 { 8 } else { u64::MAX };
            let raw = Raw {
                parent: (0..len(&mut rng))
                    .map(|_| rng.below(n as u64 + 2) as u32)
                    .collect(),
                node: (0..len(&mut rng))
                    .map(|_| match rng.below(4) {
                        0 => u64::MAX - rng.below(3),
                        _ => rng.below(node_pool),
                    })
                    .collect(),
                size: (0..len(&mut rng))
                    .map(|_| match rng.below(4) {
                        0 => NO_SIZE,
                        1 => u64::MAX - 1,
                        _ => rng.below(1000),
                    })
                    .collect(),
                rsize: (0..len(&mut rng)).map(|_| rng.next() as u32).collect(),
                flags: (0..len(&mut rng)).map(|_| rng.next() as u16).collect(),
            };
            let store = |e: u32| e % 3 == 1;
            let sz = build_from(raw.arrays(), &store, &NO_CANCEL).unwrap();
            query_everything(&sz, &raw);
        }
    }

    #[test]
    fn sparse_seen_set_for_large_nodes() {
        let mut tree = Tree::new();
        let a = tree.dir(0, "A", 10);
        tree.file(a, "x.txt", DENSE_NODE_LIMIT + 5, 3);
        tree.file(a, "y.txt", DENSE_NODE_LIMIT + 5, 3);
        tree.file(0, "z.txt", u64::MAX, 4);
        let index = tree.index();
        let sz = sizes(&index);
        assert_eq!(sz.totals(0), t(10, 3, 1));
        assert_eq!(sz.volume_classes().total(), 7);
    }

    /// An index built from an HFS+ volume with a file hard link with two
    /// names and a directory hard link with two names, plus (when `looped`)
    /// a link inside the linked folder to itself.
    fn hfs_index(looped: bool) -> Index {
        use crate::fs::hfsplus::links::{DIR_PRIVATE_FOLDER, FILE_PRIVATE_FOLDER};
        use crate::fs::hfsplus::mount;
        use crate::fs::hfsplus::test_volume::{FileSpec, ROOT, VolumeBuilder, source};

        let mut v = VolumeBuilder::new("Links");
        v.folder(ROOT, "Docs", 16)
            .file(16, "a.txt", 17, FileSpec::regular(10))
            .folder_units(ROOT, &FILE_PRIVATE_FOLDER, 30, 0x5000)
            .file(30, "iNode7", 31, FileSpec::regular(77))
            .file(ROOT, "one.jpg", 50, FileSpec::file_link(7))
            .file(16, "two.jpg", 51, FileSpec::file_link(7))
            .folder_units(ROOT, &DIR_PRIVATE_FOLDER, 40, 0x5000)
            .folder(40, "dir_9", 41)
            .file(41, "deep.txt", 42, FileSpec::regular(5))
            .file(ROOT, "linked dir", 52, FileSpec::dir_link(9))
            .file(16, "again", 53, FileSpec::dir_link(9));
        if looped {
            v.file(41, "loop", 54, FileSpec::dir_link(9));
        }
        let fs = mount(source(v.build())).unwrap();
        crate::search::build(&fs, &NO_CANCEL, &mut |_| {})
            .unwrap()
            .unwrap()
    }

    fn named(index: &Index, name: &str) -> u32 {
        (0..index.len())
            .find(|&i| index.name(i) == name)
            .unwrap_or_else(|| panic!("no entry {name:?}"))
    }

    #[test]
    fn hfs_fixture_with_hard_links() {
        let index = hfs_index(false);
        let sz = sizes(&index);
        let linked = named(&index, "linked dir");
        let again = named(&index, "again");
        let docs = named(&index, "Docs");
        assert_eq!(sz.totals(linked), t(5, 1, 0));
        assert_eq!(sz.totals(again), t(5, 1, 0));
        assert_eq!(sz.totals(docs), t(92, 3, 1));
        // Whatever the scan reports of the private stores, they are not in
        // the root's total.
        assert_eq!(sz.totals(0).unwrap().bytes, 174);
        let v = sz.volume_classes();
        assert_eq!(v.total(), 92);
        assert_eq!(v.of(KindClass::Image), 77);
        assert!(!sz.is_lower_bound());
        // Today's scan: no private store is reported, and the first report
        // of the linked folder (the link at the root) holds its children.
        assert!(sz.stores.is_empty());
        let target = sz.folder_entry(NodeId(index.entry(linked).unwrap().node.0));
        assert_eq!(target, Some(linked));
        assert_eq!(sz.children(linked).count(), 1);
        assert_eq!(sz.children(again).count(), 0);
    }

    #[test]
    fn hfs_fixture_folder_linking_into_itself() {
        let index = hfs_index(true);
        let sz = sizes(&index);
        let linked = named(&index, "linked dir");
        let again = named(&index, "again");
        let looped = named(&index, "loop");
        assert!(sz.is_lower_bound());
        assert_eq!(sz.totals(looped), t(0, 0, 0));
        assert_eq!(sz.totals(linked), t(5, 1, 1));
        assert_eq!(sz.totals(again), t(5, 1, 1));
        assert_eq!(sz.volume_classes().total(), 92);
    }

    /// Valid pre-order trees without directory links (file hard links
    /// included): every folder's totals, children and node lookup equal a
    /// naive walk up the parent chains, and the volume counts each file node
    /// once.
    #[test]
    fn random_valid_trees_match_a_naive_sum() {
        let mut rng = Rng(23);
        for _ in 0..300 {
            let n = 1 + rng.below(200) as usize;
            let mut raw = Raw {
                parent: vec![0],
                node: vec![2],
                size: vec![NO_SIZE],
                rsize: vec![NO_RSIZE],
                flags: vec![D],
            };
            let mut open = vec![0u32];
            let mut next_node = 3u64;
            for i in 1..n {
                let pop = rng.below(open.len() as u64) as usize;
                open.truncate(open.len() - pop);
                raw.parent.push(*open.last().unwrap());
                if rng.below(3) == 0 {
                    raw.flags.push(D);
                    raw.node.push(next_node);
                    next_node += 1;
                    raw.size.push(NO_SIZE);
                    raw.rsize.push(NO_RSIZE);
                    open.push(i as u32);
                } else {
                    raw.flags.push(F);
                    // A third of the files are another name of an earlier node.
                    let node = if rng.below(3) == 0 {
                        3 + rng.below(next_node - 3 + 1)
                    } else {
                        next_node += 1;
                        next_node - 1
                    };
                    raw.node.push(node);
                    raw.size.push(match rng.below(5) {
                        0 => NO_SIZE,
                        _ => rng.below(10_000),
                    });
                    raw.rsize.push(match rng.below(3) {
                        0 => rng.below(500) as u32,
                        _ => NO_RSIZE,
                    });
                }
            }
            let own = |i: usize| {
                let s = if raw.size[i] == NO_SIZE {
                    0
                } else {
                    raw.size[i]
                };
                let r = if raw.rsize[i] == NO_RSIZE {
                    0
                } else {
                    u64::from(raw.rsize[i])
                };
                s + r
            };
            let mut naive = vec![Totals::default(); n];
            let mut volume = 0u64;
            let mut seen = std::collections::HashSet::new();
            for i in 1..n {
                let folder = raw.flags[i] == D;
                if !folder && seen.insert(raw.node[i]) {
                    volume += own(i);
                }
                let mut p = raw.parent[i] as usize;
                loop {
                    if folder {
                        naive[p].folders += 1;
                    } else {
                        naive[p].bytes += own(i);
                        naive[p].files += 1;
                    }
                    if p == 0 {
                        break;
                    }
                    p = raw.parent[p] as usize;
                }
            }

            let sz = raw.sizes();
            assert!(!sz.is_lower_bound());
            assert_eq!(sz.volume_classes().total(), volume);
            for e in 0..n as u32 {
                if raw.flags[e as usize] != D {
                    assert_eq!(sz.totals(e), None);
                    continue;
                }
                assert_eq!(sz.totals(e), Some(naive[e as usize]), "entry {e}");
                let kids: Vec<u32> = (0..n as u32)
                    .filter(|&c| c > 0 && raw.parent[c as usize] == e)
                    .collect();
                assert_eq!(sz.children(e).collect::<Vec<_>>(), kids, "entry {e}");
                assert_eq!(sz.folder_entry(NodeId(raw.node[e as usize])), Some(e));
                assert_eq!(
                    sz.classes_in(raw.arrays(), e).total(),
                    naive[e as usize].bytes
                );
            }
        }
    }

    #[test]
    fn link_work_budget_gives_up_without_hanging() {
        // 150 nested hard-linked folders, each with a link at the root, and
        // 300 links at the bottom: about 45,000 link visits against a budget
        // of about 29,400.
        let mut tree = Tree::new();
        let x = tree.dir(0, "X", 9);
        tree.file(x, "x.txt", 8, 1);
        let mut at = 0;
        for k in 0..150u64 {
            at = tree.dir(at, &format!("C{k}"), 1000 + k);
        }
        let bottom = at;
        for k in 0..300 {
            tree.dir(bottom, &format!("l{k}"), 9);
        }
        for k in 0..150u64 {
            tree.dir(0, &format!("to C{k}"), 1000 + k);
        }
        let index = tree.index();
        let sz = sizes(&index);
        assert!(sz.is_lower_bound());
        assert_eq!(sz.totals(x), t(1, 1, 0));
        // Given up: the links keep their step 4 totals.
        assert_eq!(sz.totals(bottom), t(0, 0, 300));
    }

    // ---- timings ----

    /// The synthetic index with `links` of its files turned into directory
    /// links, each to a folder with children later in the index (so no
    /// cycle).
    fn with_links(base: &Index, links: usize) -> Index {
        let n = base.len() as usize;
        let mut flags = base.flag_words().to_vec();
        let mut node = base.nodes().to_vec();
        let mut size = base.sizes().to_vec();
        let mut rsize = base.rsizes().to_vec();
        let sz = sizes(base);
        let targets: Vec<u32> = (0..base.len())
            .filter(|&e| base.is_folder(e) && sz.children(e).next().is_some())
            .collect();
        let files: Vec<usize> = (1..n * 9 / 10)
            .filter(|&i| !base.is_folder(i as u32))
            .collect();
        let mut rng = Rng(11);
        let mut made = 0;
        let step = (files.len() / links).max(1);
        for &i in files.iter().step_by(step) {
            if made == links {
                break;
            }
            let from = targets.partition_point(|&e| (e as usize) <= i);
            let Some(choices) = targets.len().checked_sub(from).filter(|&c| c > 0) else {
                continue;
            };
            let target = targets[from + rng.below(choices as u64) as usize];
            node[i] = base.nodes()[target as usize];
            flags[i] = IndexFlags::new(
                NodeKind::Directory,
                EntryFlags::EMPTY,
                false,
                KindClass::Folder,
            )
            .bits();
            size[i] = NO_SIZE;
            rsize[i] = NO_RSIZE;
            made += 1;
        }
        assert_eq!(made, links, "not enough files to turn into links");
        Index::from_parts(IndexParts {
            names: std::str::from_utf8(base.names_bytes()).unwrap().into(),
            offsets: base.offsets().into(),
            parent: base.parents().into(),
            node: node.into(),
            size: size.into(),
            modified: base.modified_times().into(),
            rsize: rsize.into(),
            flags: flags.into(),
            identity: None,
            summary: ScanSummary::default(),
        })
    }

    #[allow(clippy::print_stdout)]
    fn time_build(label: &str, index: &Index, budget_ms: f64) {
        let mut times = Vec::new();
        let mut last = None;
        for _ in 0..7 {
            let started = std::time::Instant::now();
            let sz = std::hint::black_box(IndexSizes::build(index, &NO_CANCEL).unwrap());
            times.push(started.elapsed().as_secs_f64() * 1e3);
            last = Some(sz);
        }
        times.sort_by(f64::total_cmp);
        let sz = last.unwrap();
        let folders = (0..index.len()).filter(|&i| index.is_folder(i)).count();
        println!(
            "{label}: IndexSizes::build median {:.1} ms of 7 (min {:.1}, max {:.1}), budget {budget_ms:.0} ms: {}; {} entries, {folders} folders, heap_bytes {} ({:.2} MiB), lower bound {}",
            times[3],
            times[0],
            times[6],
            if times[3] <= budget_ms {
                "met"
            } else {
                "MISSED"
            },
            index.len(),
            sz.heap_bytes(),
            sz.heap_bytes() as f64 / (1024.0 * 1024.0),
            sz.is_lower_bound(),
        );
        let started = std::time::Instant::now();
        let root = sz.classes_of(index, 0);
        println!(
            "{label}: classes_of(root) {:.2} ms over {} entries; root total {:?}",
            started.elapsed().as_secs_f64() * 1e3,
            index.len(),
            sz.totals(0),
        );
        std::hint::black_box(root);
    }

    #[test]
    #[ignore = "timing; run in release with --ignored --nocapture"]
    fn sizes_bench_1m() {
        let index = crate::search::bench::synthetic(1_000_000, 42);
        time_build("sizes_bench_1m", &index, 150.0);
    }

    #[test]
    #[ignore = "timing; run in release with --ignored --nocapture"]
    fn sizes_bench_1m_with_100k_links() {
        let base = crate::search::bench::synthetic(1_000_000, 42);
        let index = with_links(&base, 100_000);
        drop(base);
        time_build("sizes_bench_1m_with_100k_links", &index, 250.0);
    }
}
