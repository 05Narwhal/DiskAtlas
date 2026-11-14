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

//! Folder totals on this computer's own drives, measured lazily (design
//! `r10-everyday.md` §3.2).
//!
//! The host's search index is built only for search and never saved, so
//! folder sizes on the host do not use it. [`HostSizes`] keeps a tree of the
//! folders it has met, fills it on worker threads it owns, and corrects it
//! in place when [`HostSizes::invalidate`] says a folder changed.
//!
//! # How it works
//!
//! - **Arena.** One 56-byte node per folder: parent and flags, generation,
//!   own bytes and files (direct children only), subtree totals, and two
//!   counts kept up the parent chain: folders in the subtree still to be
//!   read, and folders in it that could not be read or hit a limit. So
//!   [`HostSizes::state_of`] is O(1). Names sit in one shared byte buffer,
//!   each folder's children (sorted by name, so a path resolves in
//!   depth × log fan-out) in one shared run buffer, and the bytes per kind
//!   in a side table that a folder with one kind does not use at all and a
//!   folder with two uses one word of. Files get no node.
//! - **Work unit: one folder.** One `read_dir`, and per entry its file type
//!   and metadata (on Windows both come from the directory read; on Unix
//!   one `lstat`). Symbolic links and junctions are never followed, and on
//!   Unix neither is a folder on another device than the root. Files are
//!   never opened. The read runs without the lock; the result is applied
//!   under it in one step, and its difference is added to every ancestor.
//! - **Workers** take the highest priority first and, within one, the folder
//!   queued last, so one folder finishes before the next starts. A unit
//!   that panics is caught: that folder reads as a lower bound.
//!
//! # What is bounded
//!
//! At most [`MAX_HOST_FOLDERS`] folders per tree and folders at most
//! [`MAX_DEPTH`] below the root; a folder whose sub-folders could not all
//! be added reads [`SizeState::AtLeast`], as do its ancestors. Every walk
//! up the tree is bounded by the depth limit, every walk down by the node
//! count, every read by the entries the directory returns. Sums saturate.
//! Nothing here panics on what the filesystem returns.

use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{self, AtomicBool};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::search::index::MAX_DEPTH;
use crate::search::kind::{KindClass, classify};
use crate::sizes::index::{ClassBytes, Totals};

/// Most folders one tree holds. Past it the walk stops, and the folders
/// whose sub-folders could not be added read [`SizeState::AtLeast`].
pub const MAX_HOST_FOLDERS: u32 = 2_000_000;

/// `on_change` is called at most once per this interval.
pub const NOTIFY_INTERVAL: Duration = Duration::from_millis(100);

/// Worker threads per tree: half the cores, at least 1, at most 4; 1 when
/// the number of cores cannot be told.
pub fn size_workers() -> usize {
    thread::available_parallelism()
        .map(|n| n.get() / 2)
        .unwrap_or(1)
        .clamp(1, 4)
}

/// How a folder's size stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeState {
    /// Not measured and not queued.
    Unknown,
    /// Being measured: the total so far.
    Running(Totals),
    /// Measured, complete.
    Exact(Totals),
    /// Measured, but some folders inside could not be read.
    AtLeast(Totals),
}

/// How urgently a folder is wanted. Higher first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Background,
    Listed,
    Visible,
}

/// A folder in the tree: its slot and the slot's generation, so a handle
/// to a folder that was deleted (and whose slot was reused) answers `Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HostDir {
    slot: u32,
    generation: u32,
}

/// The root's slot. It is never freed.
const ROOT: u32 = 0;

/// "None" for a run offset.
const NONE: u32 = u32::MAX;

/// Slots of [`ClassBytes::by_class`].
const CLASS_SLOTS: usize = 16;

/// Longest name stored, in bytes (a component is at most 255 UTF-16 units
/// on Windows and macOS, so at most 765 bytes of WTF-8 or UTF-8).
const MAX_NAME_BYTES: usize = 4096;

/// Nodes per arena chunk. Chunks never move, so growing the arena never
/// copies it under the lock.
const CHUNK: usize = 16_384;

// `Node::link`: the parent's slot in the low 22 bits, flags above.
const PARENT_MASK: u32 = 0x003F_FFFF;
const NO_PARENT: u32 = PARENT_MASK;
/// The priority: of the queue entry when queued, of the unit when busy.
const F_PRIO: u32 = 0x00C0_0000;
const P_LISTED: u32 = 0x0040_0000;
const P_VISIBLE: u32 = 0x0080_0000;
/// In a queue at the priority in `F_PRIO`.
const F_QUEUED: u32 = 0x0100_0000;
/// Read at least once: the own values are real.
const F_READ: u32 = 0x0200_0000;
/// Needs reading (never read, or a re-read was asked for). Counted in `pending`.
const F_PENDING: u32 = 0x0400_0000;
/// Being read by a worker or by `invalidate`.
const F_BUSY: u32 = 0x0800_0000;
/// Could not be read, or its sub-folders could not all be added. Counted in `bad`.
const F_BAD: u32 = 0x1000_0000;
/// Changed while busy: read it again when the read in progress lands.
const F_AGAIN: u32 = 0x2000_0000;
/// The slot is free.
const F_FREE: u32 = 0x4000_0000;
/// Inserted by `request` before its parent's listing confirmed it: checked
/// for links and devices before it is read.
const F_GUESS: u32 = 0x8000_0000;

// Every slot fits in the parent field, with `NO_PARENT` to spare.
const _: () = assert!(MAX_HOST_FOLDERS < NO_PARENT);
// Every class has a slot.
const _: () = assert!(KindClass::ALL.len() <= CLASS_SLOTS);

// `Node::classes`: no bytes, one class inline, or an offset into the side table.
const NO_CLASSES: u32 = u32::MAX;
const INLINE_CLASS: u32 = 0x8000_0000;
/// A side-table entry whose first word is at least this holds the class
/// mask in its top 16 bits and the first value in the low 48.
const TAG_UNIT: u64 = 0x0001_0000_0000_0000;

/// Garbage in a shared buffer is compacted away when it is more than half
/// the buffer and more than this many elements.
const COMPACT_MIN: usize = 1 << 16;

/// One folder. 56 bytes; a test checks it.
#[derive(Clone, Copy, Default)]
struct Node {
    /// Bytes of everything inside, this folder's files included.
    sub_bytes: u64,
    /// Bytes of the files directly inside.
    own_bytes: u64,
    /// Parent slot (`PARENT_MASK`) and flags.
    link: u32,
    generation: u32,
    /// Offset of the name in `State::names`.
    name: u32,
    /// Offset of the children run in `State::kids`, or `NONE`.
    kids: u32,
    /// See `NO_CLASSES` and `INLINE_CLASS`.
    classes: u32,
    /// Files directly inside. Saturating.
    own_files: u32,
    /// Files inside, everything below included. Saturating.
    sub_files: u32,
    /// Folders inside, this one not counted.
    sub_folders: u32,
    /// Folders in the subtree, this one included, with `F_PENDING`.
    pending: u32,
    /// Folders in the subtree, this one included, with `F_BAD`.
    bad: u32,
}

impl Node {
    fn parent(&self) -> Option<u32> {
        let p = self.link & PARENT_MASK;
        (p != NO_PARENT).then_some(p)
    }

    fn has(&self, flag: u32) -> bool {
        self.link & flag != 0
    }

    fn set(&mut self, flag: u32, on: bool) {
        if on {
            self.link |= flag;
        } else {
            self.link &= !flag;
        }
    }

    fn prio(&self) -> Priority {
        match self.link & F_PRIO {
            P_VISIBLE => Priority::Visible,
            P_LISTED => Priority::Listed,
            _ => Priority::Background,
        }
    }

    fn set_prio(&mut self, p: Priority) {
        let bits = match p {
            Priority::Background => 0,
            Priority::Listed => P_LISTED,
            Priority::Visible => P_VISIBLE,
        };
        self.link = (self.link & !F_PRIO) | bits;
    }

    fn totals(&self) -> Totals {
        Totals {
            bytes: self.sub_bytes,
            files: u64::from(self.sub_files),
            folders: u64::from(self.sub_folders),
        }
    }

    /// The state, from the node alone.
    fn state(&self) -> SizeState {
        if self.has(F_FREE) {
            SizeState::Unknown
        } else if self.pending == 0 {
            if self.bad > 0 {
                SizeState::AtLeast(self.totals())
            } else {
                SizeState::Exact(self.totals())
            }
        } else if !self.has(F_READ | F_QUEUED | F_BUSY) && self.kids == NONE {
            SizeState::Unknown
        } else {
            SizeState::Running(self.totals())
        }
    }
}

fn rank(p: Priority) -> usize {
    match p {
        Priority::Background => 0,
        Priority::Listed => 1,
        Priority::Visible => 2,
    }
}

fn ix(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn bump32(v: u32, add: u64, sub: u64) -> u32 {
    u32::try_from(u64::from(v).saturating_add(add).saturating_sub(sub)).unwrap_or(u32::MAX)
}

fn bump64(v: u64, add: u64, sub: u64) -> u64 {
    v.saturating_add(add).saturating_sub(sub)
}

fn class_bit(c: usize) -> u16 {
    u32::try_from(c)
        .ok()
        .and_then(|c| 1u16.checked_shl(c))
        .unwrap_or(0)
}

/// Grows `v` for `extra` more elements by at least an eighth, so a large
/// buffer's spare capacity stays small (it counts in `heap_bytes`).
fn reserve_for<T>(v: &mut Vec<T>, extra: usize) {
    if v.len().saturating_add(extra) > v.capacity() {
        v.reserve_exact(extra.max(v.len() / 8).max(256));
    }
}

/// The arena: fixed-size chunks, so growing never moves a node.
#[derive(Default)]
struct Nodes {
    chunks: Vec<Vec<Node>>,
    len: u32,
}

impl Nodes {
    fn get(&self, slot: u32) -> Option<&Node> {
        let i = ix(slot);
        self.chunks.get(i / CHUNK)?.get(i % CHUNK)
    }

    fn get_mut(&mut self, slot: u32) -> Option<&mut Node> {
        let i = ix(slot);
        self.chunks.get_mut(i / CHUNK)?.get_mut(i % CHUNK)
    }

    fn push(&mut self, node: Node) -> Option<u32> {
        let slot = self.len;
        if slot >= NO_PARENT {
            return None;
        }
        if self.chunks.last().is_none_or(|c| c.len() >= CHUNK) {
            // The first chunk grows as needed; later ones are allocated whole.
            let cap = if self.chunks.is_empty() { 0 } else { CHUNK };
            self.chunks.push(Vec::with_capacity(cap));
        }
        self.chunks.last_mut()?.push(node);
        self.len = slot.checked_add(1)?;
        Some(slot)
    }

    fn heap_bytes(&self) -> usize {
        self.chunks.iter().fold(vec_bytes(&self.chunks), |sum, c| {
            sum.saturating_add(vec_bytes(c))
        })
    }
}

fn vec_bytes<T>(v: &Vec<T>) -> usize {
    v.capacity().saturating_mul(std::mem::size_of::<T>())
}

/// Appends `name` with a 1- or 2-byte length in front. `None` when it is
/// too long or the buffer would pass `u32` offsets.
fn push_name(names: &mut Vec<u8>, name: &[u8]) -> Option<u32> {
    let len = name.len();
    if len > MAX_NAME_BYTES {
        return None;
    }
    let off = u32::try_from(names.len()).ok()?;
    let header = if len < 0x80 { 1 } else { 2 };
    let end = names.len().checked_add(header)?.checked_add(len)?;
    u32::try_from(end).ok()?;
    reserve_for(names, header.saturating_add(len));
    if len < 0x80 {
        names.push(u8::try_from(len).ok()?);
    } else {
        names.push(0x80 | u8::try_from(len % 0x80).ok()?);
        names.push(u8::try_from(len / 0x80).ok()?);
    }
    names.extend_from_slice(name);
    Some(off)
}

/// The name at `off`, and the bytes its entry takes (header included).
fn name_entry(names: &[u8], off: u32) -> (&[u8], usize) {
    let o = ix(off);
    let Some(&b0) = names.get(o) else {
        return (&[], 0);
    };
    let (len, header) = if b0 < 0x80 {
        (usize::from(b0), 1usize)
    } else {
        let b1 = names.get(o.saturating_add(1)).copied().unwrap_or(0);
        (
            usize::from(b0 & 0x7F).saturating_add(usize::from(b1).saturating_mul(0x80)),
            2,
        )
    };
    let start = o.saturating_add(header);
    let name = start
        .checked_add(len)
        .and_then(|end| names.get(start..end))
        .unwrap_or(&[]);
    (name, header.saturating_add(name.len()))
}

/// The children run at `off`: `[len, slot, slot, ...]`.
fn run_at(kids: &[u32], off: u32) -> &[u32] {
    if off == NONE {
        return &[];
    }
    let o = ix(off);
    let Some(&len) = kids.get(o) else {
        return &[];
    };
    let start = o.saturating_add(1);
    start
        .checked_add(ix(len))
        .and_then(|end| kids.get(start..end))
        .unwrap_or(&[])
}

/// A stored name as an `OsString`. Names are stored as
/// [`OsStr::as_encoded_bytes`]; this is the safe way back.
fn os_name(bytes: &[u8]) -> OsString {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return OsString::from(s);
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        OsStr::from_bytes(bytes).to_os_string()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        OsString::from_wide(&wtf8_to_wide(bytes))
    }
    #[cfg(not(any(unix, windows)))]
    {
        OsString::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// WTF-8 (what `as_encoded_bytes` gives on Windows: UTF-8 that may hold
/// unpaired surrogates) to UTF-16. Damaged input becomes U+FFFD.
#[cfg(any(windows, test))]
fn wtf8_to_wide(bytes: &[u8]) -> Vec<u16> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while let Some(&b0) = bytes.get(i) {
        let (mut cp, extra) = match b0 {
            0x00..=0x7F => (u32::from(b0), 0usize),
            0xC0..=0xDF => (u32::from(b0 & 0x1F), 1),
            0xE0..=0xEF => (u32::from(b0 & 0x0F), 2),
            0xF0..=0xF7 => (u32::from(b0 & 0x07), 3),
            _ => (0xFFFD, 0),
        };
        let mut used = 1usize;
        for k in 1..=extra {
            match bytes.get(i.saturating_add(k)) {
                Some(&b) if b & 0xC0 == 0x80 => {
                    cp = cp.wrapping_shl(6) | u32::from(b & 0x3F);
                    used = used.saturating_add(1);
                }
                _ => {
                    cp = 0xFFFD;
                    break;
                }
            }
        }
        if cp > 0x10FFFF {
            cp = 0xFFFD;
        }
        match cp.checked_sub(0x10000) {
            Some(v) => {
                out.push(0xD800 | u16::try_from(v / 0x400).unwrap_or(0));
                out.push(0xDC00 | u16::try_from(v % 0x400).unwrap_or(0));
            }
            None => out.push(u16::try_from(cp).unwrap_or(0xFFFD)),
        }
        i = i.saturating_add(used);
    }
    out
}

/// Differences to add to a node and every ancestor.
#[derive(Default)]
struct Delta {
    bytes_add: u64,
    bytes_sub: u64,
    files_add: u64,
    files_sub: u64,
    folders_add: u64,
    folders_sub: u64,
    pending_add: u64,
    pending_sub: u64,
    bad_add: u64,
    bad_sub: u64,
}

/// What one read of one folder gave.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
enum Status {
    #[default]
    Ok,
    /// The folder is not there (or is a link, or on another device).
    Gone,
    /// It could not be read, or the unit panicked.
    Unreadable,
    /// `shutdown` interrupted it.
    Stopped,
}

/// The result of one unit. Each worker keeps one and reuses its buffers.
#[derive(Default)]
struct Listing {
    status: Status,
    own_bytes: u64,
    own_files: u32,
    classes: [u64; CLASS_SLOTS],
    /// An entry could not be read: the folder is a lower bound.
    incomplete: bool,
    /// Sub-folder names, back to back, and the end of each.
    names: Vec<u8>,
    ends: Vec<u32>,
    /// Sub-folder numbers, sorted by name, without duplicates.
    order: Vec<u32>,
}

fn name_in<'a>(names: &'a [u8], ends: &[u32], k: u32) -> &'a [u8] {
    let k = ix(k);
    let end = ends.get(k).copied().map_or(0, ix);
    let start = k
        .checked_sub(1)
        .and_then(|p| ends.get(p).copied())
        .map_or(0, ix);
    names.get(start..end).unwrap_or(&[])
}

impl Listing {
    fn reset(&mut self) {
        self.status = Status::Ok;
        self.own_bytes = 0;
        self.own_files = 0;
        self.classes = [0; CLASS_SLOTS];
        self.incomplete = false;
        self.names.clear();
        self.ends.clear();
        self.order.clear();
    }

    fn name(&self, k: u32) -> &[u8] {
        name_in(&self.names, &self.ends, k)
    }

    fn push_dir(&mut self, name: &[u8]) {
        let end = self
            .names
            .len()
            .checked_add(name.len())
            .and_then(|e| u32::try_from(e).ok());
        let (Some(end), Ok(k)) = (end, u32::try_from(self.ends.len())) else {
            self.incomplete = true;
            return;
        };
        self.names.extend_from_slice(name);
        self.ends.push(end);
        self.order.push(k);
    }

    fn add_file(&mut self, name: &OsStr, len: u64) {
        self.own_bytes = self.own_bytes.saturating_add(len);
        self.own_files = self.own_files.saturating_add(1);
        let class = match name.to_str() {
            Some(s) => classify(s, None),
            None => classify(&name.to_string_lossy(), None),
        };
        if let Some(slot) = self.classes.get_mut(usize::from(class.bits())) {
            *slot = slot.saturating_add(len);
        }
    }

    /// Sorts the sub-folders by name and drops duplicates.
    fn seal(&mut self) {
        let Listing {
            names, ends, order, ..
        } = self;
        order.sort_unstable_by(|&a, &b| name_in(names, ends, a).cmp(name_in(names, ends, b)));
        order.dedup_by(|a, b| name_in(names, ends, *a) == name_in(names, ends, *b));
    }
}

/// A folder taken for reading.
struct Job {
    slot: u32,
    generation: u32,
    path: PathBuf,
    /// How many trailing components (the folder itself first) were guessed
    /// by `request` and must be checked before the read.
    check: usize,
}

/// The arena, the queues and the notification state. One lock.
struct State {
    nodes: Nodes,
    free: Vec<u32>,
    /// Slots in use.
    live: u32,
    names: Vec<u8>,
    names_garbage: usize,
    kids: Vec<u32>,
    kids_garbage: usize,
    classes: Vec<u64>,
    classes_garbage: usize,
    /// The own bytes per class of every folder, summed: `classes(root)`.
    tree_classes: [u64; CLASS_SLOTS],
    /// One stack per priority: within a priority, the last queued goes first.
    stacks: [Vec<(u32, u32)>; 3],
    scratch_old: Vec<u32>,
    scratch_new: Vec<u32>,
    scratch_queue: Vec<u32>,
    walk: Vec<u32>,
    max_folders: u32,
    max_depth: usize,
    /// Some total changed since `on_change` was last called.
    changed: bool,
    last_notify: Option<Instant>,
    /// How many times a worker decided to call `on_change`.
    notifies: u64,
    #[cfg(test)]
    notify_log: Vec<Instant>,
}

impl State {
    fn new(max_folders: u32, max_depth: usize) -> Self {
        let mut s = State {
            nodes: Nodes::default(),
            free: Vec::new(),
            live: 0,
            names: Vec::new(),
            names_garbage: 0,
            kids: Vec::new(),
            kids_garbage: 0,
            classes: Vec::new(),
            classes_garbage: 0,
            tree_classes: [0; CLASS_SLOTS],
            stacks: [Vec::new(), Vec::new(), Vec::new()],
            scratch_old: Vec::new(),
            scratch_new: Vec::new(),
            scratch_queue: Vec::new(),
            walk: Vec::new(),
            max_folders: max_folders.max(1),
            max_depth,
            changed: false,
            last_notify: None,
            notifies: 0,
            #[cfg(test)]
            notify_log: Vec::new(),
        };
        let name = push_name(&mut s.names, b"").unwrap_or(0);
        let root = Node {
            link: NO_PARENT | F_PENDING,
            name,
            kids: NONE,
            classes: NO_CLASSES,
            pending: 1,
            ..Node::default()
        };
        if s.nodes.push(root).is_some() {
            s.live = 1;
        }
        s
    }

    fn live_node(&self, slot: u32, generation: u32) -> Option<&Node> {
        self.nodes
            .get(slot)
            .filter(|n| !n.has(F_FREE) && n.generation == generation)
    }

    fn name_of(&self, slot: u32) -> &[u8] {
        self.nodes
            .get(slot)
            .map_or(&[], |n| name_entry(&self.names, n.name).0)
    }

    fn kids_of(&self, slot: u32) -> &[u32] {
        self.nodes
            .get(slot)
            .map_or(&[], |n| run_at(&self.kids, n.kids))
    }

    fn child(&self, parent: u32, name: &[u8]) -> Option<u32> {
        let kids = self.kids_of(parent);
        let i = kids.binary_search_by(|&c| self.name_of(c).cmp(name)).ok()?;
        kids.get(i).copied()
    }

    /// The slot of `rel` (a path relative to the root), if it is in the tree.
    fn find(&self, rel: &Path) -> Option<u32> {
        let mut cur = ROOT;
        let mut depth = 0usize;
        for comp in rel.components() {
            match comp {
                Component::Normal(n) => {
                    depth = depth.saturating_add(1);
                    if depth > self.max_depth {
                        return None;
                    }
                    cur = self.child(cur, n.as_encoded_bytes())?;
                }
                Component::CurDir => {}
                _ => return None,
            }
        }
        Some(cur)
    }

    /// Like `find`, inserting the missing folders as unread guesses.
    fn find_or_insert(&mut self, rel: &Path) -> Option<u32> {
        // Validate the whole path first, so a bad one inserts nothing.
        let mut depth = 0usize;
        for comp in rel.components() {
            match comp {
                Component::Normal(n) if n.as_encoded_bytes().len() <= MAX_NAME_BYTES => {
                    depth = depth.saturating_add(1);
                }
                Component::CurDir => {}
                _ => return None,
            }
        }
        if depth > self.max_depth {
            return None;
        }
        let mut cur = ROOT;
        for comp in rel.components() {
            let Component::Normal(n) = comp else {
                continue;
            };
            let name = n.as_encoded_bytes();
            cur = match self.child(cur, name) {
                Some(c) => c,
                None => self.insert_guess(cur, name)?,
            };
        }
        Some(cur)
    }

    fn insert_guess(&mut self, parent: u32, name: &[u8]) -> Option<u32> {
        let slot = self.alloc(parent, name, F_PENDING | F_GUESS)?;
        let mut list = std::mem::take(&mut self.scratch_new);
        list.clear();
        list.extend_from_slice(self.kids_of(parent));
        let at = list
            .binary_search_by(|&c| self.name_of(c).cmp(name))
            .unwrap_or_else(|i| i);
        list.insert(at, slot);
        self.set_kids(parent, &list);
        list.clear();
        self.scratch_new = list;
        let d = Delta {
            folders_add: 1,
            pending_add: 1,
            ..Delta::default()
        };
        self.propagate(parent, &d);
        self.changed = true;
        Some(slot)
    }

    /// The depth of `slot` (the root is 0); `None` past the depth limit or
    /// when the chain does not end at the root.
    fn depth(&self, slot: u32) -> Option<usize> {
        let mut cur = slot;
        for depth in 0..=self.max_depth {
            match self.nodes.get(cur)?.parent() {
                Some(p) => cur = p,
                None => return (cur == ROOT).then_some(depth),
            }
        }
        None
    }

    fn alloc(&mut self, parent: u32, name: &[u8], flags: u32) -> Option<u32> {
        if self.live >= self.max_folders {
            return None;
        }
        let name = push_name(&mut self.names, name)?;
        let mut node = Node {
            link: (parent & PARENT_MASK) | flags,
            name,
            kids: NONE,
            classes: NO_CLASSES,
            pending: u32::from(flags & F_PENDING != 0),
            ..Node::default()
        };
        let slot = if let Some(slot) = self.free.pop() {
            let n = self.nodes.get_mut(slot)?;
            node.generation = n.generation;
            *n = node;
            slot
        } else {
            match self.nodes.push(node) {
                Some(slot) => slot,
                None => {
                    self.names_garbage = self
                        .names_garbage
                        .saturating_add(name_entry(&self.names, name).1);
                    return None;
                }
            }
        };
        self.live = self.live.saturating_add(1);
        Some(slot)
    }

    fn free_slot(&mut self, slot: u32) {
        if slot == ROOT {
            return;
        }
        let Some(n) = self.nodes.get(slot).copied() else {
            return;
        };
        if n.has(F_FREE) {
            return;
        }
        self.names_garbage = self
            .names_garbage
            .saturating_add(name_entry(&self.names, n.name).1);
        if n.kids != NONE {
            let len = run_at(&self.kids, n.kids).len();
            self.kids_garbage = self.kids_garbage.saturating_add(len.saturating_add(1));
        }
        self.classes_garbage = self.classes_garbage.saturating_add(self.class_words(&n));
        if let Some(m) = self.nodes.get_mut(slot) {
            *m = Node {
                link: NO_PARENT | F_FREE,
                generation: n.generation.wrapping_add(1),
                kids: NONE,
                classes: NO_CLASSES,
                ..Node::default()
            };
        }
        self.free.push(slot);
        self.live = self.live.saturating_sub(1);
    }

    fn set_kids(&mut self, slot: u32, list: &[u32]) {
        let Some(old) = self.nodes.get(slot).map(|n| n.kids) else {
            return;
        };
        if old != NONE {
            let len = run_at(&self.kids, old).len();
            self.kids_garbage = self.kids_garbage.saturating_add(len.saturating_add(1));
        }
        let mut new = NONE;
        if !list.is_empty() {
            let off = u32::try_from(self.kids.len()).ok();
            let len = u32::try_from(list.len()).ok();
            let fits = self
                .kids
                .len()
                .checked_add(list.len())
                .and_then(|e| e.checked_add(1))
                .and_then(|e| u32::try_from(e).ok())
                .is_some_and(|e| e < NONE);
            if let (Some(off), Some(len), true) = (off, len, fits) {
                reserve_for(&mut self.kids, list.len().saturating_add(1));
                self.kids.push(len);
                self.kids.extend_from_slice(list);
                new = off;
            }
        }
        if let Some(n) = self.nodes.get_mut(slot) {
            n.kids = new;
        }
    }

    /// Side-table words of a node's class entry.
    fn class_words(&self, n: &Node) -> usize {
        if n.classes == NO_CLASSES || n.classes & INLINE_CLASS != 0 {
            return 0;
        }
        let Some(&w0) = self.classes.get(ix(n.classes)) else {
            return 0;
        };
        let (mask, tagged) = split_head(w0);
        let k = usize::try_from(mask.count_ones()).unwrap_or(0);
        if tagged { k.saturating_sub(1) } else { k }
    }

    /// The own bytes per class of a node.
    fn decode_classes(&self, n: &Node) -> [u64; CLASS_SLOTS] {
        let mut out = [0u64; CLASS_SLOTS];
        if n.classes == NO_CLASSES {
            return out;
        }
        if n.classes & INLINE_CLASS != 0 {
            if let Some(v) = out.get_mut(ix(n.classes & 0xF)) {
                *v = n.own_bytes;
            }
            return out;
        }
        let off = ix(n.classes);
        let head = self.classes.get(off).copied().unwrap_or(0);
        let (mask, tagged) = split_head(head);
        if mask.count_ones() < 2 {
            // Damaged entry: everything to Other.
            if let Some(v) = out.first_mut() {
                *v = n.own_bytes;
            }
            return out;
        }
        let highest = 15usize.saturating_sub(usize::try_from(mask.leading_zeros()).unwrap_or(0));
        let mut first = tagged.then_some(head % TAG_UNIT);
        let mut next = off.saturating_add(1);
        let mut sum = 0u64;
        for (c, slot) in out.iter_mut().enumerate() {
            if mask & class_bit(c) == 0 || c == highest {
                continue;
            }
            let v = match first.take() {
                Some(v) => v,
                None => {
                    let v = self.classes.get(next).copied().unwrap_or(0);
                    next = next.saturating_add(1);
                    v
                }
            };
            *slot = v;
            sum = sum.saturating_add(v);
        }
        if let Some(v) = out.get_mut(highest) {
            *v = n.own_bytes.saturating_sub(sum);
        }
        out
    }

    /// Stores the bytes per class of a folder. The class with the highest
    /// number is not stored: it is the folder's own bytes less the others.
    fn store_classes(&mut self, by: &[u64; CLASS_SLOTS]) -> u32 {
        let mut mask = 0u16;
        for (c, &v) in by.iter().enumerate() {
            if v != 0 {
                mask |= class_bit(c);
            }
        }
        match mask.count_ones() {
            0 => return NO_CLASSES,
            1 => {
                return INLINE_CLASS | mask.trailing_zeros();
            }
            _ => {}
        }
        let highest = 15usize.saturating_sub(usize::try_from(mask.leading_zeros()).unwrap_or(0));
        let words = mask.count_ones();
        let off = self.classes.len();
        let fits = off
            .checked_add(usize::try_from(words).unwrap_or(usize::MAX))
            .and_then(|e| u32::try_from(e).ok())
            .is_some_and(|e| e < INLINE_CLASS);
        let Ok(off32) = u32::try_from(off) else {
            return INLINE_CLASS | u32::try_from(highest).unwrap_or(0);
        };
        if !fits {
            return INLINE_CLASS | u32::try_from(highest).unwrap_or(0);
        }
        reserve_for(&mut self.classes, usize::try_from(words).unwrap_or(0));
        let mut first = true;
        for (c, &v) in by.iter().enumerate() {
            if mask & class_bit(c) == 0 || c == highest {
                continue;
            }
            if first {
                first = false;
                match u64::from(mask).checked_mul(TAG_UNIT) {
                    Some(tag) if v < TAG_UNIT => self.classes.push(tag | v),
                    _ => {
                        self.classes.push(u64::from(mask));
                        self.classes.push(v);
                    }
                }
            } else {
                self.classes.push(v);
            }
        }
        off32
    }

    /// Adds `d` to `from` and every ancestor.
    fn propagate(&mut self, from: u32, d: &Delta) {
        let mut cur = from;
        for _ in 0..=self.max_depth {
            let Some(n) = self.nodes.get_mut(cur) else {
                return;
            };
            n.sub_bytes = bump64(n.sub_bytes, d.bytes_add, d.bytes_sub);
            n.sub_files = bump32(n.sub_files, d.files_add, d.files_sub);
            n.sub_folders = bump32(n.sub_folders, d.folders_add, d.folders_sub);
            n.pending = bump32(n.pending, d.pending_add, d.pending_sub);
            n.bad = bump32(n.bad, d.bad_add, d.bad_sub);
            match n.parent() {
                Some(p) => cur = p,
                None => return,
            }
        }
    }

    /// Frees `slot` and everything below it, adding what they held to `d`
    /// (to be subtracted from the parent and up). The parent's run is not
    /// touched.
    fn free_subtree(&mut self, slot: u32, d: &mut Delta) {
        if slot == ROOT {
            return;
        }
        let Some(n) = self.nodes.get(slot).copied() else {
            return;
        };
        if n.has(F_FREE) {
            return;
        }
        d.bytes_sub = d.bytes_sub.saturating_add(n.sub_bytes);
        d.files_sub = d.files_sub.saturating_add(u64::from(n.sub_files));
        d.folders_sub = d
            .folders_sub
            .saturating_add(u64::from(n.sub_folders).saturating_add(1));
        d.pending_sub = d.pending_sub.saturating_add(u64::from(n.pending));
        d.bad_sub = d.bad_sub.saturating_add(u64::from(n.bad));
        let mut walk = std::mem::take(&mut self.walk);
        walk.clear();
        walk.push(slot);
        let limit = ix(self.nodes.len).saturating_add(1);
        for _ in 0..limit {
            let Some(s) = walk.pop() else {
                break;
            };
            let Some(node) = self.nodes.get(s).copied() else {
                continue;
            };
            if node.has(F_FREE) {
                continue;
            }
            walk.extend_from_slice(run_at(&self.kids, node.kids));
            let cls = self.decode_classes(&node);
            for (t, v) in self.tree_classes.iter_mut().zip(cls) {
                *t = t.saturating_sub(v);
            }
            self.free_slot(s);
        }
        walk.clear();
        self.walk = walk;
    }

    /// Removes `slot` and its subtree from its parent.
    fn detach(&mut self, slot: u32) {
        let Some(parent) = self.nodes.get(slot).and_then(Node::parent) else {
            return;
        };
        let mut d = Delta::default();
        self.free_subtree(slot, &mut d);
        let mut list = std::mem::take(&mut self.scratch_new);
        list.clear();
        list.extend(self.kids_of(parent).iter().copied().filter(|&c| c != slot));
        self.set_kids(parent, &list);
        list.clear();
        self.scratch_new = list;
        self.propagate(parent, &d);
    }

    /// Queues `slot` at `prio`, unless it needs no reading, is being read,
    /// or is queued at that priority or higher.
    fn enqueue(&mut self, slot: u32, prio: Priority) -> bool {
        let Some(n) = self.nodes.get_mut(slot) else {
            return false;
        };
        if n.has(F_FREE) || !n.has(F_PENDING) || n.has(F_BUSY) {
            return false;
        }
        if n.has(F_QUEUED) && n.prio() >= prio {
            return false;
        }
        n.set(F_QUEUED, true);
        n.set_prio(prio);
        let entry = (slot, n.generation);
        match self.stacks.get_mut(rank(prio)) {
            Some(stack) => {
                stack.push(entry);
                true
            }
            None => false,
        }
    }

    /// Queues every folder below `slot` (itself included) that needs
    /// reading at `prio`; raises the priority of folders being read.
    fn queue_subtree(&mut self, slot: u32, prio: Priority) -> usize {
        let mut pushed = 0usize;
        let mut walk = std::mem::take(&mut self.walk);
        walk.clear();
        walk.push(slot);
        let limit = ix(self.nodes.len).saturating_add(1);
        for _ in 0..limit {
            let Some(s) = walk.pop() else {
                break;
            };
            let Some(n) = self.nodes.get(s).copied() else {
                continue;
            };
            if n.has(F_FREE) || n.pending == 0 {
                continue;
            }
            if n.has(F_PENDING) {
                if n.has(F_BUSY) {
                    if prio > n.prio()
                        && let Some(m) = self.nodes.get_mut(s)
                    {
                        m.set_prio(prio);
                    }
                } else if self.enqueue(s, prio) {
                    pushed = pushed.saturating_add(1);
                }
            }
            walk.extend_from_slice(run_at(&self.kids, n.kids));
        }
        walk.clear();
        self.walk = walk;
        self.trim_stacks();
        pushed
    }

    /// Takes the next folder to read and marks it busy.
    fn pop_slot(&mut self) -> Option<(u32, u32)> {
        for r in (0..self.stacks.len()).rev() {
            while let Some((slot, generation)) = self.stacks.get_mut(r).and_then(Vec::pop) {
                if !entry_valid(&self.nodes, slot, generation, r) {
                    continue;
                }
                let Some(n) = self.nodes.get_mut(slot) else {
                    continue;
                };
                if !n.has(F_PENDING) || n.has(F_BUSY) {
                    n.set(F_QUEUED, false);
                    continue;
                }
                n.set(F_QUEUED, false);
                n.set(F_BUSY, true);
                return Some((slot, generation));
            }
        }
        None
    }

    fn next_job(&mut self, root: &Path) -> Option<Job> {
        while let Some((slot, _)) = self.pop_slot() {
            match self.job_for(slot, root) {
                Some(job) => return Some(job),
                None => {
                    if let Some(n) = self.nodes.get_mut(slot) {
                        n.set(F_BUSY, false);
                    }
                    self.finish_unreadable(slot);
                }
            }
        }
        None
    }

    /// The path of `slot` and how much of it was guessed.
    fn job_for(&self, slot: u32, root: &Path) -> Option<Job> {
        let mut chain = Vec::new();
        let mut cur = slot;
        let mut reached = false;
        for _ in 0..=self.max_depth {
            chain.push(cur);
            match self.nodes.get(cur)?.parent() {
                Some(p) => cur = p,
                None => {
                    reached = cur == ROOT;
                    break;
                }
            }
        }
        if !reached {
            return None;
        }
        let mut path = root.to_path_buf();
        let mut check = 0usize;
        let mut guessing = false;
        for &s in chain.iter().rev().skip(1) {
            path.push(os_name(self.name_of(s)));
            guessing |= self.nodes.get(s).is_some_and(|n| n.has(F_GUESS));
            if guessing {
                check = check.saturating_add(1);
            }
        }
        let generation = self.nodes.get(slot)?.generation;
        Some(Job {
            slot,
            generation,
            path,
            check,
        })
    }

    fn finish_unreadable(&mut self, slot: u32) {
        let Some(n) = self.nodes.get_mut(slot) else {
            return;
        };
        let mut d = Delta::default();
        n.set(F_READ, true);
        if n.has(F_PENDING) {
            n.set(F_PENDING, false);
            d.pending_sub = 1;
        }
        if !n.has(F_BAD) {
            n.set(F_BAD, true);
            d.bad_add = 1;
        }
        self.propagate(slot, &d);
    }

    /// Applies one read of `job`. `sync` is a read by `invalidate`: new
    /// sub-folders are queued at `Background` and nothing else is queued.
    /// Returns how many folders were queued.
    fn finish(&mut self, job: &Job, l: &Listing, sync: bool) -> usize {
        let slot = job.slot;
        let Some(n) = self.live_node(slot, job.generation) else {
            return 0;
        };
        if !n.has(F_BUSY) {
            return 0;
        }
        let unit_prio = n.prio();
        if let Some(n) = self.nodes.get_mut(slot) {
            n.set(F_BUSY, false);
        }
        let mut pushed = 0usize;
        match l.status {
            Status::Stopped => return 0,
            Status::Gone if slot != ROOT => {
                self.detach(slot);
                self.changed = true;
                self.maybe_compact();
                return 0;
            }
            Status::Gone | Status::Unreadable => self.finish_unreadable(slot),
            Status::Ok => {
                let (prio, matched) = if sync {
                    (Priority::Background, false)
                } else {
                    (unit_prio, true)
                };
                pushed = self.finish_listing(slot, l, prio, matched);
            }
        }
        if self.nodes.get(slot).is_some_and(|n| n.has(F_AGAIN)) {
            let mut d = Delta::default();
            if let Some(n) = self.nodes.get_mut(slot) {
                n.set(F_AGAIN, false);
                if !n.has(F_PENDING) {
                    n.set(F_PENDING, true);
                    d.pending_add = 1;
                }
            }
            self.propagate(slot, &d);
            if self.enqueue(slot, Priority::Background) {
                pushed = pushed.saturating_add(1);
            }
        }
        self.changed = true;
        self.maybe_compact();
        self.trim_stacks();
        pushed
    }

    /// Merges a successful listing into `slot`.
    fn finish_listing(
        &mut self,
        slot: u32,
        l: &Listing,
        prio: Priority,
        queue_matched: bool,
    ) -> usize {
        let depth = self.depth(slot);
        let can_nest = depth.is_some_and(|d| d < self.max_depth);
        let mut limited = depth.is_none();
        let mut d = Delta::default();
        let mut old = std::mem::take(&mut self.scratch_old);
        let mut new = std::mem::take(&mut self.scratch_new);
        let mut queue = std::mem::take(&mut self.scratch_queue);
        old.clear();
        new.clear();
        queue.clear();
        old.extend_from_slice(self.kids_of(slot));

        let (mut i, mut j) = (0usize, 0usize);
        let bound = old.len().saturating_add(l.order.len()).saturating_add(1);
        for _ in 0..bound {
            let o = old.get(i).copied();
            let name = l.order.get(j).map(|&k| l.name(k));
            let step = match (o, name) {
                (None, None) => break,
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (Some(c), Some(nm)) => self.name_of(c).cmp(nm),
            };
            match step {
                Ordering::Less => {
                    if let Some(c) = o {
                        self.free_subtree(c, &mut d);
                    }
                    i = i.saturating_add(1);
                }
                Ordering::Greater => {
                    if let Some(nm) = name {
                        match can_nest.then(|| self.alloc(slot, nm, F_PENDING)).flatten() {
                            Some(c) => {
                                new.push(c);
                                queue.push(c);
                                d.folders_add = d.folders_add.saturating_add(1);
                                d.pending_add = d.pending_add.saturating_add(1);
                            }
                            None => limited = true,
                        }
                    }
                    j = j.saturating_add(1);
                }
                Ordering::Equal => {
                    if let Some(c) = o {
                        new.push(c);
                        if let Some(cn) = self.nodes.get_mut(c) {
                            cn.set(F_GUESS, false);
                        }
                        if queue_matched {
                            queue.push(c);
                        }
                    }
                    i = i.saturating_add(1);
                    j = j.saturating_add(1);
                }
            }
        }
        self.set_kids(slot, &new);

        let mut pushed = 0usize;
        if let Some(n) = self.nodes.get(slot).copied() {
            let before = self.decode_classes(&n);
            self.classes_garbage = self.classes_garbage.saturating_add(self.class_words(&n));
            let class_ref = self.store_classes(&l.classes);
            for ((t, old_v), new_v) in self.tree_classes.iter_mut().zip(before).zip(l.classes) {
                *t = t.saturating_sub(old_v).saturating_add(new_v);
            }
            d.bytes_add = d.bytes_add.saturating_add(l.own_bytes);
            d.bytes_sub = d.bytes_sub.saturating_add(n.own_bytes);
            d.files_add = d.files_add.saturating_add(u64::from(l.own_files));
            d.files_sub = d.files_sub.saturating_add(u64::from(n.own_files));
            if n.has(F_PENDING) {
                d.pending_sub = d.pending_sub.saturating_add(1);
            }
            let bad = l.incomplete || limited;
            match (bad, n.has(F_BAD)) {
                (true, false) => d.bad_add = d.bad_add.saturating_add(1),
                (false, true) => d.bad_sub = d.bad_sub.saturating_add(1),
                _ => {}
            }
            if let Some(m) = self.nodes.get_mut(slot) {
                m.own_bytes = l.own_bytes;
                m.own_files = l.own_files;
                m.classes = class_ref;
                m.set(F_READ, true);
                m.set(F_PENDING, false);
                m.set(F_BAD, bad);
            }
            self.propagate(slot, &d);
            for &c in &queue {
                if self.enqueue(c, prio) {
                    pushed = pushed.saturating_add(1);
                }
            }
        }
        old.clear();
        new.clear();
        queue.clear();
        self.scratch_old = old;
        self.scratch_new = new;
        self.scratch_queue = queue;
        pushed
    }

    /// Drops stale queue entries when a stack has grown well past the tree.
    fn trim_stacks(&mut self) {
        let cap = ix(self.live).saturating_mul(2).saturating_add(1024);
        for r in 0..self.stacks.len() {
            let Some(stack) = self.stacks.get_mut(r) else {
                continue;
            };
            if stack.len() <= cap {
                continue;
            }
            let mut stack = std::mem::take(stack);
            stack.retain(|&(slot, generation)| entry_valid(&self.nodes, slot, generation, r));
            stack.shrink_to_fit();
            if let Some(s) = self.stacks.get_mut(r) {
                *s = stack;
            }
        }
    }

    fn maybe_compact(&mut self) {
        if self.names_garbage > COMPACT_MIN && self.names_garbage > self.names.len() / 2 {
            self.compact_names();
        }
        if self.kids_garbage > COMPACT_MIN && self.kids_garbage > self.kids.len() / 2 {
            self.compact_kids();
        }
        if self.classes_garbage > COMPACT_MIN && self.classes_garbage > self.classes.len() / 2 {
            self.compact_classes();
        }
    }

    fn compact_names(&mut self) {
        let old = std::mem::take(&mut self.names);
        let mut names = Vec::with_capacity(old.len().saturating_sub(self.names_garbage));
        for slot in 0..self.nodes.len {
            let Some(n) = self.nodes.get(slot).copied() else {
                continue;
            };
            let off = if n.has(F_FREE) {
                0
            } else {
                push_name(&mut names, name_entry(&old, n.name).0).unwrap_or(0)
            };
            if let Some(m) = self.nodes.get_mut(slot) {
                m.name = off;
            }
        }
        self.names = names;
        self.names_garbage = 0;
    }

    fn compact_kids(&mut self) {
        let old = std::mem::take(&mut self.kids);
        self.kids = Vec::with_capacity(old.len().saturating_sub(self.kids_garbage));
        self.kids_garbage = 0;
        let mut list = std::mem::take(&mut self.scratch_new);
        for slot in 0..self.nodes.len {
            let Some(n) = self.nodes.get(slot).copied() else {
                continue;
            };
            if n.has(F_FREE) || n.kids == NONE {
                continue;
            }
            list.clear();
            list.extend_from_slice(run_at(&old, n.kids));
            if let Some(m) = self.nodes.get_mut(slot) {
                m.kids = NONE;
            }
            self.set_kids(slot, &list);
        }
        list.clear();
        self.scratch_new = list;
    }

    fn compact_classes(&mut self) {
        let mut fresh = Vec::with_capacity(self.classes.len().saturating_sub(self.classes_garbage));
        for slot in 0..self.nodes.len {
            let Some(n) = self.nodes.get(slot).copied() else {
                continue;
            };
            if n.has(F_FREE) || n.classes == NO_CLASSES || n.classes & INLINE_CLASS != 0 {
                continue;
            }
            let words = self.class_words(&n);
            let start = ix(n.classes);
            let src = start
                .checked_add(words)
                .and_then(|end| self.classes.get(start..end))
                .unwrap_or(&[]);
            let off = u32::try_from(fresh.len()).unwrap_or(NO_CLASSES);
            fresh.extend_from_slice(src);
            if let Some(m) = self.nodes.get_mut(slot) {
                m.classes = if src.is_empty() { NO_CLASSES } else { off };
            }
        }
        self.classes = fresh;
        self.classes_garbage = 0;
    }

    /// Calls for `on_change` when something changed and the last call was
    /// at least `NOTIFY_INTERVAL` ago.
    fn take_notify(&mut self, now: Instant) -> bool {
        if !self.changed {
            return false;
        }
        if self
            .last_notify
            .is_some_and(|t| now.saturating_duration_since(t) < NOTIFY_INTERVAL)
        {
            return false;
        }
        self.changed = false;
        self.last_notify = Some(now);
        self.notifies = self.notifies.saturating_add(1);
        #[cfg(test)]
        self.notify_log.push(now);
        true
    }

    /// How long until `take_notify` would say yes.
    fn notify_wait(&self, now: Instant) -> Duration {
        self.last_notify
            .and_then(|t| t.checked_add(NOTIFY_INTERVAL))
            .map_or(Duration::ZERO, |due| due.saturating_duration_since(now))
    }

    /// The own bytes per class of `slot` and everything below it.
    fn subtree_classes(&self, slot: u32) -> [u64; CLASS_SLOTS] {
        if slot == ROOT {
            return self.tree_classes;
        }
        let mut sum = [0u64; CLASS_SLOTS];
        let mut walk = vec![slot];
        let limit = ix(self.nodes.len).saturating_add(1);
        for _ in 0..limit {
            let Some(s) = walk.pop() else {
                break;
            };
            let Some(n) = self.nodes.get(s) else {
                continue;
            };
            if n.has(F_FREE) {
                continue;
            }
            for (t, v) in sum.iter_mut().zip(self.decode_classes(n)) {
                *t = t.saturating_add(v);
            }
            walk.extend_from_slice(run_at(&self.kids, n.kids));
        }
        sum
    }

    fn invalidate_all(&mut self) -> usize {
        let mut order = std::mem::take(&mut self.scratch_old);
        let mut walk = std::mem::take(&mut self.walk);
        order.clear();
        walk.clear();
        walk.push(ROOT);
        let limit = ix(self.nodes.len).saturating_add(1);
        for _ in 0..limit {
            let Some(s) = walk.pop() else {
                break;
            };
            order.push(s);
            walk.extend_from_slice(self.kids_of(s));
        }
        for &s in &order {
            if let Some(n) = self.nodes.get_mut(s)
                && n.has(F_READ)
                && !n.has(F_FREE)
            {
                if n.has(F_BUSY) {
                    n.set(F_AGAIN, true);
                } else {
                    n.set(F_PENDING, true);
                }
            }
        }
        // Children come after their parent in `order`: count bottom-up.
        for &s in order.iter().rev() {
            let own = self
                .nodes
                .get(s)
                .map_or(0u32, |n| u32::from(n.has(F_PENDING)));
            let below = self.kids_of(s).iter().fold(0u32, |sum, &c| {
                sum.saturating_add(self.nodes.get(c).map_or(0, |n| n.pending))
            });
            if let Some(n) = self.nodes.get_mut(s) {
                n.pending = own.saturating_add(below);
            }
        }
        let mut pushed = 0usize;
        for &s in &order {
            if self.nodes.get(s).is_some_and(|n| n.has(F_READ))
                && self.enqueue(s, Priority::Background)
            {
                pushed = pushed.saturating_add(1);
            }
        }
        order.clear();
        walk.clear();
        self.scratch_old = order;
        self.walk = walk;
        self.changed = true;
        self.trim_stacks();
        pushed
    }

    fn refocus(&mut self, keep: &[u32]) {
        let max_depth = self.max_depth;
        let Some(stack) = self.stacks.get_mut(rank(Priority::Background)) else {
            return;
        };
        let mut stack = std::mem::take(stack);
        let nodes = &mut self.nodes;
        stack.retain(|&(slot, generation)| {
            if !entry_valid(nodes, slot, generation, 0) {
                return false;
            }
            if inside(nodes, slot, keep, max_depth) {
                return true;
            }
            if let Some(n) = nodes.get_mut(slot) {
                n.set(F_QUEUED, false);
            }
            false
        });
        if let Some(s) = self.stacks.get_mut(rank(Priority::Background)) {
            *s = stack;
        }
    }

    fn heap_bytes(&self) -> usize {
        [
            self.nodes.heap_bytes(),
            vec_bytes(&self.free),
            vec_bytes(&self.names),
            vec_bytes(&self.kids),
            vec_bytes(&self.classes),
            self.stacks
                .iter()
                .fold(0usize, |s, v| s.saturating_add(vec_bytes(v))),
            vec_bytes(&self.scratch_old),
            vec_bytes(&self.scratch_new),
            vec_bytes(&self.scratch_queue),
            vec_bytes(&self.walk),
        ]
        .iter()
        .fold(0usize, |sum, &b| sum.saturating_add(b))
    }
}

/// The mask of a side-table entry and whether its first value is in the
/// same word.
fn split_head(w0: u64) -> (u16, bool) {
    let top = u16::try_from(w0 / TAG_UNIT).unwrap_or(0);
    if top != 0 {
        (top, true)
    } else {
        (u16::try_from(w0 % 0x1_0000).unwrap_or(0), false)
    }
}

fn entry_valid(nodes: &Nodes, slot: u32, generation: u32, r: usize) -> bool {
    nodes.get(slot).is_some_and(|n| {
        !n.has(F_FREE) && n.generation == generation && n.has(F_QUEUED) && rank(n.prio()) == r
    })
}

/// Is `slot` one of `keep` or below one?
fn inside(nodes: &Nodes, slot: u32, keep: &[u32], max_depth: usize) -> bool {
    let mut cur = slot;
    for _ in 0..=max_depth {
        if keep.contains(&cur) {
            return true;
        }
        match nodes.get(cur).and_then(Node::parent) {
            Some(p) => cur = p,
            None => return false,
        }
    }
    false
}

#[cfg(test)]
type Seam = Box<dyn Fn(&Path) -> bool + Send + Sync>;

/// What `new` fixes; tests lower the limits.
struct Config {
    workers: usize,
    max_folders: u32,
    max_depth: usize,
    /// Called with each unit's path before it is read; `true` makes the
    /// read fail. It may block or panic.
    #[cfg(test)]
    seam: Option<Seam>,
}

impl Config {
    fn standard() -> Self {
        Config {
            workers: size_workers(),
            max_folders: MAX_HOST_FOLDERS,
            max_depth: MAX_DEPTH,
            #[cfg(test)]
            seam: None,
        }
    }
}

/// What the workers share with `HostSizes`.
struct Inner {
    root: PathBuf,
    /// The root's device on Unix; folders on other devices are not entered.
    root_dev: Option<u64>,
    workers: usize,
    state: Mutex<State>,
    work: Condvar,
    stop: AtomicBool,
    on_change: Box<dyn Fn() + Send + Sync>,
    #[cfg(test)]
    seam: Option<Seam>,
}

#[derive(Default)]
struct Workers {
    started: bool,
    handles: Vec<JoinHandle<()>>,
}

/// The folder-size tree of one host drive or place.
///
/// `Send + Sync`; every method takes `&self` and blocks only briefly on the
/// tree's lock, except [`invalidate`](Self::invalidate) (one directory
/// read), [`classes`](Self::classes) and [`children`](Self::children)
/// (O(subtree) or O(children)), and [`shutdown`](Self::shutdown).
///
/// Workers start on the first [`request`](Self::request). If no worker
/// thread can be spawned, requests stay queued and nothing is measured.
pub struct HostSizes {
    inner: Arc<Inner>,
    workers: Mutex<Workers>,
}

impl HostSizes {
    /// One per host drive or place. `on_change` is called from a worker
    /// thread, at most once per `NOTIFY_INTERVAL`, when any total changed.
    pub fn new(root: PathBuf, on_change: Box<dyn Fn() + Send + Sync>) -> Self {
        Self::with_config(root, on_change, Config::standard())
    }

    fn with_config(root: PathBuf, on_change: Box<dyn Fn() + Send + Sync>, config: Config) -> Self {
        let root_dev = root_device(&root);
        let state = State::new(
            config.max_folders.min(MAX_HOST_FOLDERS),
            config.max_depth.min(MAX_DEPTH),
        );
        HostSizes {
            inner: Arc::new(Inner {
                root,
                root_dev,
                workers: config.workers,
                state: Mutex::new(state),
                work: Condvar::new(),
                stop: AtomicBool::new(false),
                on_change,
                #[cfg(test)]
                seam: config.seam,
            }),
            workers: Mutex::new(Workers::default()),
        }
    }

    fn rel<'a>(&self, dir: &'a Path) -> Option<&'a Path> {
        dir.strip_prefix(&self.inner.root).ok()
    }

    fn stopped(&self) -> bool {
        self.inner.stop.load(atomic::Ordering::SeqCst)
    }

    /// Queue `dir` and everything inside it. Raising a priority re-queues.
    ///
    /// A folder below the root that the tree has not reached yet is added
    /// with the folders between, unread. A path outside the root, or with
    /// `..` in it, is ignored, as is a folder already measured (changes
    /// come through [`invalidate`](Self::invalidate)).
    pub fn request(&self, dir: &Path, priority: Priority) {
        if self.stopped() {
            return;
        }
        let Some(rel) = self.rel(dir) else {
            return;
        };
        let pushed = {
            let mut st = lock(&self.inner.state);
            let Some(slot) = st.find_or_insert(rel) else {
                return;
            };
            if st.nodes.get(slot).is_none_or(|n| n.pending == 0) {
                return;
            }
            st.queue_subtree(slot, priority)
        };
        if pushed > 0 {
            self.start_workers();
            self.inner.work.notify_all();
        }
    }

    /// Drop every queued folder below `Listed` priority that is not inside
    /// one of `keep`. What was measured stays.
    pub fn refocus(&self, keep: &[PathBuf]) {
        let mut st = lock(&self.inner.state);
        let slots: Vec<u32> = keep
            .iter()
            .filter_map(|p| self.rel(p).and_then(|rel| st.find(rel)))
            .collect();
        st.refocus(&slots);
    }

    fn with_node<T>(&self, dir: &Path, f: impl FnOnce(&State, u32, &Node) -> T) -> Option<T> {
        let rel = self.rel(dir)?;
        let st = lock(&self.inner.state);
        let slot = st.find(rel)?;
        let n = *st.nodes.get(slot)?;
        Some(f(&st, slot, &n))
    }

    /// The state of `dir`; `Unknown` when it is not in the tree.
    pub fn state(&self, dir: &Path) -> SizeState {
        self.with_node(dir, |_, _, n| n.state())
            .unwrap_or(SizeState::Unknown)
    }

    /// Bytes per kind inside `dir`, everything below included. O(1) for the
    /// root, O(subtree) otherwise: call it off the UI thread. `system` is
    /// always 0 on the host.
    pub fn classes(&self, dir: &Path) -> ClassBytes {
        self.with_node(dir, |st, slot, _| ClassBytes {
            by_class: st.subtree_classes(slot),
            system: 0,
        })
        .unwrap_or_default()
    }

    /// The files directly in `dir`, its sub-folders not counted
    /// (`folders` is 0).
    pub fn own(&self, dir: &Path) -> Totals {
        self.with_node(dir, |_, _, n| Totals {
            bytes: n.own_bytes,
            files: u64::from(n.own_files),
            folders: 0,
        })
        .unwrap_or_default()
    }

    /// The sub-folders of `dir` that are known, with their states, sorted
    /// by the bytes of their names. For the space map (R7) and "What is
    /// inside" (§7). Off the UI thread.
    pub fn children(&self, dir: &Path) -> Vec<(OsString, SizeState)> {
        self.with_node(dir, |st, slot, _| {
            st.kids_of(slot)
                .iter()
                .filter_map(|&c| {
                    let n = st.nodes.get(c)?;
                    Some((os_name(st.name_of(c)), n.state()))
                })
                .collect()
        })
        .unwrap_or_default()
    }

    /// Is `dir` in the tree? Cheap; the watcher asks it for every event.
    pub fn knows(&self, dir: &Path) -> bool {
        self.with_node(dir, |_, _, _| ()).is_some()
    }

    /// A handle a row keeps, so a frame never resolves a path.
    pub fn dir(&self, dir: &Path) -> Option<HostDir> {
        self.with_node(dir, |_, slot, n| HostDir {
            slot,
            generation: n.generation,
        })
    }

    /// O(1). `Unknown` when the folder behind the handle is gone.
    pub fn state_of(&self, dir: HostDir) -> SizeState {
        let st = lock(&self.inner.state);
        st.live_node(dir.slot, dir.generation)
            .map_or(SizeState::Unknown, Node::state)
    }

    /// Bytes measured so far under the root, for the bar's percentage.
    pub fn measured_bytes(&self) -> u64 {
        let st = lock(&self.inner.state);
        st.nodes.get(ROOT).map_or(0, |n| n.sub_bytes)
    }

    /// Bytes held by the tree: nodes, names, children, classes and queues.
    pub fn heap_bytes(&self) -> usize {
        let st = lock(&self.inner.state);
        st.heap_bytes().saturating_add(self.inner.root.capacity())
    }

    /// The direct entries of `dir` changed: read it again and pass the
    /// difference up. Unknown folders are ignored.
    ///
    /// Reads the folder on the calling thread (one `read_dir`): call it off
    /// the UI thread. A folder that is in the tree but was never read is
    /// ignored too; one being read by a worker (for the first time or
    /// again) is read again after.
    pub fn invalidate(&self, dir: &Path) {
        if self.stopped() {
            return;
        }
        let Some(rel) = self.rel(dir) else {
            return;
        };
        let job = {
            let mut st = lock(&self.inner.state);
            let Some(slot) = st.find(rel) else {
                return;
            };
            let Some(n) = st.nodes.get_mut(slot) else {
                return;
            };
            // Being read (the first time or again): that read may have missed
            // the change, so read once more when it lands.
            if n.has(F_BUSY) {
                n.set(F_AGAIN, true);
                return;
            }
            if !n.has(F_READ) {
                return;
            }
            n.set(F_QUEUED, false);
            n.set(F_BUSY, true);
            match st.job_for(slot, &self.inner.root) {
                Some(job) => job,
                None => {
                    if let Some(n) = st.nodes.get_mut(slot) {
                        n.set(F_BUSY, false);
                    }
                    return;
                }
            }
        };
        let mut listing = Listing::default();
        read_job(&self.inner, &job, &mut listing);
        let pushed = {
            let mut st = lock(&self.inner.state);
            st.finish(&job, &listing, true)
        };
        if pushed > 0 {
            self.start_workers();
        }
        // Wake a worker to deliver `on_change` (and take new work).
        self.inner.work.notify_all();
    }

    /// Everything may have changed (the watcher lost events). Every folder
    /// that was read is queued at `Background` to be read again; the old
    /// numbers stay (as `Running`) until each re-read replaces them.
    pub fn invalidate_all(&self) {
        if self.stopped() {
            return;
        }
        let pushed = lock(&self.inner.state).invalidate_all();
        if pushed > 0 {
            self.start_workers();
        }
        self.inner.work.notify_all();
    }

    /// Stop the workers and wait for them.
    ///
    /// Idempotent, and safe from any thread; called from a worker thread
    /// (from inside `on_change`), it does not wait for that worker, which
    /// exits when `on_change` returns. A worker in the middle of a folder
    /// stops at the next entry. After it, [`request`](Self::request) does
    /// nothing.
    pub fn shutdown(&self) {
        self.inner.stop.store(true, atomic::Ordering::SeqCst);
        // Taking the lock orders the flag before any worker's next wait.
        drop(lock(&self.inner.state));
        self.inner.work.notify_all();
        // Take the handles and release the lock before joining, so a worker
        // that reaches `start_workers` (from inside `on_change`) cannot block
        // on it while we wait for that worker.
        let handles = std::mem::take(&mut lock(&self.workers).handles);
        let me = thread::current().id();
        for handle in handles {
            if handle.thread().id() == me {
                continue;
            }
            let _ = handle.join();
        }
    }

    fn start_workers(&self) {
        let mut workers = lock(&self.workers);
        if workers.started || self.stopped() {
            return;
        }
        workers.started = true;
        for _ in 0..self.inner.workers {
            let inner = Arc::clone(&self.inner);
            match thread::Builder::new()
                .name("disk-atlas-sizes".into())
                .spawn(move || worker(inner))
            {
                Ok(handle) => workers.handles.push(handle),
                Err(_) => break,
            }
        }
    }
}

impl Drop for HostSizes {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn deliver(inner: &Inner) {
    let _ = catch_unwind(AssertUnwindSafe(|| (inner.on_change)()));
}

fn worker(inner: Arc<Inner>) {
    let mut listing = Listing::default();
    loop {
        let job = {
            let mut st = lock(&inner.state);
            loop {
                if inner.stop.load(atomic::Ordering::SeqCst) {
                    return;
                }
                if let Some(job) = st.next_job(&inner.root) {
                    break Some(job);
                }
                if st.changed {
                    let now = Instant::now();
                    if st.take_notify(now) {
                        break None;
                    }
                    let wait = st.notify_wait(now);
                    st = inner
                        .work
                        .wait_timeout(st, wait)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                } else {
                    st = inner.work.wait(st).unwrap_or_else(PoisonError::into_inner);
                }
            }
        };
        let Some(job) = job else {
            deliver(&inner);
            continue;
        };
        read_job(&inner, &job, &mut listing);
        let (pushed, fire) = {
            let mut st = lock(&inner.state);
            let pushed = st.finish(&job, &listing, false);
            (pushed, st.take_notify(Instant::now()))
        };
        if pushed > 1 {
            inner.work.notify_all();
        }
        if fire {
            deliver(&inner);
        }
    }
}

/// Reads one job's folder into `out`, catching a panic.
fn read_job(inner: &Inner, job: &Job, out: &mut Listing) {
    let result = catch_unwind(AssertUnwindSafe(|| {
        out.reset();
        #[cfg(test)]
        if let Some(seam) = &inner.seam
            && seam(&job.path)
        {
            out.status = Status::Unreadable;
            return;
        }
        if !guesses_hold(&job.path, job.check, inner.root_dev) {
            out.status = Status::Gone;
            return;
        }
        list_dir(&job.path, inner.root_dev, &inner.stop, out);
    }));
    if result.is_err() {
        out.reset();
        out.status = Status::Unreadable;
    }
}

/// The last `check` components of `path` (guessed by `request`, not seen
/// in a listing) are real folders: not links, and on the root's device.
fn guesses_hold(path: &Path, check: usize, root_dev: Option<u64>) -> bool {
    path.ancestors().take(check).all(|p| {
        fs::symlink_metadata(p).is_ok_and(|md| {
            !md.file_type().is_symlink() && md.is_dir() && same_device(&md, root_dev)
        })
    })
}

#[cfg(unix)]
fn same_device(md: &fs::Metadata, root_dev: Option<u64>) -> bool {
    use std::os::unix::fs::MetadataExt;
    root_dev.is_none_or(|d| md.dev() == d)
}

#[cfg(not(unix))]
fn same_device(_md: &fs::Metadata, _root_dev: Option<u64>) -> bool {
    true
}

#[cfg(unix)]
fn root_device(root: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    let md = fs::symlink_metadata(root).ok()?;
    if md.file_type().is_symlink() {
        // The root itself may be a link (`/home` on some systems): what is
        // measured is what it points to.
        return fs::metadata(root).ok().map(|m| m.dev());
    }
    Some(md.dev())
}

#[cfg(not(unix))]
fn root_device(_root: &Path) -> Option<u64> {
    None
}

/// One unit: the direct entries of `path`.
fn list_dir(path: &Path, root_dev: Option<u64>, stop: &AtomicBool, out: &mut Listing) {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(e) => {
            out.status = if e.kind() == io::ErrorKind::NotFound {
                Status::Gone
            } else {
                Status::Unreadable
            };
            return;
        }
    };
    for entry in entries {
        if stop.load(atomic::Ordering::Relaxed) {
            out.status = Status::Stopped;
            return;
        }
        let Ok(entry) = entry else {
            out.incomplete = true;
            continue;
        };
        let Ok(kind) = entry.file_type() else {
            out.incomplete = true;
            continue;
        };
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            // On Unix this is the one `lstat`; elsewhere it is free.
            if cfg!(unix)
                && let Ok(md) = entry.metadata()
                && !same_device(&md, root_dev)
            {
                continue;
            }
            out.push_dir(entry.file_name().as_encoded_bytes());
            continue;
        }
        match entry.metadata() {
            Ok(md) => out.add_file(&entry.file_name(), md.len()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(_) => out.incomplete = true,
        }
    }
    out.seal();
}

// ---------------------------------------------------------------------------
// Drive types (§3.2, "Network and removable drives")
// ---------------------------------------------------------------------------

/// Linux filesystem types that are network filesystems (besides `fuse.*`).
const LINUX_NETWORK: [&str; 7] = ["nfs", "nfs4", "cifs", "smb3", "9p", "afs", "ceph"];

/// macOS `mount` types that are network filesystems.
const MAC_NETWORK: [&str; 4] = ["nfs", "smbfs", "afpfs", "webdav"];

/// The Windows rule, on a path already through `std::fs::canonicalize`:
/// `\\?\UNC\…` (a mapped network drive) and `\\server\…` are network;
/// `\\?\C:\…`, `\\?\Volume{…}\…` and `\\.\…` devices are local, as is
/// everything else. `/` counts as `\`.
pub fn network_by_path(path: &str) -> bool {
    let b = path.as_bytes();
    let sep = |c: Option<&u8>| matches!(c, Some(b'\\' | b'/'));
    if !sep(b.first()) || !sep(b.get(1)) {
        return false;
    }
    match b.get(2) {
        Some(b'?' | b'.') if sep(b.get(3)) => {
            let unc = b.get(4..7).is_some_and(|s| s.eq_ignore_ascii_case(b"UNC"));
            unc && sep(b.get(7))
        }
        _ => true,
    }
}

/// The Linux rule: the filesystem type of the mount that holds
/// `mount_point`, from the text of `/proc/self/mountinfo`. The longest
/// mount point that contains it wins (the last line among equals, as a
/// later mount covers an earlier one). `None` when no well-formed line
/// contains it.
pub fn network_by_mountinfo(mountinfo: &str, mount_point: &Path) -> Option<bool> {
    let target = trim_slashes(mount_point.as_os_str().as_encoded_bytes());
    let mut best: Option<(usize, bool)> = None;
    for line in mountinfo.lines() {
        let mut fields = line.split_ascii_whitespace();
        let Some(point) = fields.nth(4) else {
            continue;
        };
        let Some(fstype) = fields.skip_while(|f| *f != "-").nth(1) else {
            continue;
        };
        let point = unescape_octal(point);
        let point = trim_slashes(&point);
        if !contains_path(point, target) {
            continue;
        }
        let network = LINUX_NETWORK.contains(&fstype) || fstype.starts_with("fuse.");
        if best.is_none_or(|(len, _)| point.len() >= len) {
            best = Some((point.len(), network));
        }
    }
    best.map(|(_, network)| network)
}

/// The macOS rule: the type of the volume that holds `mount_point`, from
/// the output of `mount` (lines like `//user@host/share on /Volumes/share
/// (smbfs, nodev, nosuid, mounted by user)`). The longest mount point that
/// contains it wins. `None` when no well-formed line contains it.
pub fn network_by_mount_output(output: &str, mount_point: &Path) -> Option<bool> {
    let target = trim_slashes(mount_point.as_os_str().as_encoded_bytes());
    let mut best: Option<(usize, bool)> = None;
    for line in output.lines() {
        let Some(open) = line.rfind(" (") else {
            continue;
        };
        let (Some(head), Some(tail)) = (line.get(..open), line.get(open.saturating_add(2)..))
        else {
            continue;
        };
        let Some(close) = tail.find(')') else {
            continue;
        };
        let Some(fstype) = tail
            .get(..close)
            .and_then(|inside| inside.split(',').next())
            .map(str::trim)
        else {
            continue;
        };
        let Some(on) = head.find(" on ") else {
            continue;
        };
        let Some(point) = head.get(on.saturating_add(4)..) else {
            continue;
        };
        let point = trim_slashes(point.as_bytes());
        if fstype.is_empty() || !contains_path(point, target) {
            continue;
        }
        let network = MAC_NETWORK.contains(&fstype);
        if best.is_none_or(|(len, _)| point.len() >= len) {
            best = Some((point.len(), network));
        }
    }
    best.map(|(_, network)| network)
}

/// Is the drive or place at `root` on a network? A drive whose type cannot
/// be told counts as one. Blocking (it may read `/proc` or run `mount`):
/// call it off the UI thread.
pub fn is_network_drive(root: &Path) -> bool {
    #[cfg(windows)]
    {
        if root
            .to_str()
            .is_some_and(|s| s.starts_with("\\\\") && network_by_path(s))
        {
            return true;
        }
        match fs::canonicalize(root) {
            Ok(path) => path.to_str().is_none_or(network_by_path),
            Err(_) => true,
        }
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let path = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        match fs::read_to_string("/proc/self/mountinfo") {
            Ok(text) => network_by_mountinfo(&text, &path).unwrap_or(true),
            Err(_) => true,
        }
    }
    #[cfg(target_os = "macos")]
    {
        let path = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        match std::process::Command::new("/sbin/mount").output() {
            Ok(out) if out.status.success() => {
                network_by_mount_output(&String::from_utf8_lossy(&out.stdout), &path)
                    .unwrap_or(true)
            }
            _ => true,
        }
    }
    #[cfg(not(any(
        windows,
        target_os = "linux",
        target_os = "android",
        target_os = "macos"
    )))]
    {
        let _ = root;
        true
    }
}

/// `\ooo` octal escapes (mountinfo writes space, tab, newline and `\` so)
/// decoded; anything else kept as it is.
fn unescape_octal(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0usize;
    while let Some(&c) = b.get(i) {
        if c == b'\\'
            && let Some(digits) = b.get(i.saturating_add(1)..i.saturating_add(4))
            && digits.iter().all(|d| (b'0'..=b'7').contains(d))
        {
            let v = digits.iter().fold(0u32, |v, d| {
                v.wrapping_mul(8) | u32::from(d.wrapping_sub(b'0'))
            });
            if let Ok(v) = u8::try_from(v) {
                out.push(v);
                i = i.saturating_add(4);
                continue;
            }
        }
        out.push(c);
        i = i.saturating_add(1);
    }
    out
}

/// `p` without trailing `/` (but `/` stays `/`).
fn trim_slashes(mut p: &[u8]) -> &[u8] {
    while p.len() > 1
        && let Some((b'/', rest)) = p.split_last().map(|(l, r)| (*l, r))
    {
        p = rest;
    }
    p
}

/// Is `target` the mount point `point` or below it, by whole components?
fn contains_path(point: &[u8], target: &[u8]) -> bool {
    if point.is_empty() {
        return false;
    }
    if point == b"/" {
        return target.first() == Some(&b'/');
    }
    target == point || (target.starts_with(point) && target.get(point.len()) == Some(&b'/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicU32, AtomicUsize};

    // --- helpers -----------------------------------------------------------

    /// A uniquely named temp folder, removed (best effort) on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, atomic::Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("disk-atlas-hostsizes-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn put(path: &Path, bytes: usize) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, vec![b'x'; bytes]).unwrap();
    }

    fn quiet() -> Box<dyn Fn() + Send + Sync> {
        Box::new(|| {})
    }

    fn config(workers: usize) -> Config {
        Config {
            workers,
            ..Config::standard()
        }
    }

    fn sizes(root: &Path) -> HostSizes {
        HostSizes::new(root.to_path_buf(), quiet())
    }

    fn done(state: SizeState) -> bool {
        matches!(state, SizeState::Exact(_) | SizeState::AtLeast(_))
    }

    /// Polls until `dir` is complete (or 20 s pass).
    fn wait_done(s: &HostSizes, dir: &Path) -> SizeState {
        let start = Instant::now();
        loop {
            let st = s.state(dir);
            if done(st) {
                return st;
            }
            assert!(
                start.elapsed() < Duration::from_secs(20),
                "{} never completed: {st:?}",
                dir.display()
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn exact(state: SizeState) -> Totals {
        match state {
            SizeState::Exact(t) => t,
            other => panic!("expected Exact, got {other:?}"),
        }
    }

    /// A plain recursive walk: links skipped, files summed.
    #[derive(Default, Debug, PartialEq)]
    struct Reference {
        totals: Totals,
        classes: [u64; CLASS_SLOTS],
    }

    fn reference(dir: &Path) -> Reference {
        let mut r = Reference::default();
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let md = fs::symlink_metadata(entry.path()).unwrap();
            if md.file_type().is_symlink() {
                continue;
            }
            if md.is_dir() {
                let sub = reference(&entry.path());
                r.totals.bytes += sub.totals.bytes;
                r.totals.files += sub.totals.files;
                r.totals.folders += sub.totals.folders + 1;
                for (a, b) in r.classes.iter_mut().zip(sub.classes) {
                    *a += b;
                }
            } else {
                let name = entry.file_name();
                let class = classify(&name.to_string_lossy(), None);
                r.totals.bytes += md.len();
                r.totals.files += 1;
                r.classes[usize::from(class.bits())] += md.len();
            }
        }
        r
    }

    /// Own files of `dir` by the reference walk.
    fn reference_own(dir: &Path) -> Totals {
        let mut t = Totals::default();
        for entry in fs::read_dir(dir).unwrap() {
            let md = fs::symlink_metadata(entry.unwrap().path()).unwrap();
            if !md.is_dir() && !md.file_type().is_symlink() {
                t.bytes += md.len();
                t.files += 1;
            }
        }
        t
    }

    /// Nested folders, empty folders, zero-byte files, several classes.
    fn sample_tree(root: &Path) {
        put(&root.join("readme.txt"), 120);
        put(&root.join("empty.bin"), 0);
        put(&root.join("photos/a.jpg"), 3_000);
        put(&root.join("photos/b.png"), 1_500);
        put(&root.join("photos/raw/c.tiff"), 7_000);
        put(&root.join("photos/raw/notes.md"), 40);
        put(&root.join("music/song.mp3"), 4_000);
        put(&root.join("music/album/track.flac"), 9_000);
        put(&root.join("music/album/zero.mp3"), 0);
        put(&root.join("code/src/main.rs"), 512);
        put(&root.join("code/src/lib.rs"), 256);
        put(&root.join("code/Cargo.toml"), 64);
        fs::create_dir_all(root.join("empty")).unwrap();
        fs::create_dir_all(root.join("code/target/debug/deps")).unwrap();
        put(&root.join("archive.zip"), 2_048);
    }

    // --- layout --------------------------------------------------------------

    #[test]
    fn node_is_56_bytes() {
        assert_eq!(std::mem::size_of::<Node>(), 56);
    }

    #[test]
    fn host_sizes_is_send_and_sync() {
        fn check<T: Send + Sync>() {}
        check::<HostSizes>();
    }

    #[test]
    fn size_workers_is_clamped() {
        let n = size_workers();
        assert!((1..=4).contains(&n));
    }

    #[test]
    fn class_entries_round_trip() {
        let mut st = State::new(16, 8);
        let cases: [&[(usize, u64)]; 5] = [
            &[],
            &[(3, 77)],
            &[(2, 10), (9, 5)],
            &[(0, 1), (4, 2), (11, 3)],
            &[(2, TAG_UNIT + 5), (6, 9), (7, 1)],
        ];
        for case in cases {
            let mut by = [0u64; CLASS_SLOTS];
            for &(c, v) in case {
                by[c] = v;
            }
            let own: u64 = by.iter().sum();
            let r = st.store_classes(&by);
            let n = Node {
                own_bytes: own,
                classes: r,
                ..Node::default()
            };
            assert_eq!(st.decode_classes(&n), by, "{case:?}");
            let expected_words = match case.len() {
                0 | 1 => 0,
                k if case[0].1 >= TAG_UNIT => k,
                k => k - 1,
            };
            assert_eq!(st.class_words(&n), expected_words, "{case:?}");
        }
    }

    #[test]
    fn names_round_trip() {
        let mut names = Vec::new();
        let long = vec![b'a'; 700];
        let a = push_name(&mut names, b"short").unwrap();
        let b = push_name(&mut names, &long).unwrap();
        assert_eq!(name_entry(&names, a), (&b"short"[..], 6));
        assert_eq!(name_entry(&names, b), (&long[..], 702));
        assert!(push_name(&mut names, &vec![b'a'; MAX_NAME_BYTES + 1]).is_none());
        // An offset past the end, or a damaged length, gives an empty name.
        assert_eq!(name_entry(&names, 1_000_000).0, b"");
        assert_eq!(name_entry(&[0x85, 0x01, b'x'], 0).0, b"");
        assert_eq!(run_at(&[5, 1], 0), &[] as &[u32]);
        assert_eq!(run_at(&[2, 7, 8], 0), &[7, 8]);
    }

    #[test]
    fn wtf8_decodes_surrogates_and_damage() {
        let wide: Vec<u16> = "aé€😀".encode_utf16().collect();
        assert_eq!(wtf8_to_wide("aé€😀".as_bytes()), wide);
        // An unpaired surrogate as WTF-8 (ED A0 80 = U+D800).
        assert_eq!(wtf8_to_wide(&[0xED, 0xA0, 0x80]), vec![0xD800]);
        // Truncated and invalid bytes do not panic.
        assert_eq!(wtf8_to_wide(&[0xE2, 0x82]), vec![0xFFFD]);
        assert_eq!(wtf8_to_wide(&[0xE2, b'a']), vec![0xFFFD, u16::from(b'a')]);
        assert_eq!(wtf8_to_wide(&[0xFF]), vec![0xFFFD]);
    }

    // --- measuring -----------------------------------------------------------

    #[test]
    fn totals_match_a_plain_walk() {
        let tmp = TempDir::new();
        sample_tree(tmp.path());
        let s = sizes(tmp.path());
        assert_eq!(s.state(tmp.path()), SizeState::Unknown);
        s.request(tmp.path(), Priority::Visible);
        let root = exact(wait_done(&s, tmp.path()));
        let r = reference(tmp.path());
        assert_eq!(root, r.totals);
        assert_eq!(s.classes(tmp.path()).by_class, r.classes);
        assert_eq!(s.measured_bytes(), r.totals.bytes);

        for sub in [
            "photos",
            "photos/raw",
            "music",
            "music/album",
            "code",
            "code/target/debug",
            "empty",
        ] {
            let dir = tmp.path().join(sub);
            let r = reference(&dir);
            assert_eq!(exact(s.state(&dir)), r.totals, "{sub}");
            assert_eq!(s.classes(&dir).by_class, r.classes, "{sub}");
            assert_eq!(s.own(&dir), reference_own(&dir), "{sub}");
            // The O(subtree) sum agrees with the running total for the root.
        }
        let st = lock(&s.inner.state);
        assert_eq!(st.tree_classes, {
            let mut walk = [0u64; CLASS_SLOTS];
            for slot in 0..st.nodes.len {
                let n = st.nodes.get(slot).unwrap();
                if !n.has(F_FREE) {
                    for (a, b) in walk.iter_mut().zip(st.decode_classes(n)) {
                        *a += b;
                    }
                }
            }
            walk
        });
    }

    #[test]
    fn own_and_children_agree_with_the_walk() {
        let tmp = TempDir::new();
        sample_tree(tmp.path());
        let s = sizes(tmp.path());
        s.request(tmp.path(), Priority::Listed);
        wait_done(&s, tmp.path());
        assert_eq!(s.own(tmp.path()), reference_own(tmp.path()));
        let kids = s.children(tmp.path());
        let names: Vec<String> = kids
            .iter()
            .map(|(n, _)| n.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["code", "empty", "music", "photos"]);
        for (name, state) in &kids {
            assert_eq!(exact(*state), reference(&tmp.path().join(name)).totals);
        }
        let empty = tmp.path().join("empty");
        assert_eq!(s.own(&empty), Totals::default());
        assert!(s.children(&empty).is_empty());
        assert!(s.knows(&empty));
        assert!(!s.knows(&tmp.path().join("readme.txt")));
        let h = s.dir(&tmp.path().join("music")).unwrap();
        assert_eq!(s.state_of(h), s.state(&tmp.path().join("music")));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed() {
        let tmp = TempDir::new();
        put(&tmp.path().join("real/big.bin"), 10_000);
        let other = TempDir::new();
        put(&other.path().join("far.bin"), 50_000);
        std::os::unix::fs::symlink(other.path(), tmp.path().join("link")).unwrap();
        std::os::unix::fs::symlink(
            tmp.path().join("real/big.bin"),
            tmp.path().join("file-link"),
        )
        .unwrap();
        let s = sizes(tmp.path());
        s.request(tmp.path(), Priority::Visible);
        let t = exact(wait_done(&s, tmp.path()));
        assert_eq!(
            t,
            Totals {
                bytes: 10_000,
                files: 1,
                folders: 1
            }
        );
        assert!(!s.knows(&tmp.path().join("link")));
        // Asked for directly, the link is checked and not entered.
        s.request(&tmp.path().join("link"), Priority::Visible);
        let t = exact(wait_done(&s, tmp.path()));
        assert_eq!(t.bytes, 10_000);
        assert!(!s.knows(&tmp.path().join("link")));
    }

    #[cfg(windows)]
    #[test]
    fn symlinks_are_not_followed() {
        let tmp = TempDir::new();
        put(&tmp.path().join("real/big.bin"), 10_000);
        let other = TempDir::new();
        put(&other.path().join("far.bin"), 50_000);
        if let Err(e) = std::os::windows::fs::symlink_dir(other.path(), tmp.path().join("link")) {
            #[allow(clippy::print_stdout)]
            {
                println!("skipped: cannot create a directory symlink ({e})");
            }
            return;
        }
        let s = sizes(tmp.path());
        s.request(tmp.path(), Priority::Visible);
        let t = exact(wait_done(&s, tmp.path()));
        assert_eq!(
            t,
            Totals {
                bytes: 10_000,
                files: 1,
                folders: 1
            }
        );
        assert!(!s.knows(&tmp.path().join("link")));
        s.request(&tmp.path().join("link"), Priority::Visible);
        assert_eq!(exact(wait_done(&s, tmp.path())).bytes, 10_000);
        assert!(!s.knows(&tmp.path().join("link")));
    }

    #[cfg(windows)]
    #[test]
    fn junctions_are_not_followed() {
        let tmp = TempDir::new();
        put(&tmp.path().join("real/big.bin"), 10_000);
        let other = TempDir::new();
        put(&other.path().join("far.bin"), 50_000);
        let made = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(tmp.path().join("junction"))
            .arg(other.path())
            .output();
        if !made.as_ref().is_ok_and(|o| o.status.success()) {
            #[allow(clippy::print_stdout)]
            {
                println!("skipped: cannot create a junction ({made:?})");
            }
            return;
        }
        assert!(tmp.path().join("junction/far.bin").exists());
        let s = sizes(tmp.path());
        s.request(tmp.path(), Priority::Visible);
        let t = exact(wait_done(&s, tmp.path()));
        assert_eq!(
            t,
            Totals {
                bytes: 10_000,
                files: 1,
                folders: 1
            }
        );
        assert!(!s.knows(&tmp.path().join("junction")));
        // Removing the junction must not touch its target.
        fs::remove_dir(tmp.path().join("junction")).unwrap();
        assert!(other.path().join("far.bin").exists());
    }

    #[test]
    fn unreadable_folder_gives_at_least_up_the_chain() {
        let tmp = TempDir::new();
        sample_tree(tmp.path());
        let blocked = tmp.path().join("photos").join("raw");
        let target = blocked.clone();
        let s = HostSizes::with_config(
            tmp.path().to_path_buf(),
            quiet(),
            Config {
                seam: Some(Box::new(move |p| p == target)),
                ..config(2)
            },
        );
        s.request(tmp.path(), Priority::Visible);
        let root = wait_done(&s, tmp.path());
        let full = reference(tmp.path()).totals;
        let raw = reference(&blocked).totals;
        let expect = Totals {
            bytes: full.bytes - raw.bytes,
            files: full.files - raw.files,
            folders: full.folders,
        };
        assert_eq!(root, SizeState::AtLeast(expect));
        assert!(matches!(
            s.state(&tmp.path().join("photos")),
            SizeState::AtLeast(_)
        ));
        assert_eq!(s.state(&blocked), SizeState::AtLeast(Totals::default()));
        assert_eq!(
            exact(s.state(&tmp.path().join("music"))),
            reference(&tmp.path().join("music")).totals
        );
        let st = lock(&s.inner.state);
        assert_eq!(st.nodes.get(ROOT).unwrap().bad, 1);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_by_permission_gives_at_least() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new();
        put(&tmp.path().join("open/a.txt"), 10);
        put(&tmp.path().join("shut/b.txt"), 20);
        let shut = tmp.path().join("shut");
        fs::set_permissions(&shut, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&shut).is_ok() {
            fs::set_permissions(&shut, fs::Permissions::from_mode(0o755)).unwrap();
            println!("skipped: running as root");
            return;
        }
        let s = sizes(tmp.path());
        s.request(tmp.path(), Priority::Visible);
        let state = wait_done(&s, tmp.path());
        fs::set_permissions(&shut, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            state,
            SizeState::AtLeast(Totals {
                bytes: 10,
                files: 1,
                folders: 2
            })
        );
    }

    #[test]
    fn a_panicking_unit_is_caught_and_the_worker_goes_on() {
        let tmp = TempDir::new();
        put(&tmp.path().join("boom/inner/x.txt"), 100);
        put(&tmp.path().join("fine/y.txt"), 30);
        put(&tmp.path().join("z.txt"), 5);
        let s = HostSizes::with_config(
            tmp.path().to_path_buf(),
            quiet(),
            Config {
                seam: Some(Box::new(|p| {
                    if p.ends_with("boom") {
                        panic!("test panic in a size unit");
                    }
                    false
                })),
                ..config(1)
            },
        );
        s.request(tmp.path(), Priority::Visible);
        let root = wait_done(&s, tmp.path());
        assert_eq!(
            root,
            SizeState::AtLeast(Totals {
                bytes: 35,
                files: 2,
                folders: 2
            })
        );
        assert_eq!(
            s.state(&tmp.path().join("boom")),
            SizeState::AtLeast(Totals::default())
        );
        assert_eq!(
            s.state(&tmp.path().join("fine")),
            SizeState::Exact(Totals {
                bytes: 30,
                files: 1,
                folders: 0
            })
        );
        // The one worker is still there and still works.
        put(&tmp.path().join("fine/new/w.txt"), 7);
        s.invalidate(&tmp.path().join("fine"));
        let fine = exact(wait_done(&s, &tmp.path().join("fine")));
        assert_eq!(
            fine,
            Totals {
                bytes: 37,
                files: 2,
                folders: 1
            }
        );
    }

    /// One worker, held at the root's read while the test queues work.
    fn gated(root: &Path, gate_on: PathBuf) -> (HostSizes, Arc<Mutex<Vec<PathBuf>>>, Arc<Barrier>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(Barrier::new(2));
        let first = Arc::new(AtomicBool::new(true));
        let (log2, gate2) = (Arc::clone(&log), Arc::clone(&gate));
        let s = HostSizes::with_config(
            root.to_path_buf(),
            quiet(),
            Config {
                seam: Some(Box::new(move |p| {
                    lock(&log2).push(p.to_path_buf());
                    if p == gate_on && first.swap(false, atomic::Ordering::SeqCst) {
                        gate2.wait(); // entered
                        gate2.wait(); // released
                    }
                    false
                })),
                ..config(1)
            },
        );
        (s, log, gate)
    }

    #[test]
    fn priorities_reorder_work() {
        let tmp = TempDir::new();
        for top in ["a", "b", "c"] {
            for sub in ["x", "y"] {
                put(&tmp.path().join(top).join(sub).join("f.txt"), 1);
            }
        }
        let (s, log, gate) = gated(tmp.path(), tmp.path().to_path_buf());
        s.request(tmp.path(), Priority::Background);
        gate.wait(); // the worker is inside the root's unit
        s.request(&tmp.path().join("c"), Priority::Visible);
        s.request(&tmp.path().join("a"), Priority::Listed);
        // Asking again at a lower priority changes nothing.
        s.request(&tmp.path().join("c"), Priority::Background);
        gate.wait();
        wait_done(&s, tmp.path());
        let log = lock(&log).clone();
        let group = |p: &PathBuf| -> u8 {
            match p.strip_prefix(tmp.path()).unwrap().components().next() {
                None => 0,
                Some(c) => match c.as_os_str().to_str().unwrap() {
                    "c" => 1,
                    "a" => 2,
                    "b" => 3,
                    _ => 9,
                },
            }
        };
        let groups: Vec<u8> = log.iter().map(group).collect();
        assert_eq!(groups.len(), 10, "{log:?}");
        let mut sorted = groups.clone();
        sorted.sort_unstable();
        assert_eq!(
            groups, sorted,
            "visible, then listed, then background: {log:?}"
        );
        // Within one priority, depth-first: c, then c's two, before a.
        assert_eq!(&groups[..4], &[0, 1, 1, 1]);
        assert_eq!(exact(s.state(tmp.path())), reference(tmp.path()).totals);
    }

    #[test]
    fn refocus_keeps_what_was_measured_and_drops_background_work() {
        let tmp = TempDir::new();
        for top in ["drop", "keep", "listed"] {
            for sub in ["x", "y"] {
                put(&tmp.path().join(top).join(sub).join("f.txt"), 10);
            }
        }
        let keep = tmp.path().join("keep");
        // Children are queued in name order, so the last ("listed") is read
        // first and "keep" next: hold the worker there.
        let (s, log, gate) = gated(tmp.path(), keep.clone());
        s.request(tmp.path(), Priority::Background);
        gate.wait();
        s.request(&tmp.path().join("listed"), Priority::Listed);
        s.refocus(std::slice::from_ref(&keep));
        gate.wait();
        let k = exact(wait_done(&s, &keep));
        assert_eq!(k, reference(&keep).totals);
        assert_eq!(
            exact(wait_done(&s, &tmp.path().join("listed"))),
            reference(&tmp.path().join("listed")).totals
        );
        // "drop" was queued at Background outside `keep`: never read.
        let drop_dir = tmp.path().join("drop");
        let wait_idle = Instant::now();
        while lock(&s.inner.state).stacks.iter().any(|q| !q.is_empty())
            && wait_idle.elapsed() < Duration::from_secs(5)
        {
            thread::sleep(Duration::from_millis(1));
        }
        thread::sleep(Duration::from_millis(20));
        assert_eq!(s.state(&drop_dir), SizeState::Unknown);
        assert!(!lock(&log).iter().any(|p| p.starts_with(&drop_dir)));
        assert!(matches!(s.state(tmp.path()), SizeState::Running(_)));
        // What was measured stays through another refocus.
        s.refocus(&[]);
        assert_eq!(s.state(&keep), SizeState::Exact(k));
        // Asked again, the rest is measured.
        s.request(tmp.path(), Priority::Background);
        assert_eq!(
            exact(wait_done(&s, tmp.path())),
            reference(tmp.path()).totals
        );
    }

    #[test]
    fn request_reaches_ahead_of_the_walk() {
        let tmp = TempDir::new();
        sample_tree(tmp.path());
        let s = sizes(tmp.path());
        let raw = tmp.path().join("photos/raw");
        s.request(&raw, Priority::Visible);
        assert_eq!(exact(wait_done(&s, &raw)), reference(&raw).totals);
        assert!(s.knows(&tmp.path().join("photos")));
        assert!(matches!(
            s.state(&tmp.path().join("photos")),
            SizeState::Running(_)
        ));
        assert!(matches!(s.state(tmp.path()), SizeState::Running(_)));
        // Outside the root, `..`, and a folder that is not there.
        s.request(
            Path::new("/definitely/not/below/the/root"),
            Priority::Visible,
        );
        s.request(&tmp.path().join("photos/../music"), Priority::Visible);
        assert!(!s.knows(&tmp.path().join("music")));
        s.request(&tmp.path().join("missing/deeper"), Priority::Visible);
        // The whole tree: the guess is matched by name, not duplicated.
        s.request(tmp.path(), Priority::Visible);
        assert_eq!(
            exact(wait_done(&s, tmp.path())),
            reference(tmp.path()).totals
        );
        let photos: Vec<OsString> = s
            .children(&tmp.path().join("photos"))
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(photos, [OsString::from("raw")]);
        assert!(!s.knows(&tmp.path().join("missing/deeper")));
    }

    // --- changes ---------------------------------------------------------------

    #[test]
    fn invalidate_applies_differences_to_every_ancestor() {
        let tmp = TempDir::new();
        put(&tmp.path().join("f.txt"), 10);
        put(&tmp.path().join("sub/g.txt"), 5);
        put(&tmp.path().join("sub/deep/h.txt"), 3);
        let s = sizes(tmp.path());
        s.request(tmp.path(), Priority::Visible);
        wait_done(&s, tmp.path());
        let sub = tmp.path().join("sub");
        let deep = sub.join("deep");
        let check = |what: &str| {
            for dir in [tmp.path(), sub.as_path(), deep.as_path()] {
                if dir.exists() {
                    assert_eq!(
                        exact(s.state(dir)),
                        reference(dir).totals,
                        "{what}: {dir:?}"
                    );
                }
            }
            assert_eq!(
                s.classes(tmp.path()).by_class,
                reference(tmp.path()).classes,
                "{what}"
            );
        };
        check("start");

        put(&deep.join("new.png"), 7);
        s.invalidate(&deep);
        assert_eq!(exact(s.state(tmp.path())).bytes, 25);
        assert_eq!(exact(s.state(&sub)).files, 3);
        assert_eq!(s.classes(tmp.path()).of(KindClass::Image), 7);
        check("added");

        put(&sub.join("g.txt"), 15);
        s.invalidate(&sub);
        assert_eq!(exact(s.state(tmp.path())).bytes, 35);
        check("grown");

        fs::remove_file(tmp.path().join("f.txt")).unwrap();
        s.invalidate(tmp.path());
        assert_eq!(
            exact(s.state(tmp.path())),
            Totals {
                bytes: 25,
                files: 3,
                folders: 2
            }
        );
        check("deleted file");

        let handle = s.dir(&deep).unwrap();
        fs::remove_dir_all(&deep).unwrap();
        s.invalidate(&sub);
        assert_eq!(s.state_of(handle), SizeState::Unknown);
        assert!(!s.knows(&deep));
        assert_eq!(
            exact(s.state(tmp.path())),
            Totals {
                bytes: 15,
                files: 1,
                folders: 1
            }
        );
        check("deleted folder");

        // The freed slot is reused; the old handle still answers Unknown.
        put(&sub.join("other/o.txt"), 2);
        s.invalidate(&sub);
        let other = exact(wait_done(&s, &sub.join("other")));
        assert_eq!(other.bytes, 2);
        assert_eq!(s.state_of(handle), SizeState::Unknown);
        wait_done(&s, tmp.path());
        check("new folder");

        // Unknown and never-read folders are ignored.
        s.invalidate(&tmp.path().join("nowhere"));
        s.invalidate(Path::new("/elsewhere"));
    }

    #[test]
    fn a_renamed_folder_is_measured_again_under_its_new_name() {
        let tmp = TempDir::new();
        put(&tmp.path().join("old/a/x.txt"), 40);
        put(&tmp.path().join("old/b.txt"), 2);
        let s = sizes(tmp.path());
        s.request(tmp.path(), Priority::Visible);
        wait_done(&s, tmp.path());
        let before = exact(s.state(&tmp.path().join("old")));
        fs::rename(tmp.path().join("old"), tmp.path().join("new")).unwrap();
        s.invalidate(tmp.path());
        assert!(!s.knows(&tmp.path().join("old")));
        assert_eq!(s.state(&tmp.path().join("old")), SizeState::Unknown);
        assert_eq!(exact(wait_done(&s, &tmp.path().join("new"))), before);
        assert_eq!(
            exact(wait_done(&s, tmp.path())),
            reference(tmp.path()).totals
        );
    }

    #[test]
    fn invalidate_all_measures_again_and_keeps_old_numbers_meanwhile() {
        let tmp = TempDir::new();
        sample_tree(tmp.path());
        let music = tmp.path().join("music");
        let (s, _log, gate) = gated(tmp.path(), music.clone());
        s.request(tmp.path(), Priority::Visible);
        gate.wait();
        gate.wait();
        let old = exact(wait_done(&s, tmp.path()));
        put(&music.join("album/more.ogg"), 1_000);
        fs::remove_file(tmp.path().join("readme.txt")).unwrap();
        s.invalidate_all();
        // Old numbers, Running, until the re-reads land.
        match s.state(tmp.path()) {
            SizeState::Running(t) => assert!(t.bytes <= old.bytes + 1_000),
            SizeState::Exact(_) => {}
            other => panic!("{other:?}"),
        }
        assert_eq!(
            exact(wait_done(&s, tmp.path())),
            reference(tmp.path()).totals
        );
        assert_eq!(
            s.classes(tmp.path()).by_class,
            reference(tmp.path()).classes
        );
    }

    // --- limits ----------------------------------------------------------------

    #[test]
    fn the_folder_limit_gives_at_least() {
        let tmp = TempDir::new();
        for name in ["a", "b", "c", "d", "e"] {
            put(&tmp.path().join(name).join("f.txt"), 1);
        }
        let s = HostSizes::with_config(
            tmp.path().to_path_buf(),
            quiet(),
            Config {
                max_folders: 4,
                ..config(1)
            },
        );
        s.request(tmp.path(), Priority::Visible);
        let state = wait_done(&s, tmp.path());
        assert_eq!(
            state,
            SizeState::AtLeast(Totals {
                bytes: 3,
                files: 3,
                folders: 3
            })
        );
        assert_eq!(s.children(tmp.path()).len(), 3);
    }

    #[test]
    fn the_depth_limit_gives_at_least() {
        let tmp = TempDir::new();
        put(&tmp.path().join("a/a.txt"), 1);
        put(&tmp.path().join("a/b/b.txt"), 2);
        put(&tmp.path().join("a/b/c/c.txt"), 4);
        put(&tmp.path().join("a/b/c/d/d.txt"), 8);
        let s = HostSizes::with_config(
            tmp.path().to_path_buf(),
            quiet(),
            Config {
                max_depth: 2,
                ..config(1)
            },
        );
        s.request(tmp.path(), Priority::Visible);
        let state = wait_done(&s, tmp.path());
        assert_eq!(
            state,
            SizeState::AtLeast(Totals {
                bytes: 3,
                files: 2,
                folders: 2
            })
        );
        assert_eq!(
            s.state(&tmp.path().join("a/b")),
            SizeState::AtLeast(Totals {
                bytes: 2,
                files: 1,
                folders: 0
            })
        );
        assert!(!s.knows(&tmp.path().join("a/b/c")));
        // A request past the limit is ignored.
        s.request(&tmp.path().join("a/b/c/d"), Priority::Visible);
        assert!(!s.knows(&tmp.path().join("a/b/c")));
    }

    // --- threads -----------------------------------------------------------------

    #[test]
    fn shutdown_joins_and_drop_joins() {
        let tmp = TempDir::new();
        sample_tree(tmp.path());
        let s = sizes(tmp.path());
        s.request(tmp.path(), Priority::Visible);
        let before = wait_done(&s, tmp.path());
        let weak = Arc::downgrade(&s.inner);
        assert!(weak.strong_count() > 1, "workers hold the inner state");
        s.shutdown();
        assert_eq!(weak.strong_count(), 1, "every worker has exited");
        s.shutdown();
        s.request(&tmp.path().join("music"), Priority::Visible);
        put(&tmp.path().join("later.txt"), 5);
        s.invalidate(tmp.path());
        s.invalidate_all();
        assert_eq!(s.state(tmp.path()), before);
        assert!(lock(&s.workers).handles.is_empty());
        drop(s);
        assert!(weak.upgrade().is_none());

        // Dropped while still measuring, without shutdown.
        let s = sizes(tmp.path());
        s.request(tmp.path(), Priority::Background);
        let weak = Arc::downgrade(&s.inner);
        drop(s);
        assert!(weak.upgrade().is_none());

        // Never started.
        let s = sizes(tmp.path());
        drop(s);
    }

    #[test]
    fn shutdown_interrupts_a_unit() {
        let tmp = TempDir::new();
        put(&tmp.path().join("a/f.txt"), 1);
        let (s, _log, gate) = gated(tmp.path(), tmp.path().join("a"));
        s.request(tmp.path(), Priority::Visible);
        gate.wait();
        s.inner.stop.store(true, atomic::Ordering::SeqCst);
        gate.wait();
        s.shutdown();
        assert!(matches!(s.state(tmp.path()), SizeState::Running(_)));
    }

    #[test]
    fn on_change_is_throttled_and_the_last_change_is_delivered() {
        let tmp = TempDir::new();
        for i in 0..60 {
            put(&tmp.path().join(format!("d{i:02}")).join("f.txt"), 3);
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = Arc::clone(&calls);
        let s = HostSizes::with_config(
            tmp.path().to_path_buf(),
            Box::new(move || {
                calls2.fetch_add(1, atomic::Ordering::SeqCst);
            }),
            Config {
                // About 5 ms per folder: the walk takes ~300 ms.
                seam: Some(Box::new(|_| {
                    thread::sleep(Duration::from_millis(5));
                    false
                })),
                ..config(1)
            },
        );
        let start = Instant::now();
        s.request(tmp.path(), Priority::Visible);
        wait_done(&s, tmp.path());
        let finished = start.elapsed();
        // The last change reaches on_change soon after the walk ends.
        let delivered = loop {
            let (changed, notifies) = {
                let st = lock(&s.inner.state);
                (st.changed, st.notifies)
            };
            let n = calls.load(atomic::Ordering::SeqCst);
            if !changed && u64::try_from(n).unwrap() == notifies {
                break start.elapsed();
            }
            assert!(
                start.elapsed() < finished + Duration::from_secs(2),
                "never delivered"
            );
            thread::sleep(Duration::from_millis(1));
        };
        let n = calls.load(atomic::Ordering::SeqCst);
        assert!(n >= 2, "{n} calls in {finished:?}");
        let st = lock(&s.inner.state);
        for pair in st.notify_log.windows(2) {
            assert!(pair[1] - pair[0] >= NOTIFY_INTERVAL);
        }
        let most = delivered.as_millis() / NOTIFY_INTERVAL.as_millis() + 1;
        assert!(n as u128 <= most, "{n} calls in {delivered:?}");
        let last = *st.notify_log.last().unwrap();
        assert!(
            last >= start + finished.saturating_sub(Duration::from_millis(1)) - NOTIFY_INTERVAL
        );
    }

    #[test]
    fn a_panicking_on_change_does_not_stop_the_worker() {
        let tmp = TempDir::new();
        for i in 0..5 {
            put(&tmp.path().join(format!("d{i}")).join("f.txt"), 1);
        }
        let s = HostSizes::with_config(
            tmp.path().to_path_buf(),
            Box::new(|| panic!("test panic in on_change")),
            config(1),
        );
        s.request(tmp.path(), Priority::Visible);
        assert_eq!(exact(wait_done(&s, tmp.path())).files, 5);
        thread::sleep(NOTIFY_INTERVAL * 2);
        put(&tmp.path().join("d0/more/g.txt"), 1);
        s.invalidate(&tmp.path().join("d0"));
        assert_eq!(exact(wait_done(&s, tmp.path())).files, 6);
    }

    // --- memory ----------------------------------------------------------------

    /// Fills the tree through the workers' own code (`pop_slot` and
    /// `finish`), with simulated listings: names of 8 to 24 bytes, ten
    /// sub-folders per folder down to depth 6, files of one to three kinds.
    fn fill_synthetic(s: &HostSizes, target: u32) {
        let mut st = lock(&s.inner.state);
        st.queue_subtree(ROOT, Priority::Background);
        let mut rng = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let kinds = [
            KindClass::Document,
            KindClass::Image,
            KindClass::Code,
            KindClass::Audio,
            KindClass::Other,
        ];
        let mut created = 1u32;
        let mut listing = Listing::default();
        let mut name = Vec::new();
        while let Some((slot, generation)) = st.pop_slot() {
            listing.reset();
            let k = 1 + (next() % 3) as usize;
            for i in 0..k {
                let class = kinds[(next() as usize + i) % kinds.len()];
                let bytes = 1 + next() % 1_000_000;
                listing.own_bytes += bytes;
                listing.own_files += 1 + (next() % 20) as u32;
                listing.classes[usize::from(class.bits())] += bytes;
            }
            let depth = st.depth(slot).unwrap();
            if depth < 6 && created < target {
                let n = 10.min(target - created);
                for _ in 0..n {
                    name.clear();
                    let len = 8 + (next() % 17) as usize;
                    for _ in 0..len {
                        name.push(b'a' + (next() % 26) as u8);
                    }
                    listing.push_dir(&name);
                }
                created += n;
            }
            listing.seal();
            let job = Job {
                slot,
                generation,
                path: PathBuf::new(),
                check: 0,
            };
            st.finish(&job, &listing, false);
        }
    }

    fn synthetic_report(folders: u32) -> (usize, f64, SizeState) {
        let s = HostSizes::with_config(PathBuf::from("synthetic"), quiet(), config(0));
        fill_synthetic(&s, folders);
        let heap = s.heap_bytes();
        let st = lock(&s.inner.state);
        let live = st.live;
        let state = st.nodes.get(ROOT).unwrap().state();
        (heap, heap as f64 / f64::from(live), state)
    }

    #[test]
    fn memory_stays_within_96_bytes_per_folder() {
        let (heap, per, state) = synthetic_report(100_000);
        let SizeState::Exact(t) = state else {
            panic!("{state:?}");
        };
        assert!(t.folders >= 99_000, "{t:?}");
        assert!(per <= 96.0, "{per:.1} bytes per folder ({heap} bytes)");
    }

    #[test]
    #[ignore = "timing; run in release with --ignored --nocapture"]
    #[allow(clippy::print_stdout)]
    fn host_sizes_heap_1m_folders() {
        let started = Instant::now();
        let (heap, per, state) = synthetic_report(1_000_000);
        let elapsed = started.elapsed();
        let folders = match state {
            SizeState::Exact(t) => t.folders + 1,
            other => panic!("{other:?}"),
        };
        println!(
            "host_sizes_heap_1m_folders: heap_bytes {heap} ({:.1} MiB) for {folders} folders, {per:.1} bytes per folder; budget 96 MiB and 96 bytes per folder: {}; filled in {:.0} ms",
            heap as f64 / (1024.0 * 1024.0),
            if heap <= 96 * 1024 * 1024 && per <= 96.0 {
                "met"
            } else {
                "MISSED"
            },
            elapsed.as_secs_f64() * 1e3,
        );
    }

    #[test]
    #[ignore = "timing; run in release with --ignored --nocapture"]
    #[allow(clippy::print_stdout)]
    fn host_sizes_walk_500k_files() {
        let tmp = TempDir::new();
        let created = Instant::now();
        thread::scope(|scope| {
            for t in 0..8usize {
                let root = tmp.path();
                scope.spawn(move || {
                    for folder in (t..5_000).step_by(8) {
                        let dir = root
                            .join(format!("g{:02}", folder / 100))
                            .join(format!("d{folder:04}"));
                        fs::create_dir_all(&dir).unwrap();
                        for file in 0..100 {
                            fs::write(dir.join(format!("f{file:03}.txt")), [b'x'; 16]).unwrap();
                        }
                    }
                });
            }
        });
        println!(
            "host_sizes_walk_500k_files: tree of 500,000 files in 5,050 folders made in {:.1} s",
            created.elapsed().as_secs_f64()
        );
        let workers = size_workers();
        let mut times = Vec::new();
        for run in 0..4 {
            let s = sizes(tmp.path());
            let started = Instant::now();
            s.request(tmp.path(), Priority::Visible);
            let t = exact(wait_done(&s, tmp.path()));
            let ms = started.elapsed().as_secs_f64() * 1e3;
            assert_eq!(t.files, 500_000);
            if run > 0 {
                times.push(ms);
            }
        }
        times.sort_by(f64::total_cmp);
        println!(
            "host_sizes_walk_500k_files: request(root, Visible) to Exact, warm cache, {workers} workers: median {:.0} ms of 3 (min {:.0}, max {:.0}), budget 3000 ms: {}",
            times[1],
            times[0],
            times[2],
            if times[1] <= 3_000.0 { "met" } else { "MISSED" },
        );
    }

    // --- drive types -------------------------------------------------------------

    #[test]
    fn windows_paths() {
        assert!(network_by_path(r"\\?\UNC\server\share\"));
        assert!(network_by_path(r"\\?\unc\server\share"));
        assert!(network_by_path(r"\\server\share\folder"));
        assert!(network_by_path("//server/share"));
        assert!(network_by_path(r"\\.\UNC\server\share"));
        assert!(!network_by_path(r"\\?\C:\"));
        assert!(!network_by_path(r"\\?\C:\Users"));
        assert!(!network_by_path(
            r"\\?\Volume{0b1c2d3e-0000-0000-0000-100000000000}\"
        ));
        assert!(!network_by_path(r"\\.\PhysicalDrive0"));
        assert!(!network_by_path(r"C:\"));
        assert!(!network_by_path(r"C:\Users\Someone"));
        assert!(!network_by_path(""));
        assert!(!network_by_path(r"\"));
        assert!(!network_by_path(r"\\?\UNCX"));
    }

    const MOUNTINFO: &str = "\
22 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw,errors=remount-ro
23 22 0:21 / /proc rw,nosuid,nodev,noexec,relatime shared:12 - proc proc rw
40 22 0:45 / /mnt/nfs rw,relatime shared:30 - nfs4 server:/export rw,vers=4.2
41 22 0:46 / /mnt/win rw,relatime shared:31 - cifs //host/share rw,vers=3.1.1
42 22 0:47 / /home/u/remote rw,nosuid,nodev,relatime shared:32 - fuse.sshfs u@host:/ rw,user_id=1000
43 22 259:3 / /media/My\\040Disk rw,relatime shared:33 - ext4 /dev/sdb1 rw
44 40 259:4 / /mnt/nfs/local rw,relatime shared:34 - xfs /dev/sdc1 rw
45 22 0:48 / /mnt/a rw shared:35 master:2 - 9p tag rw
this line is broken
46 22 0:49
47 22 0:50 / /mnt/nosep rw shared:36 ext4 /dev/x rw
";

    #[test]
    fn linux_mountinfo() {
        let net = |p: &str| network_by_mountinfo(MOUNTINFO, Path::new(p));
        assert_eq!(net("/"), Some(false));
        assert_eq!(net("/home/u"), Some(false));
        assert_eq!(net("/mnt/nfs"), Some(true));
        assert_eq!(net("/mnt/nfs/"), Some(true));
        assert_eq!(net("/mnt/nfs/deep/er"), Some(true));
        assert_eq!(net("/mnt/nfsx"), Some(false), "not a component prefix");
        assert_eq!(net("/mnt/nfs/local"), Some(false), "nested mount wins");
        assert_eq!(net("/mnt/nfs/local/x"), Some(false));
        assert_eq!(net("/mnt/win/docs"), Some(true));
        assert_eq!(net("/home/u/remote/x"), Some(true));
        assert_eq!(net("/media/My Disk"), Some(false));
        assert_eq!(net("/media/My Disk/sub"), Some(false));
        assert_eq!(net("/mnt/a"), Some(true));
        // The line without a separator is skipped: the root matches instead.
        assert_eq!(net("/mnt/nosep"), Some(false));
        assert_eq!(network_by_mountinfo("", Path::new("/")), None);
        assert_eq!(
            network_by_mountinfo("garbage\n\n- - -\n", Path::new("/")),
            None
        );
        assert_eq!(
            network_by_mountinfo("1 2 3 4 relative rw - nfs x y", Path::new("/x")),
            None
        );
        assert_eq!(unescape_octal(r"a\040b\134c\0"), b"a b\\c\\0");
        assert_eq!(unescape_octal(r"\777"), b"\\777");
        assert_eq!(unescape_octal("\\"), b"\\");
    }

    const MOUNT_OUTPUT: &str = "\
/dev/disk3s1s1 on / (apfs, sealed, local, read-only, journaled)
devfs on /dev (devfs, local, nobrowse)
/dev/disk3s5 on /System/Volumes/Data (apfs, local, journaled, nobrowse, protect)
//alice@nas.local/Media on /Volumes/Media (smbfs, nodev, nosuid, mounted by alice)
afp_0TQ0uN0R2w0;0VmRrO1mVE;0 on /Volumes/Old (afpfs, nodev, nosuid, mounted by alice)
https://dav.example.com/files on /Volumes/files (webdav, nodev, noexec, nosuid, mounted by alice)
/dev/disk5s1 on /Volumes/My Disk (apfs, local, nodev, nosuid, journaled, noowners)
nas:/export on /Volumes/Media/nfs (nfs, nodev, nosuid)
broken line without parens
also broken on /Volumes/X (
";

    #[test]
    fn macos_mount_output() {
        let net = |p: &str| network_by_mount_output(MOUNT_OUTPUT, Path::new(p));
        assert_eq!(net("/"), Some(false));
        assert_eq!(net("/Users/alice"), Some(false));
        assert_eq!(net("/System/Volumes/Data/x"), Some(false));
        assert_eq!(net("/Volumes/Media"), Some(true));
        assert_eq!(net("/Volumes/Media/Films"), Some(true));
        assert_eq!(net("/Volumes/Old"), Some(true));
        assert_eq!(net("/Volumes/files"), Some(true));
        assert_eq!(net("/Volumes/My Disk"), Some(false));
        assert_eq!(net("/Volumes/My Disk/sub"), Some(false));
        assert_eq!(net("/Volumes/Media/nfs/x"), Some(true));
        assert_eq!(
            net("/Volumes/X"),
            Some(false),
            "broken line skipped, / matches"
        );
        assert_eq!(network_by_mount_output("", Path::new("/")), None);
        assert_eq!(network_by_mount_output("x on y (", Path::new("/")), None);
        assert_eq!(network_by_mount_output("( on ) ()", Path::new("/")), None);
    }

    #[test]
    fn this_computer_has_a_local_temp_folder() {
        // The temp folder is local on every machine the tests run on; if it
        // is not, this says so rather than failing.
        let local = !is_network_drive(&std::env::temp_dir());
        assert!(local || cfg!(not(any(windows, target_os = "linux", target_os = "macos"))));
    }
}
