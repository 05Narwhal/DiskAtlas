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

//! Background half of the listing pipeline. No GPUI types.
//!
//! A [`Loader`] reads a folder one page at a time. Each [`Loader::step`] reads a page, sorts it,
//! merges it into the running display order and hands back the page and the whole new order. The
//! UI thread only pushes the page and swaps the order in; it never compares two names.
//!
//! Rows are named by their arrival index: the position at which the stream reported them. That
//! index never changes while the folder is shown, so a selection survives any re-ordering.

use std::cmp::Ordering;
use std::sync::Arc;

use disk_atlas_lib::error::Error;
use disk_atlas_lib::fs::{DirBatch, DirEntry, DirProgress, DirStream, FileSystem, NodeId};
use disk_atlas_lib::search::Index;
use disk_atlas_lib::sizes::{IndexSizes, Totals};

use super::order::{SortColumn, SortSpec, compare_entries_sized};
use super::sizes::{LiveSizes, RowSize, SizeState, resolve_page};
use super::visibility::{Rule, Visibility};
use crate::format;

/// Entries asked for by the first step: enough for the first screen, quickly.
const FIRST_PAGE: usize = 256;
/// Each later step asks for this many times more than the one before.
const PAGE_GROWTH: usize = 4;
/// The largest page asked for.
const LARGEST_PAGE: usize = 16_384;
/// Guess at the average name length, to size a page's name buffer.
const NAME_BYTES_GUESS: usize = 24;
/// The most entries one folder may have: arrival indices are `u32`.
const MAX_ROWS: usize = u32::MAX as usize;
/// Empty "more" answers in a row after which a listing is given up.
const MAX_STALLS: u32 = 64;

/// The rows of a folder in arrival order, as shared pages. Cheap to clone.
#[derive(Clone, Default)]
pub struct Rows {
    pages: Vec<Arc<DirBatch>>,
    /// Arrival index of each page's first entry. Strictly increasing: empty pages are not kept.
    starts: Vec<u32>,
    len: u32,
    /// The totals of the folder rows that have one, by arrival index, ascending.
    totals: Arc<Vec<(u32, Totals)>>,
    /// True when the totals are lower bounds.
    lower_bound: bool,
    /// The host's folder rows: handles into the source's live tree (`None` on every other
    /// volume, and until the rows have been looked up).
    live: Option<Arc<LiveSizes>>,
}

impl Rows {
    /// Number of rows.
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// True when there are no rows.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The entry with this arrival index. Page lookup is a binary search over a few dozen page
    /// starts.
    pub fn get(&self, arrival: u32) -> Option<DirEntry<'_>> {
        if arrival >= self.len {
            return None;
        }
        let page = self
            .starts
            .partition_point(|&start| start <= arrival)
            .checked_sub(1)?;
        let start = *self.starts.get(page)?;
        self.pages.get(page)?.get((arrival - start) as usize)
    }

    /// Appends a page. An empty page is ignored, and so is a page that would take the folder past
    /// `u32::MAX` rows (the loader never produces one).
    pub fn push(&mut self, page: Arc<DirBatch>) {
        let Ok(added) = u32::try_from(page.len()) else {
            return;
        };
        if added == 0 {
            return;
        }
        let Some(len) = self.len.checked_add(added) else {
            return;
        };
        self.starts.push(self.len);
        self.pages.push(page);
        self.len = len;
    }

    /// Adds the totals of rows of a page just pushed: arrivals ascending and above every total
    /// kept so far (what [`resolve_page`] gives).
    pub fn push_totals(&mut self, items: Vec<(u32, Totals)>) {
        if !items.is_empty() {
            Arc::make_mut(&mut self.totals).extend(items);
        }
    }

    /// Replaces every total (a table that arrived after the rows) and says whether they are
    /// lower bounds.
    pub fn set_totals(&mut self, items: Vec<(u32, Totals)>, lower_bound: bool) {
        self.totals = Arc::new(items);
        self.lower_bound = lower_bound;
    }

    /// Says whether the totals pushed from now on are lower bounds.
    pub fn set_lower_bound(&mut self, lower_bound: bool) {
        self.lower_bound = lower_bound;
    }

    /// What the Size column keeps for the row `arrival`: its total, or nothing. A binary search
    /// over the folders that have a total.
    pub fn row_size(&self, arrival: u32) -> RowSize {
        match self.totals.binary_search_by_key(&arrival, |(at, _)| *at) {
            Ok(at) => self
                .totals
                .get(at)
                .map_or(RowSize::None, |(_, t)| RowSize::Fixed(*t, self.lower_bound)),
            Err(_) => self
                .live
                .as_ref()
                .and_then(|live| live.row(arrival))
                .unwrap_or(RowSize::None),
        }
    }

    /// Gives the folder rows their handles into the host's live tree (or takes them away).
    pub fn set_live(&mut self, live: Option<Arc<LiveSizes>>) {
        self.live = live;
    }

    /// How the Size column shows the row `arrival`: a volume's total as it was resolved, or the
    /// host's state read through the row's handle. O(1) apart from the two small searches, and
    /// one short lock for a host row; no path is resolved.
    pub fn size_state(&self, arrival: u32) -> SizeState {
        match self.row_size(arrival) {
            RowSize::Live(source, dir) => self
                .live
                .as_ref()
                .map_or(SizeState::Unknown, |live| live.state(source, dir)),
            other => other.state(),
        }
    }

    /// The bytes a folder row is ordered by: its total, or what the host's tree has measured of
    /// it so far.
    fn sort_bytes(&self, arrival: u32) -> Option<u64> {
        match self.row_size(arrival) {
            RowSize::Live(..) => self.size_state(arrival).bytes(),
            other => other.bytes(),
        }
    }

    /// How many folder rows have a handle into the host tree.
    pub fn live_count(&self) -> usize {
        self.live.as_ref().map_or(0, |live| live.resolved())
    }

    /// True while the host folder these rows list is still being measured.
    pub fn live_folder_running(&self) -> bool {
        self.live.as_ref().is_some_and(|live| live.folder_running())
    }

    /// The (arrival, totals) of every folder row that has a total.
    pub fn totals(&self) -> &[(u32, Totals)] {
        &self.totals
    }

    /// Every row with its arrival index, in arrival order, without page lookups.
    pub fn iter(&self) -> impl Iterator<Item = (u32, DirEntry<'_>)> + '_ {
        self.pages
            .iter()
            .zip(&self.starts)
            .flat_map(|(page, &start)| (start..).zip(page.iter()))
    }
}

/// Where the rows the list shows come from. Cheap to clone.
///
/// A row is named by a `u32` id: the arrival index for a folder, the entry number for search
/// results. The selection and the order are kept in those ids, so they work the same for both.
#[derive(Clone)]
pub enum RowSource {
    /// The rows of a folder, by arrival index.
    Folder(Rows),
    /// Search results: an entry number of the index plays the part of the arrival index.
    Index(Arc<Index>),
}

impl Default for RowSource {
    fn default() -> Self {
        Self::Folder(Rows::default())
    }
}

impl RowSource {
    /// The row with this id. O(1) apart from the page lookup of a folder; no allocation.
    #[inline]
    pub fn get(&self, id: u32) -> Option<DirEntry<'_>> {
        match self {
            Self::Folder(rows) => rows.get(id),
            Self::Index(index) => index.entry(id),
        }
    }

    /// The name of the row with this id, without building the whole entry.
    #[inline]
    pub fn name(&self, id: u32) -> Option<&str> {
        match self {
            Self::Folder(rows) => rows.get(id).map(|entry| entry.name),
            Self::Index(index) => (id < index.len()).then(|| index.name(id)),
        }
    }

    /// Number of rows: those of the folder, or every entry of the index.
    pub fn len(&self) -> usize {
        match self {
            Self::Folder(rows) => rows.len(),
            Self::Index(index) => index.len() as usize,
        }
    }

    /// True when there is no row.
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Folder(rows) => rows.is_empty(),
            Self::Index(index) => index.is_empty(),
        }
    }

    /// The folder rows, or `None` for search results.
    pub fn folder(&self) -> Option<&Rows> {
        match self {
            Self::Folder(rows) => Some(rows),
            Self::Index(_) => None,
        }
    }
}

/// How a step ended.
#[derive(Debug)]
pub enum Outcome {
    /// There is more to read: call `step` again.
    More,
    /// The folder has been read to its end.
    Done,
    /// Reading stopped with this error. Rows read before it stay.
    Failed(Error),
}

/// What one [`Loader::step`] produced.
pub struct Step {
    /// The entries read by this step, if any. Already in the loader's own `Rows`; the UI pushes
    /// the same Arc into its copy.
    pub page: Option<Arc<DirBatch>>,
    /// The whole display order after this step: arrival indices.
    pub order: Arc<[u32]>,
    /// The totals of the page's folder rows, for [`Rows::push_totals`]. Empty without a size
    /// table.
    pub totals: Vec<(u32, Totals)>,
    /// Rows read so far that the visibility rule hides (left out of `order` unless the folder is
    /// revealed).
    pub hidden: usize,
    /// Whether to call `step` again.
    pub outcome: Outcome,
}

/// An entry with what the comparator needs, borrowed from its page.
#[derive(Clone, Copy)]
struct Key<'a> {
    entry: DirEntry<'a>,
    /// `format::kind` text when sorting by kind, otherwise empty.
    kind: &'static str,
    /// The size to order by: a folder's total when it has one, else the entry's own.
    size: Option<u64>,
    arrival: u32,
}

/// The display order: the column order, then arrival as the final tie-break. Never `Equal` for
/// two different rows.
#[inline]
fn compare_keys(spec: SortSpec, a: &Key<'_>, b: &Key<'_>) -> Ordering {
    let by_entry =
        compare_entries_sized(spec, (&a.entry, a.kind, a.size), (&b.entry, b.kind, b.size));
    by_entry.then_with(|| a.arrival.cmp(&b.arrival))
}

/// The size a row is ordered by under `spec`: a folder's total when `rows` has one.
#[inline]
fn size_of(spec: SortSpec, rows: &Rows, arrival: u32, entry: &DirEntry<'_>) -> Option<u64> {
    if spec.column == SortColumn::Size && entry.is_browsable() {
        rows.sort_bytes(arrival).or(entry.size)
    } else {
        entry.size
    }
}

#[inline]
fn kind_of(spec: SortSpec, entry: &DirEntry<'_>) -> &'static str {
    if spec.column == SortColumn::Kind {
        format::kind(entry.name, entry.kind)
    } else {
        ""
    }
}

/// Reads one folder page by page, keeping its display order up to date. Blocking; lives on the
/// background executor between steps.
pub struct Loader {
    fs: Arc<dyn FileSystem>,
    dir: NodeId,
    stream: Option<Box<dyn DirStream>>,
    /// True once the stream ended, failed, or could not be opened.
    finished: bool,
    rows: Rows,
    order: Vec<u32>,
    /// `format::kind` of every row by arrival index, when sorting by kind; empty otherwise.
    kinds: Vec<&'static str>,
    /// The volume's folder totals, when its size table is built.
    sizes: Option<Arc<IndexSizes>>,
    sort: SortSpec,
    next_max: usize,
    /// Pages that needed a real merge (not the in-order append). For tests.
    merges: usize,
    /// Steps in a row that returned `More` with no entries.
    stalls: u32,
    /// What to leave out of the order.
    rule: Rule,
    /// True by arrival index for a row the rule hides; empty while the rule hides nothing.
    hides: Vec<bool>,
    /// Number of `true` in `hides`.
    hidden: usize,
}

impl Loader {
    /// A loader for the folder `dir`. Does no I/O.
    pub fn new(fs: Arc<dyn FileSystem>, dir: NodeId, sort: SortSpec) -> Self {
        Self {
            fs,
            dir,
            stream: None,
            finished: false,
            rows: Rows::default(),
            order: Vec::new(),
            kinds: Vec::new(),
            sizes: None,
            sort,
            next_max: FIRST_PAGE,
            merges: 0,
            stalls: 0,
            rule: Rule {
                visibility: Visibility::EVERYTHING,
                ..Rule::default()
            },
            hides: Vec::new(),
            hidden: 0,
        }
    }

    /// Leaves the rows `rule` hides out of every step's order (they stay in the rows).
    #[must_use]
    pub fn with_rule(mut self, rule: Rule) -> Self {
        self.rule = rule;
        self
    }

    /// The display order to report: the full order, less the rows the rule drops.
    fn visible_order(&self) -> Arc<[u32]> {
        if self.hidden == 0 || self.rule.reveal {
            return Arc::from(self.order.as_slice());
        }
        self.order
            .iter()
            .copied()
            .filter(|&arrival| !self.hides.get(arrival as usize).copied().unwrap_or(false))
            .collect()
    }

    /// Gives every folder row its total from `sizes` as its page arrives (see
    /// [`Step::totals`]), and orders by those totals when sorting by size.
    #[must_use]
    pub fn with_sizes(mut self, sizes: Option<Arc<IndexSizes>>) -> Self {
        if let Some(table) = &sizes {
            self.rows.set_lower_bound(table.is_lower_bound());
        }
        self.sizes = sizes;
        self
    }

    /// Blocking. Reads one page, orders it, merges it. Call only on the background executor.
    ///
    /// After a step that returned `Done` or `Failed`, further steps return `Done` with no page.
    pub fn step(&mut self) -> Step {
        if self.finished {
            return self.finish(None, Vec::new(), Outcome::Done);
        }
        if self.stream.is_none() {
            match self.fs.read_dir(self.dir) {
                Ok(stream) => self.stream = Some(stream),
                Err(error) => return self.finish(None, Vec::new(), Outcome::Failed(error)),
            }
        }
        let room = MAX_ROWS.saturating_sub(self.rows.len());
        if room == 0 {
            let error = Error::unsupported("more than 4,294,967,295 entries in one folder");
            return self.finish(None, Vec::new(), Outcome::Failed(error));
        }
        let Some(stream) = self.stream.as_mut() else {
            return self.finish(None, Vec::new(), Outcome::Done);
        };

        let max = self.next_max.min(room);
        self.next_max = self.next_max.saturating_mul(PAGE_GROWTH).min(LARGEST_PAGE);
        let mut batch = DirBatch::with_capacity(max, max.saturating_mul(NAME_BYTES_GUESS));
        let result = stream.next_batch(&mut batch, max);
        if batch.len() > room {
            // A stream that ignores `max` must not push the folder past the cap.
            let mut cut = DirBatch::with_capacity(room, 0);
            for entry in batch.iter().take(room) {
                cut.push(entry);
            }
            batch = cut;
        }

        // A stream that keeps answering "more" without giving anything would spin forever.
        if batch.is_empty() && matches!(result, Ok(DirProgress::More)) {
            self.stalls += 1;
            if self.stalls >= MAX_STALLS {
                let error = Error::unsupported("the folder listing stopped making progress");
                return self.finish(None, Vec::new(), Outcome::Failed(error));
            }
        } else {
            self.stalls = 0;
        }

        let page = (!batch.is_empty()).then(|| Arc::new(batch));
        let totals = page
            .as_ref()
            .map(|page| self.merge(page))
            .unwrap_or_default();
        match result {
            Ok(DirProgress::More) => Step {
                page,
                order: self.visible_order(),
                totals,
                hidden: self.hidden,
                outcome: Outcome::More,
            },
            Ok(DirProgress::Done) => self.finish(page, totals, Outcome::Done),
            Err(error) => self.finish(page, totals, Outcome::Failed(error)),
        }
    }

    /// Reads the folder to its end without stopping between pages, and gives back the rows, the
    /// final display order, how many rows the rule hides, and how it ended. For a refresh that
    /// swaps the whole listing in at once. Blocking: background executor only.
    pub fn run_to_end(mut self) -> (Rows, Arc<[u32]>, usize, Outcome) {
        loop {
            let step = self.step();
            if !matches!(step.outcome, Outcome::More) {
                return (self.rows, step.order, step.hidden, step.outcome);
            }
        }
    }

    /// Ends the listing: drops the stream and reports the final order.
    fn finish(
        &mut self,
        page: Option<Arc<DirBatch>>,
        totals: Vec<(u32, Totals)>,
        outcome: Outcome,
    ) -> Step {
        self.finished = true;
        self.stream = None;
        Step {
            page,
            order: self.visible_order(),
            totals,
            hidden: self.hidden,
            outcome,
        }
    }

    /// Adds `page` to the rows and its entries to the display order. Returns the totals of the
    /// page's folder rows.
    fn merge(&mut self, page: &Arc<DirBatch>) -> Vec<(u32, Totals)> {
        let start = self.rows.len;
        self.rows.push(Arc::clone(page));
        let totals = self
            .sizes
            .as_deref()
            .map(|table| resolve_page(table, start, page))
            .unwrap_or_default();
        self.rows.push_totals(totals.clone());
        let spec = self.sort;
        if self.rule.visibility != Visibility::EVERYTHING {
            self.hides.resize(start as usize, false);
            for entry in page.iter() {
                let hides = self.rule.hides(&entry);
                self.hidden += usize::from(hides);
                self.hides.push(hides);
            }
        }
        if spec.column == SortColumn::Kind {
            self.kinds.extend(
                page.iter()
                    .map(|entry| format::kind(entry.name, entry.kind)),
            );
        }

        let mut keys: Vec<Key<'_>> = (start..)
            .zip(page.iter())
            .map(|(arrival, entry)| Key {
                kind: kind_of(spec, &entry),
                size: size_of(spec, &self.rows, arrival, &entry),
                entry,
                arrival,
            })
            .collect();
        let in_order = keys
            .windows(2)
            .all(|pair| compare_keys(spec, &pair[0], &pair[1]) == Ordering::Less);
        if !in_order {
            keys.sort_unstable_by(|a, b| compare_keys(spec, a, b));
        }

        let lookup = |arrival: u32| -> Option<Key<'_>> {
            let entry = self.rows.get(arrival)?;
            Some(Key {
                kind: self.kinds.get(arrival as usize).copied().unwrap_or(""),
                size: size_of(spec, &self.rows, arrival, &entry),
                entry,
                arrival,
            })
        };

        // Fast path: the page goes after every row already ordered.
        let after_last = match (self.order.last(), keys.first()) {
            (Some(&last), Some(first)) => {
                lookup(last).is_none_or(|last| compare_keys(spec, &last, first) == Ordering::Less)
            }
            _ => true,
        };
        if after_last {
            self.order.extend(keys.iter().map(|key| key.arrival));
            return totals;
        }

        self.merges += 1;
        let mut merged = Vec::with_capacity(self.order.len() + keys.len());
        let mut new = keys.iter().peekable();
        for &old in &self.order {
            // Each old row is looked up once.
            if let Some(old_key) = lookup(old) {
                while let Some(next) = new.next_if(|key| compare_keys(spec, key, &old_key).is_lt())
                {
                    merged.push(next.arrival);
                }
            }
            merged.push(old);
        }
        merged.extend(new.map(|key| key.arrival));
        self.order = merged;
        totals
    }
}

/// A full ordering of `rows` for `spec` (used when the sort changes on a loaded folder).
/// Blocking, background only.
#[cfg(test)]
pub fn sort_rows(rows: &Rows, spec: SortSpec) -> Arc<[u32]> {
    sort_rows_visible(rows, spec, &Rule::default_everything()).0
}

/// [`sort_rows`] leaving out the rows `rule` drops, and counting the rows it hides (which is
/// more than it drops while the folder is revealed).
pub fn sort_rows_visible(rows: &Rows, spec: SortSpec, rule: &Rule) -> (Arc<[u32]>, usize) {
    let mut hidden = 0;
    let mut keys: Vec<Key<'_>> = rows
        .iter()
        .filter(|(_, entry)| {
            let hides = rule.hides(entry);
            hidden += usize::from(hides);
            !(hides && !rule.reveal)
        })
        .map(|(arrival, entry)| Key {
            kind: kind_of(spec, &entry),
            size: size_of(spec, rows, arrival, &entry),
            entry,
            arrival,
        })
        .collect();
    keys.sort_unstable_by(|a, b| compare_keys(spec, a, b));
    (keys.iter().map(|key| key.arrival).collect(), hidden)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::order::compare_entries;
    use disk_atlas_lib::error::Result;
    use disk_atlas_lib::fs::native::NativeFs;
    use disk_atlas_lib::fs::{EntryFlags, Metadata, NodeKind, Timestamp, VolumeInfo};
    use std::sync::Mutex;
    use std::time::Instant;

    /// One entry the fake filesystem reports.
    #[derive(Clone)]
    struct Fake {
        name: String,
        kind: NodeKind,
        size: Option<u64>,
        resource_size: Option<u64>,
        modified: Option<Timestamp>,
        flags: EntryFlags,
    }

    impl Fake {
        fn entry(&self, node: u64) -> DirEntry<'_> {
            DirEntry {
                node: NodeId(node),
                name: &self.name,
                kind: self.kind,
                size: self.size,
                resource_size: self.resource_size,
                modified: self.modified,
                flags: self.flags,
            }
        }
    }

    /// How the fake stream misbehaves.
    #[derive(Clone, Copy, Default)]
    struct Faults {
        fail_open: bool,
        /// Fail on this call (1-based) after appending this many entries.
        fail_on_call: Option<(usize, usize)>,
        /// Report at most this many entries per call, whatever `max` is.
        cap: Option<usize>,
    }

    struct FakeFs {
        info: VolumeInfo,
        entries: Arc<Vec<Fake>>,
        faults: Faults,
        maxes: Arc<Mutex<Vec<usize>>>,
    }

    impl FakeFs {
        fn new(entries: Vec<Fake>, faults: Faults) -> Self {
            // `VolumeInfo` is `#[non_exhaustive]`: borrow one from a real filesystem.
            let info = NativeFs::new(std::env::temp_dir(), "test")
                .expect("temp dir opens")
                .info();
            Self {
                info,
                entries: Arc::new(entries),
                faults,
                maxes: Arc::default(),
            }
        }
    }

    impl FileSystem for FakeFs {
        fn info(&self) -> VolumeInfo {
            self.info.clone()
        }
        fn root(&self) -> NodeId {
            NodeId(1)
        }
        fn metadata(&self, node: NodeId) -> Result<Metadata> {
            Err(Error::NotFound { node: node.0 })
        }
        fn lookup(&self, _dir: NodeId, _name: &str) -> Result<Option<NodeId>> {
            Ok(None)
        }
        fn read_dir(&self, dir: NodeId) -> Result<Box<dyn DirStream>> {
            if self.faults.fail_open {
                return Err(Error::NotADirectory { node: dir.0 });
            }
            Ok(Box::new(FakeStream {
                entries: Arc::clone(&self.entries),
                next: 0,
                calls: 0,
                faults: self.faults,
                maxes: Arc::clone(&self.maxes),
            }))
        }
    }

    struct FakeStream {
        entries: Arc<Vec<Fake>>,
        next: usize,
        calls: usize,
        faults: Faults,
        maxes: Arc<Mutex<Vec<usize>>>,
    }

    impl FakeStream {
        fn append(&mut self, out: &mut DirBatch, count: usize) {
            let end = (self.next + count).min(self.entries.len());
            for (i, fake) in self.entries[self.next..end].iter().enumerate() {
                out.push(fake.entry((self.next + i) as u64 + 100));
            }
            self.next = end;
        }
    }

    impl DirStream for FakeStream {
        fn next_batch(&mut self, out: &mut DirBatch, max: usize) -> Result<DirProgress> {
            self.maxes.lock().unwrap().push(max);
            self.calls += 1;
            if let Some((call, appended)) = self.faults.fail_on_call
                && call == self.calls
            {
                self.append(out, appended);
                return Err(Error::unsupported("broken folder"));
            }
            let count = self.faults.cap.map_or(max, |cap| cap.min(max));
            self.append(out, count);
            Ok(if self.next == self.entries.len() {
                DirProgress::Done
            } else {
                DirProgress::More
            })
        }
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// A name of 8 to 40 bytes: mixed case, digits, some non-ASCII.
    fn random_name(rng: &mut Rng) -> String {
        const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 ._-";
        let len = 8 + rng.below(33);
        let mut name = String::with_capacity(len);
        while name.len() < len {
            if rng.below(40) == 0 {
                name.push('\u{e4}');
            } else {
                name.push(CHARS[rng.below(CHARS.len())] as char);
            }
        }
        name
    }

    fn random_entries(count: usize, seed: u64) -> Vec<Fake> {
        let mut rng = Rng(seed);
        let mut entries: Vec<Fake> = (0..count)
            .map(|_| {
                let kind = match rng.below(10) {
                    0 => NodeKind::Directory,
                    1 => NodeKind::Symlink,
                    _ => NodeKind::File,
                };
                let flags = if kind == NodeKind::Symlink && rng.below(2) == 0 {
                    EntryFlags::LINKS_TO_DIRECTORY
                } else {
                    EntryFlags::EMPTY
                };
                let ext = [".png", ".txt", ".rs", "", ".zip", ".PDF"][rng.below(6)];
                Fake {
                    name: format!("{}{ext}", random_name(&mut rng)),
                    kind,
                    size: (rng.below(5) != 0).then(|| rng.next() % 1000),
                    resource_size: (rng.below(5) != 0).then(|| rng.next() % 50),
                    modified: (rng.below(5) != 0)
                        .then(|| Timestamp::from_unix_nanos((rng.next() % 50) as i64)),
                    flags,
                }
            })
            .collect();
        // A few exact duplicates: the arrival tie-break must still give one order.
        for i in 0..10 {
            let copy = entries[i].clone();
            entries.push(copy);
        }
        entries
    }

    fn all_specs() -> Vec<SortSpec> {
        let mut specs = Vec::new();
        for column in [
            SortColumn::Name,
            SortColumn::Modified,
            SortColumn::Size,
            SortColumn::ResourceFork,
            SortColumn::Kind,
        ] {
            for descending in [false, true] {
                specs.push(SortSpec { column, descending });
            }
        }
        specs
    }

    /// The plain full sort the loader must agree with.
    fn reference(entries: &[Fake], spec: SortSpec) -> Vec<u32> {
        let mut order: Vec<u32> = (0..entries.len() as u32).collect();
        order.sort_by(|&x, &y| {
            compare_entries(
                spec,
                &entries[x as usize].entry(0),
                &entries[y as usize].entry(0),
            )
            .then(x.cmp(&y))
        });
        order
    }

    /// Steps to the end; returns the loader, the rows the UI would have built, the last order,
    /// the last outcome and the number of steps.
    fn run(fs: Arc<FakeFs>, spec: SortSpec) -> (Loader, Rows, Arc<[u32]>, Outcome, usize) {
        let mut loader = Loader::new(fs, NodeId(1), spec);
        let mut rows = Rows::default();
        let mut steps = 0;
        loop {
            let step = loader.step();
            steps += 1;
            if let Some(page) = step.page {
                rows.push(page);
            }
            assert_eq!(step.order.len(), rows.len());
            if !matches!(step.outcome, Outcome::More) {
                return (loader, rows, step.order, step.outcome, steps);
            }
            assert!(steps < 10_000, "loader does not finish");
        }
    }

    #[test]
    fn loader_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Loader>();
        assert_send::<Step>();
        assert_send::<Rows>();
    }

    #[test]
    fn random_arrival_matches_a_full_sort() {
        let entries = random_entries(3_000, 7);
        for cap in [None, Some(300)] {
            let fs = Arc::new(FakeFs::new(
                entries.clone(),
                Faults {
                    cap,
                    ..Faults::default()
                },
            ));
            for spec in all_specs() {
                let (loader, rows, order, outcome, _) = run(Arc::clone(&fs), spec);
                assert!(matches!(outcome, Outcome::Done));
                assert_eq!(rows.len(), entries.len());
                assert_eq!(&*order, reference(&entries, spec).as_slice(), "{spec:?}");
                assert!(loader.merges > 0, "random arrival should need merges");
            }
        }
    }

    #[test]
    fn sorted_arrival_takes_the_fast_path() {
        let spec = SortSpec::default();
        let random = random_entries(3_000, 11);
        let sorted: Vec<Fake> = reference(&random, spec)
            .iter()
            .map(|&i| random[i as usize].clone())
            .collect();
        let fs = Arc::new(FakeFs::new(
            sorted.clone(),
            Faults {
                cap: Some(500),
                ..Faults::default()
            },
        ));
        let (loader, _, order, outcome, steps) = run(fs, spec);
        assert!(matches!(outcome, Outcome::Done));
        assert!(steps > 3);
        assert_eq!(loader.merges, 0);
        assert_eq!(&*order, reference(&sorted, spec).as_slice());
        // Arrival order is display order.
        assert!(order.iter().enumerate().all(|(i, &a)| a as usize == i));
    }

    #[test]
    fn page_sizes_grow_four_times_up_to_the_largest() {
        let fs = Arc::new(FakeFs::new(random_entries(40_000, 3), Faults::default()));
        let (_, rows, _, outcome, steps) = run(Arc::clone(&fs), SortSpec::default());
        assert!(matches!(outcome, Outcome::Done));
        assert_eq!(rows.len(), 40_010);
        assert_eq!(
            *fs.maxes.lock().unwrap(),
            [256, 1024, 4096, 16_384, 16_384, 16_384]
        );
        assert_eq!(steps, 6);
    }

    #[test]
    fn a_failing_stream_keeps_what_it_appended() {
        let entries = random_entries(2_000, 5);
        let fs = Arc::new(FakeFs::new(
            entries.clone(),
            Faults {
                fail_on_call: Some((3, 2)),
                ..Faults::default()
            },
        ));
        let mut loader = Loader::new(fs, NodeId(1), SortSpec::default());
        let mut rows = Rows::default();
        for _ in 0..2 {
            let step = loader.step();
            assert!(matches!(step.outcome, Outcome::More));
            rows.push(step.page.expect("a page"));
        }
        let step = loader.step();
        let page = step
            .page
            .expect("the two entries appended before the error");
        assert_eq!(page.len(), 2);
        assert!(matches!(
            step.outcome,
            Outcome::Failed(Error::Unsupported(_))
        ));
        rows.push(page);
        assert_eq!(rows.len(), 256 + 1024 + 2);
        assert_eq!(step.order.len(), rows.len());
        let read = &entries[..rows.len()];
        assert_eq!(
            &*step.order,
            reference(read, SortSpec::default()).as_slice()
        );
        // Finished: further steps do nothing.
        let after = loader.step();
        assert!(after.page.is_none());
        assert!(matches!(after.outcome, Outcome::Done));
        assert_eq!(after.order.len(), rows.len());
    }

    #[test]
    fn read_dir_failure_is_failed_with_no_page() {
        let fs = Arc::new(FakeFs::new(
            random_entries(10, 1),
            Faults {
                fail_open: true,
                ..Faults::default()
            },
        ));
        let mut loader = Loader::new(fs, NodeId(1), SortSpec::default());
        let step = loader.step();
        assert!(step.page.is_none());
        assert!(step.order.is_empty());
        assert!(matches!(
            step.outcome,
            Outcome::Failed(Error::NotADirectory { node: 1 })
        ));
    }

    fn named(name: &str, flags: EntryFlags) -> Fake {
        Fake {
            name: name.to_owned(),
            kind: NodeKind::File,
            size: Some(1),
            resource_size: None,
            modified: None,
            flags,
        }
    }

    /// Four files: plain, hidden dotfile, a housekeeping file marked hidden, one that is not.
    fn four() -> Arc<FakeFs> {
        let hidden = EntryFlags::HIDDEN;
        Arc::new(FakeFs::new(
            vec![
                named("a.txt", EntryFlags::EMPTY),
                named(".gitignore", hidden),
                named(".DS_Store", hidden),
                named("Thumbs.db", EntryFlags::EMPTY),
            ],
            Faults::default(),
        ))
    }

    fn rule(show_hidden: bool, show_system: bool, reveal: bool) -> Rule {
        Rule {
            visibility: Visibility {
                show_hidden,
                show_system,
            },
            at_root: false,
            reveal,
        }
    }

    fn visible_names(rule: Rule) -> (Vec<String>, usize) {
        let fs = four();
        let mut loader = Loader::new(
            Arc::clone(&fs) as Arc<dyn FileSystem>,
            NodeId(1),
            SortSpec::default(),
        )
        .with_rule(rule);
        let mut rows = Rows::default();
        let step = loop {
            let step = loader.step();
            if let Some(page) = &step.page {
                rows.push(Arc::clone(page));
            }
            if !matches!(step.outcome, Outcome::More) {
                break step;
            }
        };
        let names: Vec<String> = step
            .order
            .iter()
            .map(|&a| rows.get(a).unwrap().name.to_owned())
            .collect();
        // The same answer from a re-sort of the loaded rows.
        let (again, hidden) = sort_rows_visible(&rows, SortSpec::default(), &rule);
        assert_eq!(again, step.order);
        assert_eq!(hidden, step.hidden);
        (names, step.hidden)
    }

    #[test]
    fn the_two_switches_filter_a_listing_independently() {
        assert_eq!(
            visible_names(rule(false, false, false)),
            (vec!["a.txt".into()], 3)
        );
        assert_eq!(
            visible_names(rule(true, false, false)),
            (vec![".gitignore".into(), "a.txt".into()], 2)
        );
        assert_eq!(
            visible_names(rule(false, true, false)),
            (
                vec![".DS_Store".into(), "a.txt".into(), "Thumbs.db".into()],
                1
            )
        );
        assert_eq!(visible_names(rule(true, true, false)).0.len(), 4);
        assert_eq!(visible_names(rule(true, true, false)).1, 0);
    }

    #[test]
    fn a_revealed_folder_lists_everything_and_still_counts() {
        let (names, hidden) = visible_names(rule(false, false, true));
        assert_eq!(names.len(), 4);
        assert_eq!(hidden, 3);
    }
    #[test]
    fn empty_folder_is_done_with_an_empty_order() {
        let fs = Arc::new(FakeFs::new(Vec::new(), Faults::default()));
        let mut loader = Loader::new(fs, NodeId(1), SortSpec::default());
        let step = loader.step();
        assert!(step.page.is_none());
        assert!(step.order.is_empty());
        assert!(matches!(step.outcome, Outcome::Done));
    }

    #[test]
    fn sort_rows_agrees_with_the_loader() {
        let entries = random_entries(3_000, 13);
        let fs = Arc::new(FakeFs::new(entries, Faults::default()));
        let (_, rows, _, _, _) = run(Arc::clone(&fs), SortSpec::default());
        for spec in all_specs() {
            let (_, _, order, _, _) = run(Arc::clone(&fs), spec);
            assert_eq!(sort_rows(&rows, spec), order, "{spec:?}");
        }
        assert!(sort_rows(&Rows::default(), SortSpec::default()).is_empty());
    }

    #[test]
    fn rows_get_across_pages_and_out_of_range() {
        let entries = random_entries(40, 2);
        let mut rows = Rows::default();
        assert!(rows.is_empty());
        assert!(rows.get(0).is_none());
        let mut node = 0;
        for size in [3usize, 0, 1, 26, 20] {
            let mut batch = DirBatch::new();
            for _ in 0..size {
                batch.push(entries[node].entry(node as u64));
                node += 1;
            }
            rows.push(Arc::new(batch));
        }
        assert_eq!(rows.len(), 50);
        assert_eq!(rows.pages.len(), 4, "the empty page is not kept");
        for arrival in 0..50u32 {
            let entry = rows.get(arrival).expect("in range");
            assert_eq!(entry.node, NodeId(u64::from(arrival)));
            assert_eq!(entry.name, entries[arrival as usize].name);
        }
        assert!(rows.get(50).is_none());
        assert!(rows.get(u32::MAX).is_none());
        let via_iter: Vec<u32> = rows.iter().map(|(arrival, _)| arrival).collect();
        assert_eq!(via_iter, (0..50).collect::<Vec<_>>());
        assert!(
            rows.iter()
                .all(|(arrival, entry)| entry.node == NodeId(u64::from(arrival)))
        );
        // Clones share pages.
        let copy = rows.clone();
        assert!(Arc::ptr_eq(&copy.pages[0], &rows.pages[0]));
    }

    /// Lists 100,000 random names delivered in random order and prints the timings. Run with
    /// `cargo test -p disk-atlas --profile profiling -- --ignored perf_100k --nocapture`.
    #[test]
    #[ignore]
    #[allow(clippy::print_stdout)]
    fn perf_100k() {
        const COUNT: usize = 100_000;
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        let entries: Vec<Fake> = (0..COUNT)
            .map(|_| Fake {
                name: random_name(&mut rng),
                kind: if rng.below(20) == 0 {
                    NodeKind::Directory
                } else {
                    NodeKind::File
                },
                size: Some(rng.next() % 1_000_000),
                resource_size: None,
                modified: Some(Timestamp::from_unix_nanos((rng.next() >> 2) as i64)),
                flags: EntryFlags::EMPTY,
            })
            .collect();
        let fs = Arc::new(FakeFs::new(entries, Faults::default()));

        // The fake stream alone, for scale.
        let started = Instant::now();
        let mut stream = fs.read_dir(NodeId(1)).unwrap();
        let mut max = FIRST_PAGE;
        loop {
            let mut batch = DirBatch::with_capacity(max, max * NAME_BYTES_GUESS);
            let progress = stream.next_batch(&mut batch, max).unwrap();
            max = (max * PAGE_GROWTH).min(LARGEST_PAGE);
            if progress == DirProgress::Done {
                break;
            }
        }
        let stream_only = started.elapsed();

        for spec in [
            SortSpec::default(),
            SortSpec {
                column: SortColumn::Kind,
                descending: false,
            },
        ] {
            let started = Instant::now();
            let mut loader = Loader::new(Arc::clone(&fs) as Arc<dyn FileSystem>, NodeId(1), spec);
            let mut first = None;
            let mut steps = 0;
            loop {
                let step = loader.step();
                steps += 1;
                first.get_or_insert_with(|| started.elapsed());
                if !matches!(step.outcome, Outcome::More) {
                    assert_eq!(step.order.len(), COUNT);
                    break;
                }
            }
            let total = started.elapsed();
            let resort_started = Instant::now();
            let resorted = sort_rows(
                &loader.rows,
                SortSpec {
                    column: SortColumn::Modified,
                    descending: true,
                },
            );
            let resort = resort_started.elapsed();
            assert_eq!(resorted.len(), COUNT);
            println!(
                "perf_100k {spec:?}: first step {:.2} ms, all {steps} steps {:.2} ms \
                 (fake stream alone {:.2} ms, merges {}); sort_rows by date {:.2} ms",
                first.unwrap_or_default().as_secs_f64() * 1e3,
                total.as_secs_f64() * 1e3,
                stream_only.as_secs_f64() * 1e3,
                loader.merges,
                resort.as_secs_f64() * 1e3,
            );
        }
    }
}
