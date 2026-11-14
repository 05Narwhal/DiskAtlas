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

//! Building an index from a volume scan.
//!
//! [`build`] runs in five steps, all linear in the number of entries:
//!
//! 1. **Collect** every [`ScanEntry`] into temporary arrays in arrival order.
//!    Children may arrive before their parents.
//! 2. **Resolve parents** through a map from folder node to its first
//!    arrival (for a folder reported twice through hard links, its children
//!    hang from its first report, as the scan contract says).
//! 3. **Children lists** by a counting sort over parents.
//! 4. **Pre-order walk** from the root with an explicit stack. Entries the
//!    walk never reaches (orphans, folder cycles not attached to the root)
//!    are dropped and counted in `summary.skipped`.
//! 5. **Permute** every array into walk order, pass the housekeeping bit
//!    ([`IndexFlags::SYSTEM_ITEM`]) down from each folder to everything in
//!    it, and fold the names in parallel.
//!
//! Each entry's kind class comes from [`classify_entry`] (extension, then the
//! scan's type code) and its own housekeeping bit from [`system_item`], with
//! the name as the scan reported it, whether its parent is the root, and its
//! hidden flag.
//!
//! Siblings keep their arrival order, so the same scan gives the same index.
//!
//! Peak memory is about twice the final index: the arrival-order arrays and
//! the final ones are alive together during step 5 (each temporary array is
//! dropped as soon as its permuted copy exists). For a million entries that
//! is roughly 160 MB at the peak and 80 MB after.
//!
//! Cancellation is checked every 4096 scanned entries and between steps.

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{CorruptKind, Corruption, Error, Location, Result};
use crate::fs::{EntryFlags, FileSystem, NodeKind, ScanEntry, ScanSummary};
use crate::mac::conventions::system_item;
use crate::search::index::{
    Index, IndexFlags, IndexParts, MAX_ENTRIES, MAX_NAME_BYTES, MAX_RSIZE, NO_RSIZE, NO_SIZE,
    NO_TIME, NUL_REPLACEMENT,
};
use crate::search::kind::classify_entry;

/// How far a build has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildProgress {
    /// Entries the scan has reported so far.
    pub entries: u64,
}

/// Progress is reported when the scanned count is a multiple of this, and
/// once more when the scan ends.
pub const PROGRESS_EVERY: u64 = 16_384;

/// `cancel` is checked when the scanned count is a multiple of 4096.
const CANCEL_MASK: u64 = 4095;
const PROGRESS_MASK: u64 = PROGRESS_EVERY - 1;

/// Parent slot of an entry whose parent folder was never reported.
const ORPHAN: u32 = u32::MAX;

/// Largest stored size: `NO_SIZE` is taken, so a reported `u64::MAX` is
/// stored one lower.
const MAX_SIZE: u64 = u64::MAX - 1;
/// Earliest stored time: `NO_TIME` is taken, so a reported `i64::MIN` is
/// stored one later.
const MIN_TIME: i64 = i64::MIN + 1;

/// What the builder accepts. Fixed to the index's own limits outside tests.
#[derive(Debug, Clone, Copy)]
struct Limits {
    /// Entries, the root included.
    entries: u32,
    /// Bytes of `names`, separators and the root's name included.
    name_bytes: u64,
}

const INDEX_LIMITS: Limits = Limits {
    entries: MAX_ENTRIES,
    name_bytes: MAX_NAME_BYTES as u64,
};

/// Builds the index of `fs` from [`FileSystem::scan`].
///
/// `progress` is called about every [`PROGRESS_EVERY`] scanned entries and
/// once when the scan ends. Returns `Ok(None)` when `cancel` is set before
/// the build finishes, and the scan's error when it fails.
///
/// Entries past [`MAX_ENTRIES`] or [`MAX_NAME_BYTES`], entries the walk
/// from the root never reaches, and the scan's own skipped structures are
/// all counted in `summary().skipped`; `summary().entries` is the entries
/// kept, the root not counted.
///
/// Blocking and CPU-heavy: call it on a background thread.
pub fn build(
    fs: &dyn FileSystem,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(BuildProgress),
) -> Result<Option<Index>> {
    build_with_limits(fs, cancel, progress, INDEX_LIMITS)
}

/// The scan's entries in arrival order.
#[derive(Default)]
struct Arrivals {
    /// Every name back to back, no separators, NUL bytes replaced.
    names: String,
    /// End of each name in `names`.
    name_end: Vec<u32>,
    node: Vec<u64>,
    parent_node: Vec<u64>,
    size: Vec<u64>,
    modified: Vec<i64>,
    rsize: Vec<u32>,
    flags: Vec<u16>,
}

impl Arrivals {
    fn len(&self) -> usize {
        self.node.len()
    }

    /// Appends `e`, reported by a scan whose root is `root`; false (and
    /// nothing appended) when the names would no longer fit a `u32` offset.
    fn push(&mut self, e: &ScanEntry<'_>, root: u64) -> bool {
        let Some(end) = self
            .names
            .len()
            .checked_add(e.name.len())
            .and_then(|end| u32::try_from(end).ok())
        else {
            return false;
        };
        push_name(&mut self.names, e.name);
        self.name_end.push(end);
        self.node.push(e.node.0);
        self.parent_node.push(e.parent.0);
        self.size.push(e.size.map_or(NO_SIZE, |s| s.min(MAX_SIZE)));
        self.modified
            .push(e.modified.map_or(NO_TIME, |t| t.unix_nanos().max(MIN_TIME)));
        self.rsize.push(e.resource_size.map_or(NO_RSIZE, |r| {
            u32::try_from(r).map_or(MAX_RSIZE, |r| r.min(MAX_RSIZE))
        }));
        self.flags.push(entry_flags(e, root).bits());
        true
    }

    /// Name of arrival `a`.
    fn name(&self, a: u32) -> &str {
        let a = a as usize;
        let start = match a.checked_sub(1) {
            Some(prev) => self.name_end.get(prev).map_or(0, |&e| e as usize),
            None => 0,
        };
        let end = self.name_end.get(a).map_or(start, |&e| e as usize);
        self.names.get(start..end).unwrap_or("")
    }
}

/// The index flags of a scanned entry, bit 6 for a housekeeping item
/// itself (not yet for what is inside one). `root` is the scan's root node:
/// an entry listed in it is a direct child of the root.
fn entry_flags(e: &ScanEntry<'_>, root: u64) -> IndexFlags {
    let fork = e.resource_size.is_some_and(|r| r > 0);
    let class = classify_entry(e.name, e.kind == NodeKind::Directory, e.finder_type);
    let flags = IndexFlags::new(e.kind, e.flags, fork, class);
    let hidden = e.flags.contains(EntryFlags::HIDDEN);
    if system_item(e.name, e.kind, e.parent.0 == root, hidden).is_some() {
        flags.with_system_item()
    } else {
        flags
    }
}

/// Passes bit 6 down: an entry inside a housekeeping item gets it too.
/// One forward pass, since `parent[i] < i` (pre-order); a parent that is not
/// before its child (impossible from [`preorder`]) passes nothing.
fn pass_down_system_items(flags: &mut [u16], parent: &[u32]) {
    for i in 1..flags.len() {
        let Some(&p) = parent.get(i) else {
            break;
        };
        let p = p as usize;
        if p >= i {
            continue;
        }
        let inherited = flags
            .get(p)
            .is_some_and(|&f| f & IndexFlags::SYSTEM_ITEM != 0);
        if inherited && let Some(f) = flags.get_mut(i) {
            *f |= IndexFlags::SYSTEM_ITEM;
        }
    }
}

/// Appends `name` with every NUL byte stored as [`NUL_REPLACEMENT`]: same
/// length, and the 0 separator stays unique.
fn push_name(out: &mut String, name: &str) {
    if memchr::memchr(0, name.as_bytes()).is_none() {
        out.push_str(name);
    } else {
        out.extend(name.chars().map(|c| {
            if c == '\0' {
                char::from(NUL_REPLACEMENT)
            } else {
                c
            }
        }));
    }
}

fn too_many_name_bytes() -> Error {
    Corruption::new(
        "search index",
        Location::Unspecified,
        CorruptKind::LimitExceeded {
            what: "bytes of names",
            limit: u64::from(MAX_NAME_BYTES),
        },
    )
    .into()
}

fn build_with_limits(
    fs: &dyn FileSystem,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(BuildProgress),
    limits: Limits,
) -> Result<Option<Index>> {
    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }
    let root = fs.root().0;
    let mut root_name = String::new();
    push_name(&mut root_name, &fs.info().name);
    let len_u64 = |s: &str| u64::try_from(s.len()).unwrap_or(u64::MAX);
    if len_u64(&root_name).saturating_add(1) > limits.name_bytes {
        root_name.clear();
    }

    // 1. Collect.
    let mut arrivals = Arrivals::default();
    let max_arrivals = usize::try_from(limits.entries.saturating_sub(1)).unwrap_or(usize::MAX);
    let mut bytes = len_u64(&root_name).saturating_add(1);
    let mut seen: u64 = 0;
    let mut full = false;
    let mut dropped: u64 = 0;
    let mut cancelled = false;
    let scan = fs.scan(&mut |e| {
        seen = seen.saturating_add(1);
        if seen & CANCEL_MASK == 0 {
            if seen & PROGRESS_MASK == 0 {
                progress(BuildProgress { entries: seen });
            }
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                return ControlFlow::Break(());
            }
        }
        if !full {
            let need = bytes.saturating_add(len_u64(e.name)).saturating_add(1);
            if arrivals.len() >= max_arrivals
                || need > limits.name_bytes
                || !arrivals.push(&e, root)
            {
                full = true;
            } else {
                bytes = need;
            }
        }
        if full {
            dropped = dropped.saturating_add(1);
        }
        ControlFlow::Continue(())
    })?;
    progress(BuildProgress { entries: seen });
    if cancelled || cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }

    // 2. Resolve parents into slots: 0 is the root, a + 1 is arrival a.
    let parent_slot = resolve_parents(&arrivals, root);
    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }

    // 3. Children lists.
    let slots = arrivals.len().saturating_add(1);
    let (child_end, children) = child_lists(&parent_slot, slots);
    drop(parent_slot);
    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }

    // 4. Pre-order walk.
    let (order, parent) = preorder(&arrivals.flags, &child_end, &children);
    drop((child_end, children));
    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }

    // 5. Permute.
    let arrived = arrivals.len();
    let name_bytes = order
        .iter()
        .fold(root_name.len().saturating_add(1), |sum, &a| {
            sum.saturating_add(arrivals.name(a).len()).saturating_add(1)
        });
    let mut names = String::with_capacity(name_bytes);
    let mut offsets: Vec<u32> = Vec::with_capacity(order.len().saturating_add(2));
    offsets.push(0);
    for name in std::iter::once(root_name.as_str()).chain(order.iter().map(|&a| arrivals.name(a))) {
        names.push_str(name);
        names.push('\0');
        offsets.push(u32::try_from(names.len()).map_err(|_| too_many_name_bytes())?);
    }
    let Arrivals {
        names: arrival_names,
        name_end,
        node,
        parent_node,
        size,
        modified,
        rsize,
        flags,
    } = arrivals;
    drop((arrival_names, name_end, parent_node));
    let root_flags = IndexFlags::new(
        NodeKind::Directory,
        EntryFlags::EMPTY,
        false,
        classify_entry(&root_name, true, None),
    );
    let node = permuted(root, &order, node, 0);
    let size = permuted(NO_SIZE, &order, size, NO_SIZE);
    let modified = permuted(NO_TIME, &order, modified, NO_TIME);
    let rsize = permuted(NO_RSIZE, &order, rsize, NO_RSIZE);
    let mut flags = permuted(root_flags.bits(), &order, flags, root_flags.bits());
    pass_down_system_items(&mut flags, &parent);
    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }

    let to_u64 = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
    let kept = to_u64(order.len());
    let unreached = to_u64(arrived.saturating_sub(order.len()));
    Ok(Some(Index::from_parts(IndexParts {
        names: names.into_boxed_str(),
        offsets: offsets.into_boxed_slice(),
        parent: parent.into_boxed_slice(),
        node,
        size,
        modified,
        rsize,
        flags,
        identity: fs.identity(),
        summary: ScanSummary {
            entries: kept,
            skipped: scan
                .skipped
                .saturating_add(dropped)
                .saturating_add(unreached),
        },
    })))
}

/// Parent slot of every arrival: 0 for the root, `a + 1` for arrival `a`,
/// [`ORPHAN`] when the parent folder was never reported. A folder node
/// reported more than once resolves to its first arrival.
fn resolve_parents(arrivals: &Arrivals, root: u64) -> Vec<u32> {
    let folder_count = arrivals
        .flags
        .iter()
        .filter(|&&f| IndexFlags::from_bits(f).is_folder())
        .count();
    let mut folders: HashMap<u64, u32> = HashMap::with_capacity(folder_count);
    for (a, (&node, &f)) in arrivals.node.iter().zip(&arrivals.flags).enumerate() {
        if IndexFlags::from_bits(f).is_folder()
            && let Ok(a) = u32::try_from(a)
        {
            folders.entry(node).or_insert(a);
        }
    }
    arrivals
        .parent_node
        .iter()
        .map(|&p| {
            if p == root {
                0
            } else {
                folders
                    .get(&p)
                    .and_then(|&a| a.checked_add(1))
                    .unwrap_or(ORPHAN)
            }
        })
        .collect()
}

/// Children of every slot by counting sort, stable in arrival order.
///
/// Returns `(end, children)`: the children of slot `s` are
/// `children[end[s - 1] .. end[s]]` (from 0 for the root).
fn child_lists(parent_slot: &[u32], slots: usize) -> (Vec<u32>, Vec<u32>) {
    // Count into s + 1, so the running sum leaves each slot's start at s.
    let mut start = vec![0u32; slots.saturating_add(1)];
    for &s in parent_slot {
        if s == ORPHAN {
            continue;
        }
        if let Some(count) = start.get_mut((s as usize).saturating_add(1)) {
            *count = count.saturating_add(1);
        }
    }
    let mut sum = 0u32;
    for c in &mut start {
        sum = sum.saturating_add(*c);
        *c = sum;
    }
    let attached = start.last().map_or(0, |&n| n as usize);
    let mut children = vec![0u32; attached];
    // Filling moves each slot's start to its end, which is the next slot's
    // start: one array serves as both.
    for (a, &s) in parent_slot.iter().enumerate() {
        if s == ORPHAN {
            continue;
        }
        let (Ok(a), Some(pos)) = (u32::try_from(a), start.get_mut(s as usize)) else {
            continue;
        };
        if let Some(slot) = children.get_mut(*pos as usize) {
            *slot = a;
            *pos = pos.saturating_add(1);
        }
    }
    start.truncate(slots);
    (start, children)
}

/// The children of `slot`, from [`child_lists`].
fn children_of<'a>(end: &[u32], children: &'a [u32], slot: usize) -> &'a [u32] {
    let begin = match slot.checked_sub(1) {
        Some(prev) => end.get(prev).map_or(0, |&e| e as usize),
        None => 0,
    };
    let stop = end.get(slot).map_or(begin, |&e| e as usize);
    children.get(begin..stop).unwrap_or(&[])
}

/// Walks the tree from the root in pre-order, siblings in arrival order.
///
/// Returns `(order, parent)`: `order[k]` is the arrival that becomes entry
/// `k + 1`; `parent[e]` is entry `e`'s parent entry, with `parent[0] == 0`.
/// Each arrival sits in exactly one children list and a list is pushed only
/// when its folder is first visited, so the stack sees each arrival at most
/// once; the `visited` bits guard that anyway.
fn preorder(flags: &[u16], end: &[u32], children: &[u32]) -> (Vec<u32>, Vec<u32>) {
    let n = flags.len();
    let mut order: Vec<u32> = Vec::with_capacity(children.len());
    let mut parent: Vec<u32> = Vec::with_capacity(children.len().saturating_add(1));
    parent.push(0);
    let mut visited = vec![0u64; n.div_ceil(64)];
    let mut stack: Vec<(u32, u32)> = children_of(end, children, 0)
        .iter()
        .rev()
        .map(|&a| (a, 0))
        .collect();
    while let Some((a, p)) = stack.pop() {
        let k = a as usize;
        let Some(word) = visited.get_mut(k / 64) else {
            continue;
        };
        let bit = 1u64 << (k % 64);
        if *word & bit != 0 {
            continue;
        }
        *word |= bit;
        let Ok(entry) = u32::try_from(parent.len()) else {
            break;
        };
        order.push(a);
        parent.push(p);
        let is_folder = flags
            .get(k)
            .is_some_and(|&f| IndexFlags::from_bits(f).is_folder());
        if is_folder {
            let slot = k.saturating_add(1);
            stack.extend(
                children_of(end, children, slot)
                    .iter()
                    .rev()
                    .map(|&c| (c, entry)),
            );
        }
    }
    (order, parent)
}

/// `root` followed by `from[a]` for each arrival `a` of `order`.
fn permuted<T: Copy>(root: T, order: &[u32], from: Vec<T>, missing: T) -> Box<[T]> {
    std::iter::once(root)
        .chain(
            order
                .iter()
                .map(|&a| from.get(a as usize).copied().unwrap_or(missing)),
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{
        DirStream, EntryFlags, Metadata, NodeId, Timestamp, VolumeIdentity, VolumeInfo,
    };
    use crate::search::fold::fold_into;
    use crate::search::index::{MAX_RSIZE, NO_RSIZE};
    use crate::search::kind::KindClass;
    use NodeKind::{Directory as D, File as F};
    use std::sync::atomic::AtomicU64;

    /// One entry the double reports; the name is a range of `ListFs::names`.
    #[derive(Debug, Clone)]
    struct Rec {
        node: u64,
        parent: u64,
        name: std::ops::Range<usize>,
        kind: NodeKind,
        size: Option<u64>,
        rsize: Option<u64>,
        modified: Option<Timestamp>,
        flags: EntryFlags,
        finder_type: Option<[u8; 4]>,
    }

    /// A `FileSystem` whose `scan` reports a fixed list of entries in list
    /// order. `lookup` answers from the same list (first match).
    struct ListFs {
        volume: String,
        root: u64,
        names: String,
        recs: Vec<Rec>,
        skipped: u64,
        fail: bool,
        identity: Option<VolumeIdentity>,
        lookup: HashMap<(u64, String), u64>,
        /// Entries passed to a visitor, over every scan.
        visited: AtomicU64,
    }

    impl ListFs {
        fn new(volume: &str) -> Self {
            Self {
                volume: volume.to_owned(),
                root: 1,
                names: String::new(),
                recs: Vec::new(),
                skipped: 0,
                fail: false,
                identity: None,
                lookup: HashMap::new(),
                visited: AtomicU64::new(0),
            }
        }

        fn push(&mut self, node: u64, parent: u64, name: &str, kind: NodeKind) -> &mut Rec {
            let start = self.names.len();
            self.names.push_str(name);
            self.recs.push(Rec {
                node,
                parent,
                name: start..self.names.len(),
                kind,
                size: (kind == F).then_some(1),
                rsize: None,
                modified: None,
                flags: EntryFlags::EMPTY,
                finder_type: None,
            });
            self.recs.last_mut().unwrap()
        }

        fn name_of(&self, r: &Rec) -> &str {
            &self.names[r.name.clone()]
        }

        fn scan_entry<'a>(&'a self, r: &Rec) -> ScanEntry<'a> {
            ScanEntry {
                node: NodeId(r.node),
                parent: NodeId(r.parent),
                name: self.name_of(r),
                kind: r.kind,
                size: r.size,
                resource_size: r.rsize,
                modified: r.modified,
                flags: r.flags,
                finder_type: r.finder_type,
            }
        }

        fn with_lookup(mut self) -> Self {
            let mut map = HashMap::new();
            for r in &self.recs {
                map.entry((r.parent, self.name_of(r).to_owned()))
                    .or_insert(r.node);
            }
            self.lookup = map;
            self
        }
    }

    impl FileSystem for ListFs {
        fn info(&self) -> VolumeInfo {
            VolumeInfo::new(self.volume.clone())
        }
        fn root(&self) -> NodeId {
            NodeId(self.root)
        }
        fn metadata(&self, node: NodeId) -> Result<Metadata> {
            Err(Error::NotFound { node: node.0 })
        }
        fn lookup(&self, dir: NodeId, name: &str) -> Result<Option<NodeId>> {
            Ok(self
                .lookup
                .get(&(dir.0, name.to_owned()))
                .map(|&n| NodeId(n)))
        }
        fn read_dir(&self, dir: NodeId) -> Result<Box<dyn DirStream>> {
            Err(Error::NotADirectory { node: dir.0 })
        }
        fn scan(
            &self,
            visit: &mut dyn FnMut(ScanEntry<'_>) -> ControlFlow<()>,
        ) -> Result<ScanSummary> {
            if self.fail {
                return Err(Error::unsupported("scan failed"));
            }
            let mut summary = ScanSummary {
                entries: 0,
                skipped: self.skipped,
            };
            for r in &self.recs {
                summary.entries += 1;
                self.visited.fetch_add(1, Ordering::Relaxed);
                if visit(self.scan_entry(r)).is_break() {
                    break;
                }
            }
            Ok(summary)
        }
        fn identity(&self) -> Option<VolumeIdentity> {
            self.identity
        }
    }

    fn build_all(fs: &ListFs) -> Index {
        build(fs, &AtomicBool::new(false), &mut |_| {})
            .unwrap()
            .unwrap()
    }

    fn build_limited(fs: &ListFs, entries: u32, name_bytes: u64) -> Index {
        let limits = Limits {
            entries,
            name_bytes,
        };
        build_with_limits(fs, &AtomicBool::new(false), &mut |_| {}, limits)
            .unwrap()
            .unwrap()
    }

    /// `(name, parent entry)` of every entry in index order.
    fn shape(index: &Index) -> Vec<(String, u32)> {
        (0..index.len())
            .map(|i| (index.name(i).to_owned(), index.parent(i).unwrap()))
            .collect()
    }

    fn owned(v: &[(&str, u32)]) -> Vec<(String, u32)> {
        v.iter().map(|&(n, p)| (n.to_owned(), p)).collect()
    }

    #[test]
    fn children_before_parents_and_stable_siblings() {
        let mut fs = ListFs::new("Vol");
        fs.push(20, 2, "c1.txt", F);
        fs.push(21, 2, "c2", D);
        fs.push(2, 1, "docs", D);
        fs.push(30, 21, "deep.rs", F);
        fs.push(3, 1, "top.txt", F);
        let index = build_all(&fs);
        assert_eq!(
            shape(&index),
            owned(&[
                ("Vol", 0),
                ("docs", 0),
                ("c1.txt", 1),
                ("c2", 1),
                ("deep.rs", 3),
                ("top.txt", 0)
            ])
        );
        assert_eq!(index.entry(0).unwrap().node, NodeId(1));
        assert!(index.is_folder(0));
        assert_eq!(index.subtree(1), 1..5);
        assert_eq!(
            index.summary(),
            ScanSummary {
                entries: 5,
                skipped: 0
            }
        );
    }

    #[test]
    fn duplicated_folder_children_hang_from_first_report() {
        let mut fs = ListFs::new("Vol");
        fs.push(2, 1, "first", D);
        fs.push(5, 1, "other", D);
        fs.push(2, 5, "second", D);
        fs.push(9, 2, "child", F);
        let index = build_all(&fs);
        assert_eq!(
            shape(&index),
            owned(&[
                ("Vol", 0),
                ("first", 0),
                ("child", 1),
                ("other", 0),
                ("second", 3)
            ])
        );
        // The second report is a folder with nothing inside.
        assert!(index.is_folder(4));
        assert_eq!(index.subtree(4), 4..5);
        assert_eq!(index.find_node(NodeId(2)), Some(1));
    }

    #[test]
    fn duplicated_folder_first_arrival_wins_wherever_it_sits() {
        // The link arrives first, so the child follows it, not the original
        // reported afterwards.
        let mut fs = ListFs::new("Vol");
        fs.push(9, 2, "child", F);
        fs.push(2, 5, "link", D);
        fs.push(5, 1, "other", D);
        fs.push(2, 1, "original", D);
        let index = build_all(&fs);
        assert_eq!(
            shape(&index),
            owned(&[
                ("Vol", 0),
                ("other", 0),
                ("link", 1),
                ("child", 2),
                ("original", 0)
            ])
        );
    }

    #[test]
    fn orphans_and_cycles_are_dropped_and_counted() {
        let mut fs = ListFs::new("Vol");
        fs.skipped = 4;
        fs.push(2, 1, "ok", F);
        fs.push(7, 99, "orphan", D);
        fs.push(8, 7, "orphan child", F);
        fs.push(40, 41, "x", D);
        fs.push(41, 40, "y", D);
        fs.push(42, 40, "in cycle", F);
        fs.push(50, 50, "own parent", D);
        fs.push(51, 50, "under own parent", F);
        // A file is never a parent.
        fs.push(60, 2, "under a file", F);
        let index = build_all(&fs);
        assert_eq!(shape(&index), owned(&[("Vol", 0), ("ok", 0)]));
        assert_eq!(
            index.summary(),
            ScanSummary {
                entries: 1,
                skipped: 4 + 8
            }
        );
    }

    #[test]
    fn root_reported_as_a_child_is_a_leaf() {
        // A folder entry whose node is the root: entries listed in the root
        // stay with the root, so it is empty and nothing loops.
        let mut fs = ListFs::new("Vol");
        fs.push(2, 1, "a", D);
        fs.push(1, 2, "up", D);
        fs.push(3, 1, "f", F);
        let index = build_all(&fs);
        assert_eq!(
            shape(&index),
            owned(&[("Vol", 0), ("a", 0), ("up", 1), ("f", 0)])
        );
        assert_eq!(index.subtree(2), 2..3);
    }

    #[test]
    fn nul_in_a_name_is_stored_as_substitute() {
        let mut fs = ListFs::new("V\0l");
        fs.push(2, 1, "a\0b", F);
        fs.push(3, 1, "\0", F);
        let index = build_all(&fs);
        assert_eq!(index.name(0), "V\u{1A}l");
        assert_eq!(index.name(1), "a\u{1A}b");
        assert_eq!(index.name(2), "\u{1A}");
        assert_eq!(index.entry(1).unwrap().name, "a\u{1A}b");
        assert_eq!(index.names_bytes(), b"V\x1Al\0a\x1Ab\0\x1A\0");
        assert_eq!(index.verify(), Ok(()));
    }

    #[test]
    fn empty_volume_is_the_root_alone() {
        let mut fs = ListFs::new("Empty");
        fs.root = 77;
        fs.skipped = 2;
        let id = VolumeIdentity {
            id: [1; 16],
            change: [2; 16],
        };
        fs.identity = Some(id);
        let index = build_all(&fs);
        assert_eq!(index.len(), 1);
        assert_eq!(index.name(0), "Empty");
        assert_eq!(index.parent(0), Some(0));
        assert_eq!(index.subtree(0), 0..1);
        assert_eq!(index.find_node(NodeId(77)), Some(0));
        let root = index.entry(0).unwrap();
        assert_eq!(root.kind, NodeKind::Directory);
        assert_eq!(root.size, None);
        assert_eq!(root.resource_size, None);
        assert_eq!(root.modified, None);
        assert_eq!(index.crumbs(0), vec![(NodeId(77), "Empty".to_owned())]);
        assert_eq!(index.identity(), Some(id));
        assert_eq!(
            index.summary(),
            ScanSummary {
                entries: 0,
                skipped: 2
            }
        );
    }

    #[test]
    fn entry_limit_drops_the_rest() {
        let mut fs = ListFs::new("V");
        for n in 2..7 {
            fs.push(n, 1, &format!("f{n}"), F);
        }
        let index = build_limited(&fs, 3, u64::MAX);
        assert_eq!(shape(&index), owned(&[("V", 0), ("f2", 0), ("f3", 0)]));
        assert_eq!(
            index.summary(),
            ScanSummary {
                entries: 2,
                skipped: 3
            }
        );
        // The real limits leave room for the sentinels.
        assert_eq!(INDEX_LIMITS.entries, u32::MAX - 1);
        assert_eq!(INDEX_LIMITS.name_bytes, u64::from(u32::MAX));
    }

    #[test]
    fn name_byte_limit_stops_accepting() {
        let mut fs = ListFs::new("v");
        fs.push(2, 1, "aa", F);
        fs.push(3, 1, "bb", F);
        fs.push(4, 1, "c", F); // would fit, but the builder has stopped
        // "v\0" + "aa\0" = 5; "bb\0" needs 8 > 7.
        let index = build_limited(&fs, u32::MAX, 7);
        assert_eq!(shape(&index), owned(&[("v", 0), ("aa", 0)]));
        assert_eq!(index.summary().skipped, 2);
        // Exactly at the limit is accepted.
        let index = build_limited(&fs, u32::MAX, 8);
        assert_eq!(index.len(), 3);
        assert_eq!(index.names_bytes().len(), 8);
        // A root name that alone exceeds the limit is stored empty.
        let fs = ListFs::new("long volume name");
        let index = build_limited(&fs, u32::MAX, 4);
        assert_eq!(index.name(0), "");
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn scan_error_is_the_build_error() {
        let mut fs = ListFs::new("V");
        fs.fail = true;
        assert!(build(&fs, &AtomicBool::new(false), &mut |_| {}).is_err());
    }

    #[test]
    fn values_and_flags() {
        let mut fs = ListFs::new("V");
        let t = Timestamp::from_unix_nanos(1_700_000_000_000_000_000);
        fs.push(2, 1, "none.txt", F).size = None;
        {
            let r = fs.push(3, 1, "zero fork.pdf", F);
            r.rsize = Some(0);
            r.modified = Some(t);
        }
        {
            let r = fs.push(4, 1, "small fork", F);
            r.rsize = Some(5);
            r.flags = EntryFlags::COMPRESSED;
        }
        {
            let r = fs.push(5, 1, "huge fork", F);
            r.rsize = Some(u64::MAX);
            r.size = Some(u64::MAX);
            r.modified = Some(Timestamp::from_unix_nanos(i64::MIN));
        }
        fs.push(6, 1, "fork of u32 max", F).rsize = Some(u64::from(u32::MAX));
        {
            let r = fs.push(7, 1, "link", NodeKind::Symlink);
            r.flags = EntryFlags::LINKS_TO_DIRECTORY.with(EntryFlags::HIDDEN);
            r.size = None;
        }
        fs.push(8, 1, "pipe", NodeKind::Other).size = None;
        fs.push(9, 1, "Tool.app", D);
        let index = build_all(&fs);
        let by_name = |name: &str| (0..index.len()).find(|&i| index.name(i) == name).unwrap();

        let e = index.entry(by_name("none.txt")).unwrap();
        assert_eq!((e.size, e.resource_size, e.modified), (None, None, None));
        assert_eq!(index.kind_class(by_name("none.txt")), KindClass::Document);

        let i = by_name("zero fork.pdf");
        let e = index.entry(i).unwrap();
        assert_eq!(e.resource_size, Some(0));
        assert_eq!(e.modified, Some(t));
        assert!(!index.flags(i).unwrap().has_resource_fork());

        let i = by_name("small fork");
        assert_eq!(index.entry(i).unwrap().resource_size, Some(5));
        assert!(index.flags(i).unwrap().has_resource_fork());
        assert_eq!(index.entry(i).unwrap().flags, EntryFlags::COMPRESSED);

        let i = by_name("huge fork");
        let e = index.entry(i).unwrap();
        assert_eq!(e.resource_size, Some(u64::from(MAX_RSIZE)));
        assert_eq!(e.size, Some(u64::MAX - 1));
        assert_eq!(e.modified, Some(Timestamp::from_unix_nanos(i64::MIN + 1)));
        assert!(index.flags(i).unwrap().has_resource_fork());

        // u32::MAX is NO_RSIZE in the array, so it must saturate, not vanish.
        let i = by_name("fork of u32 max");
        assert_eq!(index.rsizes()[i as usize], MAX_RSIZE);
        assert_ne!(index.rsizes()[i as usize], NO_RSIZE);
        assert_eq!(
            index.entry(i).unwrap().resource_size,
            Some(u64::from(MAX_RSIZE))
        );

        let i = by_name("link");
        let e = index.entry(i).unwrap();
        assert_eq!(e.kind, NodeKind::Symlink);
        assert!(e.is_browsable());
        assert!(index.is_hidden(i));
        assert!(!index.is_folder(i));
        assert_eq!(index.subtree(i), i..i + 1);

        assert_eq!(index.entry(by_name("pipe")).unwrap().kind, NodeKind::Other);
        let app = by_name("Tool.app");
        assert!(index.is_folder(app));
        assert_eq!(index.kind_class(app), KindClass::Application);
        assert_eq!(index.kind_class(0), KindClass::Folder);
    }

    /// Entry number of the entry named `name` whose parent is `parent`.
    fn entry_named(index: &Index, parent: u32, name: &str) -> u32 {
        (1..index.len())
            .find(|&i| index.name(i) == name && index.parent(i) == Some(parent))
            .unwrap_or_else(|| panic!("{name:?} under {parent}"))
    }

    fn system(index: &Index, i: u32) -> bool {
        index.flags(i).unwrap().is_system_item()
    }

    #[test]
    fn housekeeping_bit_is_passed_down() {
        let mut fs = ListFs::new("Vol");
        // Children before their parents, as a catalog may report them.
        fs.push(5, 4, "deep.txt", F);
        fs.push(4, 3, "a", D);
        fs.push(3, 2, "Store-V2", D);
        fs.push(2, 1, ".Spotlight-V100", D);
        fs.push(6, 1, "Docs", D);
        fs.push(7, 6, "x.txt", F);
        fs.push(11, 6, ".DS_Store", F);
        fs.push(8, 1, "Sub", D);
        // Root-only names count only at the root.
        fs.push(9, 8, ".Spotlight-V100", D);
        fs.push(10, 9, "inside.txt", F);
        // Classic names count only when hidden.
        fs.push(12, 1, "Trash", D).flags = EntryFlags::HIDDEN;
        fs.push(15, 12, "old letter", F);
        fs.push(16, 8, "Trash", D).flags = EntryFlags::HIDDEN;
        fs.push(17, 1, "Network Trash Folder", D);
        // The private store, as the scan reports its name (four NULs).
        fs.push(13, 1, "\0\0\0\0HFS+ Private Data", D);
        fs.push(14, 13, "iNode7", F);
        let index = build_all(&fs);
        assert_eq!(index.verify(), Ok(()));

        let spotlight = entry_named(&index, 0, ".Spotlight-V100");
        let store = entry_named(&index, spotlight, "Store-V2");
        let a = entry_named(&index, store, "a");
        let deep = entry_named(&index, a, "deep.txt");
        assert!(system(&index, spotlight));
        assert!(system(&index, store));
        assert!(system(&index, a));
        assert!(system(&index, deep), "three levels inside");

        let docs = entry_named(&index, 0, "Docs");
        assert!(!system(&index, docs), "the sibling");
        assert!(!system(&index, entry_named(&index, docs, "x.txt")));
        assert!(system(&index, entry_named(&index, docs, ".DS_Store")));

        let sub = entry_named(&index, 0, "Sub");
        assert!(!system(&index, sub));
        let nested = entry_named(&index, sub, ".Spotlight-V100");
        assert!(!system(&index, nested), "not at the root");
        assert!(!system(&index, entry_named(&index, nested, "inside.txt")));
        assert!(!system(&index, entry_named(&index, sub, "Trash")));

        let trash = entry_named(&index, 0, "Trash");
        assert!(system(&index, trash));
        assert!(system(&index, entry_named(&index, trash, "old letter")));
        assert!(!system(
            &index,
            entry_named(&index, 0, "Network Trash Folder")
        ));

        let private = entry_named(&index, 0, "\u{1A}\u{1A}\u{1A}\u{1A}HFS+ Private Data");
        assert!(system(&index, private));
        assert!(system(&index, entry_named(&index, private, "iNode7")));

        assert!(!system(&index, 0), "the root");
        // Everything in a system subtree has the bit, and nothing else does
        // but the items themselves.
        for i in 1..index.len() {
            let p = index.parent(i).unwrap();
            if system(&index, p) {
                assert!(system(&index, i), "{}", index.name(i));
            }
        }
        // The bit stays out of the listing flags.
        assert_eq!(index.entry(deep).unwrap().flags, EntryFlags::EMPTY);
    }

    #[test]
    fn pass_down_tolerates_bad_parents() {
        let sys = IndexFlags::SYSTEM_ITEM | IndexFlags::KIND_FOLDER;
        // Parent after the child, parent equal to the child, short parents.
        let mut flags = [sys, 0, 0, sys, 0];
        pass_down_system_items(&mut flags, &[0, 3, 2, 0]);
        assert_eq!(flags, [sys, 0, 0, sys | IndexFlags::SYSTEM_ITEM, 0]);
        let mut flags = [sys, 0];
        pass_down_system_items(&mut flags, &[0, u32::MAX]);
        assert_eq!(flags, [sys, 0]);
        pass_down_system_items(&mut [], &[]);
    }

    #[test]
    fn type_code_classifies_names_without_extension() {
        let mut fs = ListFs::new("Classic");
        fs.push(2, 1, "ReadMe", F).finder_type = Some(*b"TEXT");
        fs.push(3, 1, "SimpleText", F).finder_type = Some(*b"APPL");
        fs.push(4, 1, "photo.jpg", F).finder_type = Some(*b"TEXT");
        fs.push(5, 1, "mystery", F).finder_type = Some(*b"BINA");
        fs.push(6, 1, "Folder", D).finder_type = Some(*b"TEXT");
        fs.push(7, 1, "plain", F);
        let index = build_all(&fs);
        let class = |name: &str| index.kind_class(entry_named(&index, 0, name));
        assert_eq!(class("ReadMe"), KindClass::Document);
        assert_eq!(class("SimpleText"), KindClass::Application);
        assert_eq!(class("photo.jpg"), KindClass::Image);
        assert_eq!(class("mystery"), KindClass::Other);
        assert_eq!(class("Folder"), KindClass::Folder);
        assert_eq!(class("plain"), KindClass::Other);
    }

    #[test]
    fn package_and_label_reach_the_index() {
        let mut fs = ListFs::new("V");
        fs.push(2, 1, "Thing", D).flags = EntryFlags::PACKAGE.with_label(2);
        fs.push(3, 1, "Tool", F).flags = EntryFlags::PACKAGE.with_label(7);
        fs.push(4, 1, "Alias", F).flags = EntryFlags::ALIAS.with_label(1);
        fs.push(5, 2, "inside", F);
        let index = build_all(&fs);
        let thing = entry_named(&index, 0, "Thing");
        let f = index.flags(thing).unwrap();
        assert!(f.is_package() && f.label() == 2);
        assert_eq!(
            index.entry(thing).unwrap().flags,
            EntryFlags::PACKAGE.with_label(2)
        );
        let tool = index.flags(entry_named(&index, 0, "Tool")).unwrap();
        assert!(!tool.is_package(), "a file is not a package");
        assert_eq!(tool.label(), 7);
        let alias = entry_named(&index, 0, "Alias");
        assert_eq!(index.flags(alias).unwrap().label(), 1);
        assert_eq!(
            index.entry(alias).unwrap().flags,
            EntryFlags::EMPTY.with_label(1)
        );
        // The package bit is the package's own: not passed down.
        let inside = index.flags(entry_named(&index, thing, "inside")).unwrap();
        assert!(!inside.is_package() && inside.label() == 0);
    }

    #[test]
    fn cancel_before_build() {
        let mut fs = ListFs::new("V");
        fs.push(2, 1, "a", F);
        let cancel = AtomicBool::new(true);
        assert!(build(&fs, &cancel, &mut |_| {}).unwrap().is_none());
        assert_eq!(fs.visited.load(Ordering::Relaxed), 0);
    }

    fn flat(n: u64) -> ListFs {
        let mut fs = ListFs::new("Flat");
        for k in 0..n {
            fs.push(k + 2, 1, &format!("file {k}.txt"), F);
        }
        fs
    }

    #[test]
    fn cancel_from_progress() {
        let fs = flat(40_000);
        let cancel = AtomicBool::new(false);
        let result = build(&fs, &cancel, &mut |_| cancel.store(true, Ordering::Relaxed)).unwrap();
        assert!(result.is_none());
        // The scan stopped at the first progress call.
        assert_eq!(fs.visited.load(Ordering::Relaxed), PROGRESS_EVERY);
    }

    #[test]
    fn progress_every_16384_entries() {
        let fs = flat(40_000);
        let mut calls = Vec::new();
        let index = build(&fs, &AtomicBool::new(false), &mut |p| calls.push(p.entries))
            .unwrap()
            .unwrap();
        assert_eq!(index.len(), 40_001);
        assert_eq!(calls, vec![16_384, 32_768, 40_000]);
        let mut last = 0;
        for c in calls {
            assert!(c - last <= PROGRESS_EVERY);
            last = c;
        }
    }

    #[test]
    fn child_lists_and_preorder_helpers() {
        // Slots: 0 is the root, s is arrival s - 1.
        let parent_slot = [2, 0, ORPHAN, 0, 2];
        let (end, children) = child_lists(&parent_slot, 6);
        assert_eq!(children_of(&end, &children, 0), &[1, 3]);
        assert_eq!(children_of(&end, &children, 1), &[] as &[u32]);
        assert_eq!(children_of(&end, &children, 2), &[0, 4]);
        assert_eq!(children_of(&end, &children, 99), &[] as &[u32]);
        let folder = IndexFlags::new(D, EntryFlags::EMPTY, false, KindClass::Folder).bits();
        let file = IndexFlags::new(F, EntryFlags::EMPTY, false, KindClass::Other).bits();
        let flags = [file, folder, file, file, file];
        let (order, parent) = preorder(&flags, &end, &children);
        assert_eq!(order, vec![1, 0, 4, 3]);
        assert_eq!(parent, vec![0, 0, 1, 1, 0]);
        // Hostile children lists that name an arrival twice: visited once.
        let (order, parent) = preorder(&[folder, file], &[2, 3, 3], &[0, 0, 1]);
        assert_eq!(order, vec![0, 1]);
        assert_eq!(parent, vec![0, 0, 1]);
    }

    // ---- Generated trees ----

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            // splitmix64
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    const WORDS: [&str; 24] = [
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
    const EXTS: [&str; 12] = [
        "txt", "pdf", "png", "rs", "mp3", "mov", "zip", "dmg", "ttf", "c", "JPG", "doc",
    ];

    /// A tree of `n` entries under root 1: folders of 1-200 entries, depth
    /// up to 12, names of 1-4 words plus a serial number and an extension,
    /// unique in their folder. Siblings are contiguous and folders come in a
    /// random order (like a catalog sorted by parent id, so many children
    /// arrive before their parents); `shuffle_all` shuffles every entry.
    fn generated(n: usize, seed: u64, shuffle_all: bool) -> ListFs {
        let mut fs = ListFs::new("Generated");
        let mut rng = Rng(seed);
        let mut open: Vec<(u64, u32)> = vec![(1, 0)];
        let mut groups: Vec<std::ops::Range<usize>> = Vec::new();
        let mut next_node = 2u64;
        let mut serial = 0u64;
        let mut name = String::new();
        while fs.recs.len() < n {
            if open.is_empty() {
                open.push((1, 0));
            }
            let (dir, depth) = open.swap_remove(rng.below(open.len() as u64) as usize);
            let count = (1 + rng.below(200) as usize).min(n - fs.recs.len());
            let start = fs.recs.len();
            for _ in 0..count {
                let is_folder = depth < 12 && rng.below(6) == 0;
                name.clear();
                for w in 0..=rng.below(4) {
                    if w > 0 {
                        name.push(' ');
                    }
                    name.push_str(WORDS[rng.below(WORDS.len() as u64) as usize]);
                }
                serial += 1;
                name.push(' ');
                name.push_str(&serial.to_string());
                if !is_folder {
                    name.push('.');
                    name.push_str(EXTS[rng.below(EXTS.len() as u64) as usize]);
                }
                let node = next_node;
                next_node += 1;
                let size = rng.below(1 << 32);
                let rsize = match rng.below(4) {
                    0 => Some(rng.below(100_000)),
                    1 => Some(0),
                    _ => None,
                };
                let modified = (rng.below(8) != 0)
                    .then(|| Timestamp::from_unix_nanos((rng.next() >> 2) as i64));
                let hidden = rng.below(20) == 0;
                let kind = if is_folder { D } else { F };
                let r = fs.push(node, dir, &name, kind);
                if !is_folder {
                    r.size = Some(size);
                    r.rsize = rsize;
                }
                r.modified = modified;
                if hidden {
                    r.flags = EntryFlags::HIDDEN;
                }
                if is_folder {
                    open.push((node, depth + 1));
                }
            }
            groups.push(start..fs.recs.len());
        }
        let recs = std::mem::take(&mut fs.recs);
        let out = if shuffle_all {
            let mut out = recs;
            for k in (1..out.len()).rev() {
                out.swap(k, rng.below(k as u64 + 1) as usize);
            }
            out
        } else {
            for k in (1..groups.len()).rev() {
                groups.swap(k, rng.below(k as u64 + 1) as usize);
            }
            let mut out = Vec::with_capacity(recs.len());
            for g in groups {
                out.extend_from_slice(&recs[g]);
            }
            out
        };
        fs.recs = out;
        fs
    }

    /// Adds hard links under the root: three files, and a second report of
    /// a folder that has children, placed a third of the way into arrival
    /// order (so either report may come first).
    fn add_links(fs: &mut ListFs) {
        let files: Vec<Rec> = fs
            .recs
            .iter()
            .filter(|r| r.kind == F)
            .take(3)
            .cloned()
            .collect();
        for (k, r) in files.iter().enumerate() {
            let name = format!("file link {k}.txt");
            fs.push(r.node, 1, &name, F).size = r.size;
        }
        let parents: std::collections::HashSet<u64> = fs.recs.iter().map(|r| r.parent).collect();
        let folder = fs
            .recs
            .iter()
            .find(|r| r.kind == D && parents.contains(&r.node))
            .unwrap()
            .node;
        fs.push(folder, 1, "folder link", D);
        let link = fs.recs.pop().unwrap();
        let at = fs.recs.len() / 3;
        fs.recs.insert(at, link);
    }

    fn check_properties(fs: &ListFs) {
        let index = build_all(fs);
        let n = index.len();
        assert_eq!(index.verify(), Ok(()));
        assert_eq!(n as usize, fs.recs.len() + 1);
        assert_eq!(
            index.summary(),
            ScanSummary {
                entries: fs.recs.len() as u64,
                skipped: 0
            }
        );

        // Parents before children.
        for i in 1..n {
            assert!(index.parent(i).unwrap() < i);
        }
        assert_eq!(index.subtree(0), 0..n);

        // A subtree is exactly the entries whose ancestor chain holds it.
        let ranges: Vec<std::ops::Range<u32>> = (0..n).map(|i| index.subtree(i)).collect();
        let mut inside = vec![0u32; n as usize];
        for j in 0..n {
            let mut a = j;
            loop {
                assert!(ranges[a as usize].contains(&j), "{j} not in subtree of {a}");
                inside[a as usize] += 1;
                if a == 0 {
                    break;
                }
                a = index.parent(a).unwrap();
            }
        }
        for i in 0..n {
            assert_eq!(
                ranges[i as usize].len() as u32,
                inside[i as usize],
                "entry {i}"
            );
            if !index.is_folder(i) {
                assert_eq!(ranges[i as usize], i..i + 1);
            }
        }

        // Crumbs round-trip through lookup.
        for i in 0..n {
            let crumbs = index.crumbs(i);
            assert_eq!(crumbs[0], (NodeId(fs.root), fs.volume.clone()));
            assert_eq!(crumbs.last().unwrap().0, index.entry(i).unwrap().node);
            for pair in crumbs.windows(2) {
                assert_eq!(fs.lookup(pair[0].0, &pair[1].1).unwrap(), Some(pair[1].0));
            }
        }

        // Each entry is the scan entry that produced it, and every scan
        // entry is used once. Key: (parent node, name), unique per folder.
        let mut by_key: HashMap<(u64, &str), (usize, u32)> = HashMap::new();
        for (k, r) in fs.recs.iter().enumerate() {
            assert!(by_key.insert((r.parent, fs.name_of(r)), (k, 0)).is_none());
        }
        for i in 1..n {
            let e = index.entry(i).unwrap();
            let parent_node = index.entry(index.parent(i).unwrap()).unwrap().node.0;
            let slot = by_key.get_mut(&(parent_node, e.name)).unwrap();
            slot.1 += 1;
            let s = fs.scan_entry(&fs.recs[slot.0]);
            assert_eq!(e.node, s.node);
            assert_eq!(e.name, s.name);
            assert_eq!(e.kind, s.kind);
            assert_eq!(e.size, s.size);
            assert_eq!(e.resource_size, s.resource_size);
            assert_eq!(e.modified, s.modified);
            assert_eq!(e.flags, s.flags);
            let f = index.flags(i).unwrap();
            assert_eq!(
                f.has_resource_fork(),
                s.resource_size.is_some_and(|r| r > 0)
            );
            assert_eq!(
                f.kind_class(),
                classify_entry(s.name, s.kind == D, s.finder_type)
            );
        }
        assert!(by_key.values().all(|&(_, used)| used == 1));

        // Children of a folder reported twice hang from its first report.
        let mut first_report: HashMap<u64, (u64, &str)> = HashMap::new();
        for r in &fs.recs {
            if r.kind == D {
                first_report
                    .entry(r.node)
                    .or_insert((r.parent, fs.name_of(r)));
            }
        }
        for i in 1..n {
            let p = index.parent(i).unwrap();
            if p == 0 {
                continue;
            }
            let pe = index.entry(p).unwrap();
            let grand = index.entry(index.parent(p).unwrap()).unwrap().node.0;
            assert_eq!(first_report[&pe.node.0], (grand, pe.name));
        }

        // Folded is the fold of names.
        let mut folded = Vec::new();
        fold_into(
            std::str::from_utf8(index.names_bytes()).unwrap(),
            &mut folded,
        );
        assert_eq!(index.folded_bytes(), folded.as_slice());

        // find_node gives each node's first entry.
        let mut first: HashMap<u64, u32> = HashMap::new();
        for i in 0..n {
            first.entry(index.entry(i).unwrap().node.0).or_insert(i);
        }
        for (&node, &i) in &first {
            assert_eq!(index.find_node(NodeId(node)), Some(i));
        }

        // Deterministic.
        let again = build_all(fs);
        assert_eq!(again.names_bytes(), index.names_bytes());
        assert_eq!(again.offsets(), index.offsets());
        assert_eq!(again.parents(), index.parents());
        assert_eq!(again.nodes(), index.nodes());
        assert_eq!(again.sizes(), index.sizes());
        assert_eq!(again.modified_times(), index.modified_times());
        assert_eq!(again.flag_words(), index.flag_words());
    }

    #[test]
    fn generated_tree_properties_grouped() {
        let mut fs = generated(3_000, 7, false);
        add_links(&mut fs);
        check_properties(&fs.with_lookup());
    }

    #[test]
    fn generated_tree_properties_shuffled() {
        let mut fs = generated(3_000, 11, true);
        add_links(&mut fs);
        check_properties(&fs.with_lookup());
    }

    #[test]
    #[ignore = "timing; run in release with --ignored --nocapture"]
    // Printing the measurement is this test's purpose.
    #[allow(clippy::print_stdout)]
    fn index_build_throughput() {
        const N: usize = 1_000_000;
        let made = std::time::Instant::now();
        let fs = generated(N, 42, false);
        let gen_time = made.elapsed();
        let before_parent = {
            let mut seen = std::collections::HashSet::new();
            seen.insert(fs.root);
            let mut early = 0usize;
            for r in &fs.recs {
                if !seen.contains(&r.parent) {
                    early += 1;
                }
                if r.kind == D {
                    seen.insert(r.node);
                }
            }
            early
        };
        let mut times = Vec::new();
        let mut last = None;
        for _ in 0..3 {
            let started = std::time::Instant::now();
            let index = build_all(&fs);
            times.push(started.elapsed().as_secs_f64());
            last = Some(index);
        }
        let index = last.unwrap();
        assert_eq!(index.len() as usize, N + 1);
        let best = times.iter().copied().fold(f64::INFINITY, f64::min);
        let entries = f64::from(index.len());
        let name_bytes = index.names_bytes().len() as f64 / entries - 1.0;
        let runs: Vec<String> = times.iter().map(|t| format!("{:.0}", t * 1e3)).collect();
        println!(
            "index_build_throughput: {} entries, build {:.0} ms best of 3 ({} ms), \
             {:.0} entries/s; {:.1} bytes/entry with {:.1}-byte names on average; \
             {:.0}% of entries arrive before their parent; generator {:.0} ms",
            index.len(),
            best * 1e3,
            runs.join("/"),
            entries / best,
            index.heap_bytes() as f64 / entries,
            name_bytes,
            100.0 * before_parent as f64 / N as f64,
            gen_time.as_secs_f64() * 1e3,
        );
    }
}
