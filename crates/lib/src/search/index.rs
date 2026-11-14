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

//! The in-memory name index: struct of arrays in depth-first pre-order.
//!
//! One [`Index`] holds every entry of one volume. Entry 0 is the root; every
//! folder is followed directly by its whole subtree, so `parent[i] < i` for
//! every other entry, a parent chain cannot loop, and "inside this folder" is
//! the contiguous range [`Index::subtree`].
//!
//! Per entry the arrays cost 38 bytes (`offsets` 4, `parent` 4, `node` 8,
//! `size` 8, `modified` 8, `rsize` 4, `flags` 2) plus the name twice (`names`
//! and `folded`, each with one separator byte). With 20-byte names that is
//! the 80-byte budget of `performance.md`.
//!
//! Every accessor takes an entry number `i: u32`, answers `None`, empty or
//! `false` for a number out of range, and never panics, whatever the arrays
//! hold.

use std::fmt;
use std::ops::Range;

use rayon::prelude::*;

use crate::fs::{DirEntry, EntryFlags, NodeId, NodeKind, ScanSummary, Timestamp, VolumeIdentity};
use crate::search::fold::fold_into;
use crate::search::kind::KindClass;

/// `size` of a folder or of a file whose size is unknown.
pub const NO_SIZE: u64 = u64::MAX;
/// `modified` when the date is unknown.
pub const NO_TIME: i64 = i64::MIN;
/// `rsize` when the entry has no resource fork (a folder, or a format
/// without forks).
pub const NO_RSIZE: u32 = u32::MAX;
/// Largest stored resource fork size; larger forks are saturated to it.
pub const MAX_RSIZE: u32 = u32::MAX - 1;
/// Most entries an index holds, the root included.
pub const MAX_ENTRIES: u32 = u32::MAX - 1;
/// Most bytes of `names`, separators included, so every offset fits a `u32`.
pub const MAX_NAME_BYTES: u32 = u32::MAX;
/// Most entries in a parent chain that [`Index::crumbs`] and
/// [`Index::folder_path`] follow, the root and the entry included.
pub const MAX_DEPTH: usize = 4096;
/// What a NUL byte inside a name is stored as (SUBSTITUTE). Same length, and
/// the 0 separator stays unique.
pub const NUL_REPLACEMENT: u8 = 0x1A;

/// Entries per piece when `folded` is computed in parallel.
const FOLD_CHUNK_ENTRIES: usize = 65_536;

/// The 16-bit flag word stored per entry.
///
/// | bits  | meaning |
/// |-------|---------|
/// | 0–1   | kind: 0 file, 1 folder, 2 symbolic link, 3 other |
/// | 2     | hidden ([`EntryFlags::HIDDEN`]) |
/// | 3     | compressed ([`EntryFlags::COMPRESSED`]) |
/// | 4     | links to a folder ([`EntryFlags::LINKS_TO_DIRECTORY`]) |
/// | 5     | has a non-empty resource fork |
/// | 6     | a housekeeping item ([`system_item`]) or anything inside one |
/// | 7     | a package: a folder with [`EntryFlags::PACKAGE`] (its own entry only) |
/// | 8–11  | [`KindClass`] ([`KindClass::bits`]), from [`classify_entry`] at build |
/// | 12–14 | Finder label ([`EntryFlags::label`]), 0 = none |
/// | 15    | reserved, 0 |
///
/// Bit 6 is passed down at build: an entry has it when it is a housekeeping
/// item itself or its parent has it, so search and sizes test one bit and
/// never walk up. [`EntryFlags::ALIAS`] has no bit and is not kept.
///
/// The query engine tests the raw words with the `u16` masks below; the
/// methods are for everything else.
///
/// [`system_item`]: crate::mac::conventions::system_item
/// [`classify_entry`]: crate::search::kind::classify_entry
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct IndexFlags(u16);

impl IndexFlags {
    /// Mask of the kind bits.
    pub const KIND_MASK: u16 = 0b11;
    /// Kind value of a regular file.
    pub const KIND_FILE: u16 = 0;
    /// Kind value of a folder.
    pub const KIND_FOLDER: u16 = 1;
    /// Kind value of a symbolic link.
    pub const KIND_SYMLINK: u16 = 2;
    /// Kind value of anything else.
    pub const KIND_OTHER: u16 = 3;
    /// The entry is hidden.
    pub const HIDDEN: u16 = 1 << 2;
    /// The file's content is stored compressed.
    pub const COMPRESSED: u16 = 1 << 3;
    /// The entry is a symbolic link to a folder.
    pub const LINKS_TO_FOLDER: u16 = 1 << 4;
    /// The entry has a resource fork of at least one byte.
    pub const RESOURCE_FORK: u16 = 1 << 5;
    /// The entry is a housekeeping item or inside one.
    pub const SYSTEM_ITEM: u16 = 1 << 6;
    /// The entry is a folder with the Finder's bundle bit: a package.
    pub const PACKAGE: u16 = 1 << 7;
    /// Shift of the [`KindClass`] bits.
    pub const CLASS_SHIFT: u32 = 8;
    /// Mask of the [`KindClass`] bits.
    pub const CLASS_MASK: u16 = 0xF << 8;
    /// Shift of the Finder label bits.
    pub const LABEL_SHIFT: u32 = 12;
    /// Mask of the Finder label bits.
    pub const LABEL_MASK: u16 = 0b111 << 12;
    /// Bits no meaning is given to; always 0.
    pub const RESERVED: u16 = 1 << 15;

    /// The flags of an entry. Bit 6 is never set here: the build adds it
    /// with [`with_system_item`](Self::with_system_item).
    pub fn new(
        kind: NodeKind,
        flags: EntryFlags,
        has_resource_fork: bool,
        class: KindClass,
    ) -> Self {
        let mut bits = match kind {
            NodeKind::File => Self::KIND_FILE,
            NodeKind::Directory => Self::KIND_FOLDER,
            NodeKind::Symlink => Self::KIND_SYMLINK,
            NodeKind::Other => Self::KIND_OTHER,
        };
        if flags.contains(EntryFlags::HIDDEN) {
            bits |= Self::HIDDEN;
        }
        if flags.contains(EntryFlags::COMPRESSED) {
            bits |= Self::COMPRESSED;
        }
        if flags.contains(EntryFlags::LINKS_TO_DIRECTORY) {
            bits |= Self::LINKS_TO_FOLDER;
        }
        if has_resource_fork {
            bits |= Self::RESOURCE_FORK;
        }
        if kind == NodeKind::Directory && flags.contains(EntryFlags::PACKAGE) {
            bits |= Self::PACKAGE;
        }
        bits |= (class.bits() << Self::CLASS_SHIFT) & Self::CLASS_MASK;
        bits |= (u16::from(flags.label()) << Self::LABEL_SHIFT) & Self::LABEL_MASK;
        Self(bits)
    }

    /// `self` with bit 6: a housekeeping item, or inside one.
    #[must_use]
    pub const fn with_system_item(self) -> Self {
        Self(self.0 | Self::SYSTEM_ITEM)
    }

    /// From the stored word.
    pub const fn from_bits(bits: u16) -> Self {
        Self(bits)
    }

    /// The stored word.
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// What the entry is.
    pub const fn kind(self) -> NodeKind {
        match self.0 & Self::KIND_MASK {
            Self::KIND_FILE => NodeKind::File,
            Self::KIND_FOLDER => NodeKind::Directory,
            Self::KIND_SYMLINK => NodeKind::Symlink,
            _ => NodeKind::Other,
        }
    }

    /// True for a folder (not a link to one).
    pub const fn is_folder(self) -> bool {
        self.0 & Self::KIND_MASK == Self::KIND_FOLDER
    }

    /// True when the entry is hidden.
    pub const fn is_hidden(self) -> bool {
        self.0 & Self::HIDDEN != 0
    }

    /// True when the file's content is stored compressed.
    pub const fn is_compressed(self) -> bool {
        self.0 & Self::COMPRESSED != 0
    }

    /// True for a symbolic link to a folder.
    pub const fn links_to_folder(self) -> bool {
        self.0 & Self::LINKS_TO_FOLDER != 0
    }

    /// True when the resource fork holds at least one byte.
    pub const fn has_resource_fork(self) -> bool {
        self.0 & Self::RESOURCE_FORK != 0
    }

    /// True for a housekeeping item and for anything inside one.
    pub const fn is_system_item(self) -> bool {
        self.0 & Self::SYSTEM_ITEM != 0
    }

    /// True for a folder with the Finder's bundle bit.
    pub const fn is_package(self) -> bool {
        self.0 & Self::PACKAGE != 0
    }

    /// The kind class from bits 8–11.
    pub const fn kind_class(self) -> KindClass {
        KindClass::from_bits((self.0 & Self::CLASS_MASK) >> Self::CLASS_SHIFT)
    }

    /// The Finder label from bits 12–14, 0 (none) to 7.
    pub const fn label(self) -> u8 {
        // Three bits: always fits.
        ((self.0 & Self::LABEL_MASK) >> Self::LABEL_SHIFT) as u8
    }

    /// The listing flags these bits stand for.
    pub fn entry_flags(self) -> EntryFlags {
        let mut flags = EntryFlags::EMPTY;
        if self.is_hidden() {
            flags = flags.with(EntryFlags::HIDDEN);
        }
        if self.is_compressed() {
            flags = flags.with(EntryFlags::COMPRESSED);
        }
        if self.links_to_folder() {
            flags = flags.with(EntryFlags::LINKS_TO_DIRECTORY);
        }
        if self.is_package() {
            flags = flags.with(EntryFlags::PACKAGE);
        }
        flags.with_label(self.label())
    }
}

impl fmt::Debug for IndexFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "IndexFlags({:#018b})", self.0)
    }
}

/// Every array of an [`Index`] but `folded`, for [`Index::from_parts`].
pub(crate) struct IndexParts {
    pub(crate) names: Box<str>,
    pub(crate) offsets: Box<[u32]>,
    pub(crate) parent: Box<[u32]>,
    pub(crate) node: Box<[u64]>,
    pub(crate) size: Box<[u64]>,
    pub(crate) modified: Box<[i64]>,
    pub(crate) rsize: Box<[u32]>,
    pub(crate) flags: Box<[u16]>,
    pub(crate) identity: Option<VolumeIdentity>,
    pub(crate) summary: ScanSummary,
}

/// The names and facts of every entry of one volume, in depth-first
/// pre-order. Built by [`crate::search::build::build`]; `Send + Sync`, shared
/// behind an `Arc`.
pub struct Index {
    /// Every name, each followed by one 0 byte. A `str`, so a name is a
    /// slice with no validation; separators are valid UTF-8.
    names: Box<str>,
    /// `names` case-folded ([`fold_into`]): same length and offsets.
    folded: Box<[u8]>,
    /// Length entries + 1; name `i` is `names[offsets[i] .. offsets[i + 1] - 1]`.
    offsets: Box<[u32]>,
    /// Entry number of the parent; entry 0 is the root and its own parent.
    parent: Box<[u32]>,
    node: Box<[u64]>,
    /// [`NO_SIZE`] = unknown or a folder.
    size: Box<[u64]>,
    /// Unix nanoseconds; [`NO_TIME`] = unknown.
    modified: Box<[i64]>,
    /// Resource fork bytes, saturated at [`MAX_RSIZE`]; [`NO_RSIZE`] = none.
    rsize: Box<[u32]>,
    /// [`IndexFlags`] words.
    flags: Box<[u16]>,
    identity: Option<VolumeIdentity>,
    summary: ScanSummary,
}

impl Index {
    /// Builds an index from its arrays and computes `folded` (in parallel).
    ///
    /// Assumes, and checks only with `debug_assert` (see [`Self::verify`]):
    /// - `parent` has n ≥ 1 entries, n ≤ [`MAX_ENTRIES`]; `node`, `size`,
    ///   `modified`, `rsize` and `flags` have n entries; `offsets` has n + 1.
    /// - `names.len()` ≤ [`MAX_NAME_BYTES`]; `offsets[0] == 0`,
    ///   `offsets[n] == names.len()`, each offset larger than the one before,
    ///   the byte before each offset but the first is 0, and `names` holds
    ///   exactly n 0 bytes (no NUL inside a name).
    /// - `parent[0] == 0`, entry 0 is a folder, and for every other `i`,
    ///   `parent[i] < i` and `parent[i]` is a folder by `flags`.
    ///
    /// Callers with untrusted arrays (the saved-index loader) validate these
    /// first. If they do not hold anyway, the accessors still never panic or
    /// loop; they return wrong or empty answers.
    pub(crate) fn from_parts(parts: IndexParts) -> Self {
        let folded = fold_names(&parts.names, &parts.offsets);
        let index = Self {
            names: parts.names,
            folded,
            offsets: parts.offsets,
            parent: parts.parent,
            node: parts.node,
            size: parts.size,
            modified: parts.modified,
            rsize: parts.rsize,
            flags: parts.flags,
            identity: parts.identity,
            summary: parts.summary,
        };
        debug_assert_eq!(index.verify(), Ok(()));
        index
    }

    /// Checks every invariant [`Self::from_parts`] assumes, in one linear
    /// pass. `Err` names the first one that fails.
    pub(crate) fn verify(&self) -> std::result::Result<(), &'static str> {
        let n = self.parent.len();
        if n == 0 {
            return Err("no root entry");
        }
        if !u32::try_from(n).is_ok_and(|n| n <= MAX_ENTRIES) {
            return Err("too many entries");
        }
        let same = |len: usize| len == n;
        if !(same(self.node.len())
            && same(self.size.len())
            && same(self.modified.len())
            && same(self.rsize.len())
            && same(self.flags.len()))
        {
            return Err("array lengths differ");
        }
        if self.offsets.len() != n.saturating_add(1) {
            return Err("offsets length is not entries + 1");
        }
        let names = self.names.as_bytes();
        if u32::try_from(names.len()).is_err() {
            return Err("names longer than MAX_NAME_BYTES");
        }
        if self.folded.len() != names.len() {
            return Err("folded length differs from names");
        }
        if self.offsets.first() != Some(&0) {
            return Err("offsets[0] is not 0");
        }
        if self.offsets.last().map(|&o| o as usize) != Some(names.len()) {
            return Err("last offset is not names length");
        }
        for pair in self.offsets.windows(2) {
            let &[start, end] = pair else {
                return Err("offsets");
            };
            if end <= start {
                return Err("offsets not strictly ascending");
            }
            let separator = (end as usize).checked_sub(1).and_then(|p| names.get(p));
            if separator != Some(&0) {
                return Err("a name is not followed by a 0 byte");
            }
        }
        if memchr::memchr_iter(0, names).count() != n {
            return Err("a name holds a 0 byte");
        }
        if self.parent.first() != Some(&0) {
            return Err("parent[0] is not 0");
        }
        if !self.is_folder(0) {
            return Err("root is not a folder");
        }
        for (i, &p) in self.parent.iter().enumerate().skip(1) {
            if p as usize >= i {
                return Err("parent not before its child");
            }
            if !self.is_folder(p) {
                return Err("parent is not a folder");
            }
        }
        Ok(())
    }

    /// Number of entries, the root included.
    pub fn len(&self) -> u32 {
        u32::try_from(self.parent.len()).unwrap_or(u32::MAX)
    }

    /// True when the index has no entry at all. A built index always has
    /// the root, so this is false for every index [`build`] returns.
    ///
    /// [`build`]: crate::search::build::build
    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }

    /// Byte range of name `i` in `names` and `folded`, without the separator.
    fn name_range(&self, i: u32) -> Option<Range<usize>> {
        let k = i as usize;
        let start = *self.offsets.get(k)? as usize;
        let end = (*self.offsets.get(k.checked_add(1)?)? as usize).checked_sub(1)?;
        (start <= end).then_some(start..end)
    }

    /// The name of entry `i` (the volume's name for the root). A NUL byte of
    /// the original name reads as [`NUL_REPLACEMENT`]. Empty when out of range.
    pub fn name(&self, i: u32) -> &str {
        self.name_range(i)
            .and_then(|r| self.names.get(r))
            .unwrap_or("")
    }

    /// Entry `i` as the same borrowed view a folder listing gives.
    pub fn entry(&self, i: u32) -> Option<DirEntry<'_>> {
        let k = i as usize;
        let flags = IndexFlags(*self.flags.get(k)?);
        let node = *self.node.get(k)?;
        let size = *self.size.get(k)?;
        let rsize = *self.rsize.get(k)?;
        let modified = *self.modified.get(k)?;
        let name = self.names.get(self.name_range(i)?)?;
        Some(DirEntry {
            node: NodeId(node),
            name,
            kind: flags.kind(),
            size: (size != NO_SIZE).then_some(size),
            resource_size: (rsize != NO_RSIZE).then_some(u64::from(rsize)),
            modified: (modified != NO_TIME).then_some(Timestamp::from_unix_nanos(modified)),
            flags: flags.entry_flags(),
        })
    }

    /// The flags of entry `i`.
    pub fn flags(&self, i: u32) -> Option<IndexFlags> {
        self.flags.get(i as usize).map(|&f| IndexFlags(f))
    }

    /// Entry number of `i`'s parent folder. The root is its own parent.
    pub fn parent(&self, i: u32) -> Option<u32> {
        self.parent.get(i as usize).copied()
    }

    /// True when entry `i` is a folder (not a link to one).
    pub fn is_folder(&self, i: u32) -> bool {
        self.flags(i).is_some_and(IndexFlags::is_folder)
    }

    /// True when entry `i` is hidden.
    pub fn is_hidden(&self, i: u32) -> bool {
        self.flags(i).is_some_and(IndexFlags::is_hidden)
    }

    /// The kind class of entry `i`; `Other` when out of range.
    pub fn kind_class(&self, i: u32) -> KindClass {
        self.flags(i)
            .map_or(KindClass::Other, IndexFlags::kind_class)
    }

    /// Entry `i` and everything inside it: `i..i + 1` for a non-folder, the
    /// folder and its whole subtree for a folder, `i..i` when out of range.
    /// The root's subtree is the whole index.
    ///
    /// Linear in the size of the subtree (about 1 ms per million entries),
    /// so call it off the UI thread.
    pub fn subtree(&self, i: u32) -> Range<u32> {
        let len = self.len();
        if i >= len {
            return i..i;
        }
        let next = i.saturating_add(1);
        if !self.is_folder(i) {
            return i..next;
        }
        // Pre-order: the first later entry whose parent is before `i` is the
        // first entry outside the subtree.
        let rest = self.parent.get(next as usize..).unwrap_or(&[]);
        let end = match rest.iter().position(|&p| p < i) {
            Some(n) => u32::try_from(n).map_or(len, |n| next.saturating_add(n)),
            None => len,
        };
        i..end
    }

    /// Pushes `i`, then its ancestors nearest first, ending with the root.
    /// False when the chain is longer than [`MAX_DEPTH`] or leaves the index;
    /// what was pushed then is the nearest part.
    fn chain(&self, i: u32, out: &mut Vec<u32>) -> bool {
        let mut e = i;
        for _ in 0..MAX_DEPTH {
            out.push(e);
            if e == 0 {
                return true;
            }
            match self.parent(e) {
                // `p < e` makes the walk end even if the invariants fail.
                Some(p) if p < e => e = p,
                _ => return false,
            }
        }
        false
    }

    /// The path from the root to entry `i`, both included, as node and name
    /// pairs, root first: what a breadcrumb bar and a `Location` need.
    ///
    /// Empty when `i` is out of range or nested deeper than [`MAX_DEPTH`]
    /// (such an entry cannot be navigated to as a path).
    pub fn crumbs(&self, i: u32) -> Vec<(NodeId, String)> {
        if i >= self.len() {
            return Vec::new();
        }
        let mut chain = Vec::new();
        if !self.chain(i, &mut chain) {
            return Vec::new();
        }
        chain
            .iter()
            .rev()
            .map(|&e| {
                let node = self.node.get(e as usize).copied().unwrap_or(0);
                (NodeId(node), self.name(e).to_owned())
            })
            .collect()
    }

    /// Appends the path of the folder that holds entry `i` to `out`: the
    /// root's name first, then each folder down to `i`'s parent, joined by
    /// `separator` (for example `" › "` or `"/"`). Nothing for the root or an
    /// entry out of range. A chain deeper than [`MAX_DEPTH`] starts with `…`
    /// and keeps the nearest folders.
    pub fn folder_path(&self, i: u32, separator: &str, out: &mut String) {
        if i == 0 {
            return;
        }
        let Some(parent) = self.parent(i) else {
            return;
        };
        let mut chain = Vec::with_capacity(16);
        let complete = self.chain(parent, &mut chain);
        let mut first = true;
        if !complete {
            out.push('…');
            first = false;
        }
        for &e in chain.iter().rev() {
            if !first {
                out.push_str(separator);
            }
            first = false;
            out.push_str(self.name(e));
        }
    }

    /// The first entry whose node is `node`. Linear; meant for a scope
    /// change, not for every keystroke.
    pub fn find_node(&self, node: NodeId) -> Option<u32> {
        let at = self.node.iter().position(|&n| n == node.0)?;
        u32::try_from(at).ok()
    }

    /// The volume's identity and change marker when the index was built.
    pub fn identity(&self) -> Option<VolumeIdentity> {
        self.identity
    }

    /// What the scan covered: `entries` kept (the root not counted), and
    /// `skipped` structures and entries that are missing from the index. A
    /// non-zero `skipped` means the index is incomplete.
    pub fn summary(&self) -> ScanSummary {
        self.summary
    }

    /// Bytes held by the arrays, `folded` included.
    pub fn heap_bytes(&self) -> usize {
        use std::mem::size_of_val;
        [
            self.names.len(),
            self.folded.len(),
            size_of_val(&*self.offsets),
            size_of_val(&*self.parent),
            size_of_val(&*self.node),
            size_of_val(&*self.size),
            size_of_val(&*self.modified),
            size_of_val(&*self.rsize),
            size_of_val(&*self.flags),
        ]
        .iter()
        .fold(0usize, |sum, &b| sum.saturating_add(b))
    }
}

/// The raw arrays, read-only, for the query engine, ordering and the saved
/// index (`query.rs`, `order.rs`, `persist.rs`).
impl Index {
    /// Every name, each followed by one 0 byte.
    pub(crate) fn names_bytes(&self) -> &[u8] {
        self.names.as_bytes()
    }

    /// `names` case-folded; same length and offsets.
    pub(crate) fn folded_bytes(&self) -> &[u8] {
        &self.folded
    }

    /// Start of each name, plus `names.len()` at the end.
    pub(crate) fn offsets(&self) -> &[u32] {
        &self.offsets
    }

    /// Parent entry numbers.
    pub(crate) fn parents(&self) -> &[u32] {
        &self.parent
    }

    /// Node ids.
    pub(crate) fn nodes(&self) -> &[u64] {
        &self.node
    }

    /// Data sizes; [`NO_SIZE`] for none.
    pub(crate) fn sizes(&self) -> &[u64] {
        &self.size
    }

    /// Modification times in Unix nanoseconds; [`NO_TIME`] for none.
    pub(crate) fn modified_times(&self) -> &[i64] {
        &self.modified
    }

    /// Resource fork sizes; [`NO_RSIZE`] for none.
    pub(crate) fn rsizes(&self) -> &[u32] {
        &self.rsize
    }

    /// [`IndexFlags`] words.
    pub(crate) fn flag_words(&self) -> &[u16] {
        &self.flags
    }
}

impl fmt::Debug for Index {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Index")
            .field("entries", &self.parent.len())
            .field("name_bytes", &self.names.len())
            .field("identity", &self.identity)
            .field("summary", &self.summary)
            .finish_non_exhaustive()
    }
}

/// `names` case-folded with [`fold_into`], computed in parallel over pieces
/// of [`FOLD_CHUNK_ENTRIES`] entries, each writing its own slice of one
/// pre-sized buffer.
///
/// The fold works character by character, so any split at a character
/// boundary gives the same bytes. Split points come from `offsets`, and one
/// that is not a usable boundary is skipped, so the result is right whatever
/// `offsets` holds.
pub(crate) fn fold_names(names: &str, offsets: &[u32]) -> Box<[u8]> {
    let mut out = vec![0u8; names.len()];
    let mut pieces: Vec<(&mut [u8], &str)> = Vec::new();
    let mut rest_out: &mut [u8] = &mut out;
    let mut rest_in: &str = names;
    let mut consumed = 0usize;
    for &split in offsets.iter().step_by(FOLD_CHUNK_ENTRIES).skip(1) {
        let Some(len) = (split as usize).checked_sub(consumed) else {
            continue;
        };
        if len == 0 || len >= rest_in.len() {
            continue;
        }
        let Some((head_in, tail_in)) = rest_in.split_at_checked(len) else {
            continue;
        };
        let Some((head_out, tail_out)) = std::mem::take(&mut rest_out).split_at_mut_checked(len)
        else {
            break;
        };
        pieces.push((head_out, head_in));
        rest_out = tail_out;
        rest_in = tail_in;
        consumed = split as usize;
    }
    pieces.push((rest_out, rest_in));

    let fold_piece = |scratch: &mut Vec<u8>, (dst, src): (&mut [u8], &str)| {
        scratch.clear();
        fold_into(src, scratch);
        // `fold_into` keeps the length; fall back to the bytes as they are
        // rather than fail if it ever did not.
        let folded: &[u8] = if scratch.len() == dst.len() {
            scratch
        } else {
            src.as_bytes()
        };
        if folded.len() == dst.len() {
            dst.copy_from_slice(folded);
        }
    };
    if pieces.len() == 1 {
        let mut scratch = Vec::new();
        pieces
            .into_iter()
            .for_each(|piece| fold_piece(&mut scratch, piece));
    } else {
        pieces
            .into_par_iter()
            .for_each_init(Vec::new, |scratch, piece| fold_piece(scratch, piece));
    }
    out.into_boxed_slice()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An index from `(name, parent, kind)` triples in pre-order; entry 0 is
    /// the root. Node ids are 100 + entry number.
    pub(super) fn index_of(entries: &[(&str, u32, NodeKind)]) -> Index {
        let mut names = String::new();
        let mut offsets = vec![0u32];
        for (name, _, _) in entries {
            names.push_str(name);
            names.push('\0');
            offsets.push(names.len() as u32);
        }
        let n = entries.len();
        let class = |name: &str, kind| {
            crate::search::kind::classify_entry(name, kind == NodeKind::Directory, None)
        };
        Index::from_parts(IndexParts {
            names: names.into_boxed_str(),
            offsets: offsets.into(),
            parent: entries.iter().map(|e| e.1).collect(),
            node: (0..n as u64).map(|i| 100 + i).collect(),
            size: entries
                .iter()
                .map(|e| if e.2 == NodeKind::File { 10 } else { NO_SIZE })
                .collect(),
            modified: vec![NO_TIME; n].into(),
            rsize: vec![NO_RSIZE; n].into(),
            flags: entries
                .iter()
                .map(|e| IndexFlags::new(e.2, EntryFlags::EMPTY, false, class(e.0, e.2)).bits())
                .collect(),
            identity: None,
            summary: ScanSummary::default(),
        })
    }

    use NodeKind::{Directory as D, File as F};

    fn sample() -> Index {
        // 0 vol
        // 1   docs/        2 a.txt   3 sub/   4 b.rs
        // 5   pic.png
        // 6   empty/
        index_of(&[
            ("vol", 0, D),
            ("docs", 0, D),
            ("a.txt", 1, F),
            ("sub", 1, D),
            ("b.rs", 3, F),
            ("pic.png", 0, F),
            ("empty", 0, D),
        ])
    }

    #[test]
    fn index_is_send_and_sync() {
        fn check<T: Send + Sync>() {}
        check::<Index>();
        check::<IndexFlags>();
    }

    #[test]
    fn accessors() {
        let index = sample();
        assert_eq!(index.len(), 7);
        assert!(!index.is_empty());
        assert_eq!(index.verify(), Ok(()));
        assert_eq!(index.name(0), "vol");
        assert_eq!(index.name(4), "b.rs");
        assert_eq!(index.parent(4), Some(3));
        assert_eq!(index.parent(0), Some(0));
        assert!(index.is_folder(0) && index.is_folder(3) && !index.is_folder(4));
        assert_eq!(index.kind_class(2), KindClass::Document);
        assert_eq!(index.kind_class(4), KindClass::Code);
        assert_eq!(index.kind_class(1), KindClass::Folder);
        assert_eq!(index.find_node(NodeId(103)), Some(3));
        assert_eq!(index.find_node(NodeId(99)), None);
        let e = index.entry(2).unwrap();
        assert_eq!(e.name, "a.txt");
        assert_eq!(e.node, NodeId(102));
        assert_eq!(e.kind, NodeKind::File);
        assert_eq!(e.size, Some(10));
        assert_eq!(e.resource_size, None);
        assert_eq!(e.modified, None);
        assert_eq!(index.entry(1).unwrap().size, None);
        assert_eq!(
            index.folded_bytes(),
            b"vol\0docs\0a.txt\0sub\0b.rs\0pic.png\0empty\0"
        );
        assert!(index.heap_bytes() >= 7 * 38);
    }

    #[test]
    fn subtrees() {
        let index = sample();
        assert_eq!(index.subtree(0), 0..7);
        assert_eq!(index.subtree(1), 1..5);
        assert_eq!(index.subtree(3), 3..5);
        assert_eq!(index.subtree(2), 2..3);
        assert_eq!(index.subtree(5), 5..6);
        assert_eq!(index.subtree(6), 6..7);
        assert!(index.subtree(7).is_empty());
        assert!(index.subtree(u32::MAX).is_empty());
    }

    #[test]
    fn crumbs_and_folder_path() {
        let index = sample();
        let crumbs = index.crumbs(4);
        assert_eq!(
            crumbs,
            vec![
                (NodeId(100), "vol".to_owned()),
                (NodeId(101), "docs".to_owned()),
                (NodeId(103), "sub".to_owned()),
                (NodeId(104), "b.rs".to_owned()),
            ]
        );
        assert_eq!(index.crumbs(0), vec![(NodeId(100), "vol".to_owned())]);
        let mut path = String::new();
        index.folder_path(4, " › ", &mut path);
        assert_eq!(path, "vol › docs › sub");
        path.clear();
        index.folder_path(5, "/", &mut path);
        assert_eq!(path, "vol");
        path.clear();
        index.folder_path(0, "/", &mut path);
        assert_eq!(path, "");
        // Appends.
        let mut path = String::from("x:");
        index.folder_path(2, "/", &mut path);
        assert_eq!(path, "x:vol/docs");
    }

    #[test]
    fn out_of_range_is_empty() {
        let index = sample();
        for i in [7, 8, u32::MAX - 1, u32::MAX] {
            assert_eq!(index.name(i), "");
            assert_eq!(index.entry(i), None);
            assert_eq!(index.flags(i), None);
            assert_eq!(index.parent(i), None);
            assert!(!index.is_folder(i));
            assert!(!index.is_hidden(i));
            assert_eq!(index.kind_class(i), KindClass::Other);
            assert!(index.subtree(i).is_empty());
            assert!(index.crumbs(i).is_empty());
            let mut path = String::new();
            index.folder_path(i, "/", &mut path);
            assert!(path.is_empty());
        }
    }

    #[test]
    fn deep_chains_are_bounded() {
        // A chain of folders deeper than MAX_DEPTH.
        let depth = MAX_DEPTH + 10;
        let entries: Vec<(&str, u32, NodeKind)> = (0..depth)
            .map(|i| ("d", i.saturating_sub(1) as u32, D))
            .collect();
        let index = index_of(&entries);
        let last = (depth - 1) as u32;
        assert!(index.crumbs(last).is_empty());
        assert_eq!(index.crumbs(MAX_DEPTH as u32 - 1).len(), MAX_DEPTH);
        let mut path = String::new();
        index.folder_path(last, "/", &mut path);
        assert!(path.starts_with("…/d/"));
        assert_eq!(path.matches('d').count(), MAX_DEPTH);
        assert_eq!(index.subtree(0), 0..depth as u32);
        assert_eq!(index.subtree(last), last..last + 1);
    }

    #[test]
    fn broken_parts_never_panic() {
        // Arrays that break every invariant: accessors still answer.
        let index = Index {
            names: "ab\0c".into(),
            folded: Box::new(*b"ab\0c"),
            offsets: Box::new([5, 1, 9]),
            parent: Box::new([3, 3, 1, 0]),
            node: Box::new([1]),
            size: Box::new([]),
            modified: Box::new([]),
            rsize: Box::new([]),
            flags: Box::new([1, 1, 1, 1]),
            identity: None,
            summary: ScanSummary::default(),
        };
        assert!(index.verify().is_err());
        for i in 0..6 {
            let _ = index.name(i);
            let _ = index.entry(i);
            let _ = index.subtree(i);
            let _ = index.crumbs(i);
            let mut s = String::new();
            index.folder_path(i, "/", &mut s);
        }
        // Folding ignores offsets that are not boundaries.
        assert_eq!(&*fold_names("ÀB\0c", &[1, 99]), "àb\0c".as_bytes());
    }

    #[test]
    fn verify_catches_each_rule() {
        let good = || sample();
        assert_eq!(good().verify(), Ok(()));
        let mut i = good();
        i.parent[4] = 4;
        assert!(i.verify().is_err());
        let mut i = good();
        i.parent[4] = 2; // a file
        assert_eq!(i.verify(), Err("parent is not a folder"));
        let mut i = good();
        i.parent[0] = 1;
        assert!(i.verify().is_err());
        let mut i = good();
        i.flags[0] = IndexFlags::KIND_FILE;
        assert_eq!(i.verify(), Err("root is not a folder"));
        let mut i = good();
        i.offsets[2] = i.offsets[1];
        assert!(i.verify().is_err());
        let mut i = good();
        i.names = "vol\0docs\0a\0txt\0sub\0b.rs\0pic.png\0empty".into();
        assert!(i.verify().is_err());
        let mut i = good();
        i.rsize = Box::new([]);
        assert_eq!(i.verify(), Err("array lengths differ"));
    }

    #[test]
    fn flags_round_trip() {
        let all = EntryFlags::HIDDEN
            .with(EntryFlags::COMPRESSED)
            .with(EntryFlags::LINKS_TO_DIRECTORY);
        for kind in [
            NodeKind::File,
            NodeKind::Directory,
            NodeKind::Symlink,
            NodeKind::Other,
        ] {
            for flags in [
                EntryFlags::EMPTY,
                EntryFlags::HIDDEN,
                EntryFlags::COMPRESSED,
                all,
            ] {
                for fork in [false, true] {
                    for class in KindClass::ALL {
                        let f = IndexFlags::new(kind, flags, fork, class);
                        assert_eq!(f.kind(), kind);
                        assert_eq!(f.entry_flags(), flags);
                        assert_eq!(f.has_resource_fork(), fork);
                        assert_eq!(f.kind_class(), class);
                        assert_eq!(f.is_folder(), kind == NodeKind::Directory);
                        assert_eq!(IndexFlags::from_bits(f.bits()), f);
                        assert!(!f.is_system_item() && !f.is_package());
                        assert_eq!(f.label(), 0);
                        assert_eq!(f.bits() & 0xF0C0, 0, "bits 6, 7 and 12-15");
                    }
                }
            }
        }
    }

    #[test]
    fn package_label_and_system_bits_round_trip() {
        let all = EntryFlags::HIDDEN
            .with(EntryFlags::COMPRESSED)
            .with(EntryFlags::LINKS_TO_DIRECTORY);
        for kind in [
            NodeKind::File,
            NodeKind::Directory,
            NodeKind::Symlink,
            NodeKind::Other,
        ] {
            for base in [EntryFlags::EMPTY, all] {
                for package in [false, true] {
                    for label in 0..=7u8 {
                        for class in KindClass::ALL {
                            let mut flags = base.with_label(label);
                            if package {
                                flags = flags.with(EntryFlags::PACKAGE);
                            }
                            let f = IndexFlags::new(kind, flags, true, class);
                            let is_package = package && kind == NodeKind::Directory;
                            assert_eq!(f.is_package(), is_package);
                            assert_eq!(f.label(), label);
                            assert_eq!(f.kind_class(), class);
                            assert_eq!(f.kind(), kind);
                            assert!(f.has_resource_fork());
                            assert!(!f.is_system_item());
                            // Back to listing flags: a file's bundle bit is
                            // not a package and is not kept.
                            let mut back = base.with_label(label);
                            if is_package {
                                back = back.with(EntryFlags::PACKAGE);
                            }
                            assert_eq!(f.entry_flags(), back);
                            // Bit 6 changes nothing else.
                            let s = f.with_system_item();
                            assert!(s.is_system_item());
                            assert_eq!(s.bits() & !IndexFlags::SYSTEM_ITEM, f.bits());
                            assert_eq!(s.entry_flags(), f.entry_flags());
                            assert_eq!(s.with_system_item(), s);
                            assert_eq!(IndexFlags::from_bits(s.bits()), s);
                            assert_eq!(s.bits() & IndexFlags::RESERVED, 0);
                        }
                    }
                }
            }
        }
        // The alias bit has no place in the index.
        let alias = IndexFlags::new(NodeKind::File, EntryFlags::ALIAS, false, KindClass::Other);
        assert_eq!(alias.entry_flags(), EntryFlags::EMPTY);
        // The masks do not overlap and leave bit 15 alone.
        let masks = [
            IndexFlags::KIND_MASK,
            IndexFlags::HIDDEN,
            IndexFlags::COMPRESSED,
            IndexFlags::LINKS_TO_FOLDER,
            IndexFlags::RESOURCE_FORK,
            IndexFlags::SYSTEM_ITEM,
            IndexFlags::PACKAGE,
            IndexFlags::CLASS_MASK,
            IndexFlags::LABEL_MASK,
            IndexFlags::RESERVED,
        ];
        let mut seen = 0u16;
        for m in masks {
            assert_eq!(seen & m, 0, "{m:#06x}");
            seen |= m;
        }
        assert_eq!(seen, u16::MAX);
        assert_eq!(IndexFlags::from_bits(0x7000).label(), 7);
    }

    #[test]
    fn parallel_fold_matches_sequential() {
        // More than one piece: names of mixed scripts across several chunks.
        let words = ["Alpha", "ÉCOLE", "ΣΟΦΙΑ", "日本", "İx", "z"];
        let mut names = String::new();
        let mut offsets = vec![0u32];
        for i in 0..(FOLD_CHUNK_ENTRIES * 2 + 17) {
            names.push_str(words[i % words.len()]);
            names.push('\0');
            offsets.push(names.len() as u32);
        }
        let mut expected = Vec::new();
        fold_into(&names, &mut expected);
        assert_eq!(&*fold_names(&names, &offsets), expected.as_slice());
        assert_eq!(&*fold_names(&names, &[]), expected.as_slice());
    }
}
