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

//! The rows of the current folder, their order, the selection and the load state.
//!
//! The UI half of the listing pipeline (see `architecture.md`, "Listing pipeline"). A load runs a
//! [`Loader`] on the background executor one step at a time; each step's page and order are
//! applied here with a push, a pointer swap and one notify. Every result carries the generation
//! it was started under and is dropped if the user has moved on.
//!
//! Search results are shown in place (`m4-search.md` §7.2): the folder view is put aside in a
//! [`FolderView`], the rows come from the index, and an entry number plays the part of the
//! arrival index, so the selection, the order and the cursor work unchanged.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{self, AtomicBool};
use std::time::{Duration, Instant};

use disk_atlas_lib::error::Error;
use disk_atlas_lib::fs::{DirEntry, FileSystem, JournalState, NodeId};
use disk_atlas_lib::search::{HEAD, HitOrder, Hits, Index, order_hits};
use disk_atlas_lib::sizes::{HostSizes, IndexSizes, Priority, Totals};
use gpui::{App, Context, Entity, EventEmitter, SharedString, Subscription, Task};

use super::loader::{Loader, Outcome, RowSource, Rows, Step, sort_rows_visible};
use super::menus::SourceFacts;
use super::navigation::{Crumb, Location, Navigation, NavigationEvent};
use super::order::{
    Positions, SortColumn, SortSpec, compare_tokens, position_in, starts_with_folded,
};
use super::selection::Selection;
use super::settings::Settings;
use super::sizes::{
    LiveResolved, Mode, ResortGuard, SizeState, VISIBLE_MAX, VISIBLE_SETTLE, resolve_live,
};
use super::sources::{Availability, SourceId, SourceKind, Sources};
use super::visibility::{Rule, Visibility};

/// The loading text appears only if no row has arrived after this long.
const LOADING_DELAY: Duration = Duration::from_millis(150);
/// Folders whose cursor row is remembered for when the user comes back.
const REMEMBERED_FOLDERS: usize = 256;
/// Results: the whole hit list is sorted once the view gets this close to the end of the head.
const SORT_AHEAD: usize = 512;
/// Results: folder paths kept for the Folder column; the cache is emptied when it is full.
const FOLDER_CACHE_CAP: usize = 4096;
/// Results on a host folder: filesystem ids kept for the hits the user has selected.
const RESOLVED_CAP: usize = 4096;
/// The separator of the Folder column's paths (a lead decision: the column is narrow and
/// middle-truncated, and a path reads best there).
const FOLDER_SEPARATOR: &str = "/";
/// The sort of fresh search results.
const RELEVANCE: SortSpec = SortSpec {
    column: SortColumn::Relevance,
    descending: false,
};

/// Where a load stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadState {
    /// No folder.
    Idle,
    /// Rows are arriving.
    Loading,
    /// The folder is listed (perhaps only in part; see [`Listing::note`]).
    Ready,
    /// The folder could not be opened. The text says why, for the user.
    Failed(SharedString),
    /// The source cannot be browsed (an unsupported format, a disk that needs administrator
    /// rights, a disk or image that failed to open). Nothing is loaded.
    Unavailable(Availability),
}

/// The load state a source that cannot be browsed puts the listing in, or `None` when the
/// source is browsable and its folder should be loaded.
fn unavailable_state(availability: &Availability) -> Option<LoadState> {
    match availability {
        Availability::Browsable => None,
        other => Some(LoadState::Unavailable(other.clone())),
    }
}

/// One row as the list draws it.
pub struct Row<'a> {
    /// The entry.
    pub entry: DirEntry<'a>,
    /// True when the row is selected.
    pub selected: bool,
    /// True when the keyboard cursor is on the row.
    pub cursor: bool,
    /// How the Size column shows a folder: its total as resolved when the listing loaded, or the
    /// host's live state read through the row's handle; [`SizeState::Unknown`] for a file.
    pub size: SizeState,
}

/// A cursor movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    /// One row up.
    Up,
    /// One row down.
    Down,
    /// This many rows up.
    PageUp(usize),
    /// This many rows down.
    PageDown(usize),
    /// The first row.
    First,
    /// The last row.
    Last,
}

/// What a mouse press on a row does to the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Click {
    /// Select only this row.
    Replace,
    /// Add or remove this row.
    Toggle,
    /// Select the range from the anchor to this row.
    Extend,
}

/// What the list view needs to hear about besides a redraw.
pub enum ListingEvent {
    /// The cursor moved by user action: the view should bring it into view.
    CursorMoved,
    /// A navigation while results were shown dropped them: the search is over.
    ResultsDropped,
}

impl EventEmitter<ListingEvent> for Listing {}

/// What the list shows: rows, order, sort, selection and load state. Plain data, so switching
/// between a folder and search results is testable without an entity.
struct Shown {
    rows: RowSource,
    order: Arc<[u32]>,
    /// The inverse of `order` when a background sort built one with it; used only while it is
    /// the inverse of this very `order` (see [`position_in`]).
    positions: Option<Positions>,
    sort: SortSpec,
    selection: Selection,
    /// Display position of the cursor, kept in step with `selection.cursor()` and `order`.
    cursor_position: Option<usize>,
    state: LoadState,
    note: Option<SharedString>,
    /// True after the load put the cursor on a row and the user has not moved it since: later
    /// pages may shift that row, and the view keeps it in sight.
    follow_cursor: bool,
    /// The row name to put the cursor on when it arrives.
    pending_focus: Option<SharedString>,
    /// True while a background re-sort of the folder rows runs: `order` is still the old sort's.
    resorting: bool,
    /// Rows the rule left out of `order` (folder rows only).
    hidden: usize,
    /// The rule `order` was made under.
    rule: Rule,
}

impl Shown {
    /// Nothing shown, sorted by `sort`.
    fn new(sort: SortSpec) -> Self {
        Self {
            rows: RowSource::default(),
            order: Arc::from([]),
            positions: None,
            sort,
            selection: Selection::new(),
            cursor_position: None,
            state: LoadState::Idle,
            note: None,
            follow_cursor: false,
            pending_focus: None,
            resorting: false,
            hidden: 0,
            rule: Rule::default_everything(),
        }
    }

    /// The display position of the cursor row (see [`Listing::find_cursor`]).
    fn find_cursor(&self) -> Option<usize> {
        let cursor = self.selection.cursor()?;
        position_in(&self.order, self.positions.as_ref(), cursor)
    }
}

/// The folder view put aside while search results are shown.
struct FolderView {
    rows: Rows,
    order: Arc<[u32]>,
    positions: Option<Positions>,
    sort: SortSpec,
    selection: Selection,
    cursor_position: Option<usize>,
    state: LoadState,
    note: Option<SharedString>,
    follow_cursor: bool,
    pending_focus: Option<SharedString>,
    resorting: bool,
    hidden: usize,
    rule: Rule,
}

/// What the listing still has to do after a [`FolderView`] is put back.
#[derive(Debug, PartialEq, Eq)]
enum Restored {
    /// The folder is back exactly as it was.
    Shown,
    /// The folder was being re-sorted when it was put aside: its order is the old sort's.
    Resort,
    /// The folder was still loading when it was put aside: load it again. The cursor name is
    /// in `pending_focus`.
    Reload,
}

impl FolderView {
    /// Moves the folder view out of `shown`, which is left empty with the same sort.
    fn take(shown: &mut Shown) -> Self {
        let old = std::mem::replace(shown, Shown::new(shown.sort));
        let rows = match old.rows {
            RowSource::Folder(rows) => rows,
            RowSource::Index(_) => Rows::default(),
        };
        Self {
            rows,
            order: old.order,
            positions: old.positions,
            sort: old.sort,
            selection: old.selection,
            cursor_position: old.cursor_position,
            state: old.state,
            note: old.note,
            follow_cursor: old.follow_cursor,
            pending_focus: old.pending_focus,
            resorting: old.resorting,
            hidden: old.hidden,
            rule: old.rule,
        }
    }

    /// The name of the cursor row, or the name the load was still waiting for.
    fn focus_name(&self) -> Option<SharedString> {
        self.selection
            .cursor()
            .and_then(|cursor| self.rows.get(cursor))
            .map(|entry| SharedString::from(entry.name.to_owned()))
            .or_else(|| self.pending_focus.clone())
    }

    /// Puts the folder view back into `shown`.
    fn restore(self, shown: &mut Shown) -> Restored {
        if self.state == LoadState::Loading {
            let focus = self.focus_name();
            *shown = Shown::new(self.sort);
            shown.pending_focus = focus;
            return Restored::Reload;
        }
        let restored = if self.resorting {
            Restored::Resort
        } else {
            Restored::Shown
        };
        *shown = Shown {
            rows: RowSource::Folder(self.rows),
            order: self.order,
            positions: self.positions,
            sort: self.sort,
            selection: self.selection,
            cursor_position: self.cursor_position,
            state: self.state,
            note: self.note,
            follow_cursor: self.follow_cursor,
            pending_focus: self.pending_focus,
            resorting: false,
            hidden: self.hidden,
            rule: self.rule,
        };
        restored
    }
}

/// The parent paths of result rows, keyed by the parent's entry number. Read during paint
/// through `&self`, hence the `RefCell`.
#[derive(Default)]
struct FolderCache {
    paths: RefCell<HashMap<u32, SharedString>>,
}

impl FolderCache {
    /// The path cached under `parent`, made and cached first when missing. A full cache is
    /// emptied before the insert, so it never holds more than [`FOLDER_CACHE_CAP`] paths.
    fn get_or_make(&self, parent: u32, make: impl FnOnce() -> SharedString) -> SharedString {
        if let Ok(paths) = self.paths.try_borrow()
            && let Some(path) = paths.get(&parent)
        {
            return path.clone();
        }
        let path = make();
        if let Ok(mut paths) = self.paths.try_borrow_mut() {
            if paths.len() >= FOLDER_CACHE_CAP {
                paths.clear();
            }
            paths.insert(parent, path.clone());
        }
        path
    }

    /// The path of the folder holding `entry`, root first; empty for the root itself.
    fn folder_of(&self, index: &Index, entry: u32) -> SharedString {
        let Some(parent) = index
            .parent(entry)
            .filter(|_| entry != 0 && entry < index.len())
        else {
            return SharedString::default();
        };
        self.get_or_make(parent, || {
            let mut path = String::new();
            index.folder_path(entry, FOLDER_SEPARATOR, &mut path);
            path.into()
        })
    }

    fn clear(&self) {
        if let Ok(mut paths) = self.paths.try_borrow_mut() {
            paths.clear();
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.paths.borrow().len()
    }
}

/// Search results shown in place of the folder.
struct Results {
    /// The folder to go back to.
    stashed: FolderView,
    hits: Arc<Hits>,
    terms: Arc<[String]>,
    folders: FolderCache,
    /// Set to stop the column sort in flight.
    cancel: Arc<AtomicBool>,
    /// The generation a `sort_all` of the hits runs under, if one does.
    sorting_all: Option<u64>,
    /// Real ids of hits on a host folder, found by [`Reach::resolve`] and handed back by the
    /// details pane and Quick Look. Empty for results of other sources.
    resolved: ResolvedIds,
}

/// Index entry number to the filesystem's own id for it, at most [`RESOLVED_CAP`] of them.
#[derive(Default)]
struct ResolvedIds {
    nodes: HashMap<u32, NodeId>,
}

impl ResolvedIds {
    fn get(&self, entry: u32) -> Option<NodeId> {
        self.nodes.get(&entry).copied()
    }

    /// A full table is emptied before the insert, so it never holds more than the cap.
    fn insert(&mut self, entry: u32, node: NodeId) {
        if self.nodes.len() >= RESOLVED_CAP && !self.nodes.contains_key(&entry) {
            self.nodes.clear();
        }
        self.nodes.insert(entry, node);
    }
}

/// True unless `packages_as_folders` is on: packages are one item.
fn packages_on(cx: &App) -> bool {
    cx.try_global::<crate::state::settings::Settings>()
        .is_none_or(|settings| !settings.bool_of("packages_as_folders"))
}

/// True when the entry is a package (a folder with a package's extension or the bundle bit).
fn is_package_entry(entry: &disk_atlas_lib::fs::DirEntry<'_>) -> bool {
    disk_atlas_lib::mac::bundle::is_package(entry.name, entry.kind, entry.flags)
}

/// How the one selected row is reached on its filesystem. Ids in an index built over a host
/// folder are links inside that index only (`FIRST_SCAN_ID`): the filesystem answers `NotFound`
/// to them. A hit there is reached by its names, which [`Reach::resolve`] walks on a background
/// thread.
#[derive(Clone)]
pub enum Reach {
    /// The row's own id is the filesystem's.
    Node(NodeId),
    /// A hit of an index over a host folder.
    Hit {
        index: Arc<Index>,
        /// The entry number in `index`.
        entry: u32,
        /// Which results this hit belongs to (see [`Listing::remember_resolved`]).
        epoch: u64,
        /// The real id, when an earlier [`Reach::resolve`] found it.
        known: Option<NodeId>,
    },
}

/// What tells one selected row from another: equal while the same row of the same results (or
/// the same node of a folder) stays selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReachKey {
    /// A node of the filesystem.
    Node(NodeId),
    /// A hit: its results and its entry number there.
    Hit { epoch: u64, entry: u32 },
}

impl Reach {
    /// The row's identity, cheap to compare.
    pub fn key(&self) -> ReachKey {
        match self {
            Self::Node(node) => ReachKey::Node(*node),
            Self::Hit { epoch, entry, .. } => ReachKey::Hit {
                epoch: *epoch,
                entry: *entry,
            },
        }
    }

    /// The filesystem's id for the row, when it is known without a lookup.
    pub fn known(&self) -> Option<NodeId> {
        match self {
            Self::Node(node) => Some(*node),
            Self::Hit { known, .. } => *known,
        }
    }

    /// A stand-in id for a hit not resolved yet: the index's own, which the filesystem rejects.
    pub fn provisional(&self) -> NodeId {
        match self {
            Self::Node(node) => *node,
            Self::Hit {
                index,
                entry,
                known,
                ..
            } => known
                .or_else(|| index.entry(*entry).map(|row| row.node))
                .unwrap_or(NodeId(0)),
        }
    }

    /// The filesystem's id for the row: `Ok(None)` when the file is gone. A hit costs one
    /// `lookup` per name on its path (and nothing once [`Reach::known`]). Blocking; call it on
    /// the background executor.
    pub fn resolve(&self, fs: &dyn FileSystem) -> Result<Option<NodeId>, Error> {
        match self {
            Self::Node(node) => Ok(Some(*node)),
            Self::Hit {
                known: Some(node), ..
            } => Ok(Some(*node)),
            Self::Hit { index, entry, .. } => {
                let walked = reach_path(fs, index.crumbs(*entry))?;
                Ok(walked.and_then(|path| path.last().map(|(node, _)| *node)))
            }
        }
    }
}

/// An index path (root first) with the filesystem's own id on every step instead of the index's:
/// the root is `fs.root()`, each later step is found by name with one `lookup` in the step
/// before, so the walk costs one lookup per level. `Ok(None)` when a step is gone. Blocking.
fn reach_path(
    fs: &dyn FileSystem,
    path: Vec<(NodeId, String)>,
) -> Result<Option<Vec<(NodeId, String)>>, Error> {
    let mut walked = Vec::with_capacity(path.len());
    let mut at = fs.root();
    for (depth, (_, name)) in path.into_iter().enumerate() {
        if depth > 0 {
            match fs.lookup(at, &name)? {
                Some(next) => at = next,
                None => return Ok(None),
            }
        }
        walked.push((at, name));
    }
    Ok(Some(walked))
}

/// The engine order for a column of the results, or `None` for the relevance order.
fn hit_order(column: SortColumn) -> Option<HitOrder> {
    match column {
        SortColumn::Name => Some(HitOrder::Name),
        SortColumn::Modified => Some(HitOrder::Modified),
        SortColumn::Size => Some(HitOrder::Size),
        SortColumn::ResourceFork => Some(HitOrder::ResourceSize),
        SortColumn::Kind => Some(HitOrder::Kind),
        SortColumn::Folder => Some(HitOrder::Folder),
        SortColumn::Relevance => None,
    }
}

/// The whole order of results and its inverse, made on the background executor: by a column
/// through the engine's `order_hits`, or by relevance (`by` is `None`) as the hits hold it. `None`
/// when `cancel` was set: a newer sort or newer results replaced these.
fn results_order(
    index: &Index,
    hits: &Hits,
    by: Option<HitOrder>,
    descending: bool,
    cancel: &AtomicBool,
) -> Option<(Arc<[u32]>, Option<Positions>)> {
    let order = match by {
        Some(by) => order_hits(index, hits, by, descending, cancel)?,
        None => hits.entries_in_order(),
    };
    if cancel.load(atomic::Ordering::Relaxed) {
        return None;
    }
    let positions = Positions::new(Arc::clone(&order));
    Some((order, positions))
}

/// True when a folder listing can be sorted by `column`: `Relevance` and `Folder` exist only in
/// search results.
fn folder_column(column: SortColumn) -> bool {
    !matches!(column, SortColumn::Relevance | SortColumn::Folder)
}

/// True when the results should be sorted in full: part of them is still unsorted and the view
/// has come within [`SORT_AHEAD`] rows of the sorted head's end.
fn needs_sort_all(sorted_len: usize, len: usize, last_visible: usize) -> bool {
    sorted_len < len && last_visible.saturating_add(SORT_AHEAD) >= sorted_len
}

/// The crumbs of a location from an index path (root first), with the root renamed to the
/// source's label (the index holds the volume name). `None` for an empty path: an entry nested
/// deeper than the index can name.
fn result_crumbs(path: Vec<(NodeId, String)>, root_name: &SharedString) -> Option<Vec<Crumb>> {
    if path.is_empty() {
        return None;
    }
    Some(
        path.into_iter()
            .enumerate()
            .map(|(depth, (node, name))| Crumb {
                node,
                name: if depth == 0 {
                    root_name.clone()
                } else {
                    name.into()
                },
            })
            .collect(),
    )
}

/// The current folder's rows (or search results), their order, the sort, the selection and the
/// load state.
pub struct Listing {
    navigation: Entity<Navigation>,
    /// Looked up on every navigation for the availability of the source shown.
    sources: Entity<Sources>,
    /// The folder listed. While results are shown, the folder they were started from.
    location: Option<Location>,
    /// Whether the volume of `location` has resource forks, read once per navigation.
    resource_forks: bool,
    local_times: bool,
    /// What mounting did with the volume's journal, read once per navigation.
    journal: Option<JournalState>,
    /// True when the source of `location` is the host: ids in its search index are scan-local.
    host: bool,
    /// Identifies the results shown; bumped when results start or their index changes.
    hit_epoch: u64,
    /// Bumped by every load, every re-sort and every change of results; results from an older
    /// one are dropped.
    generation: u64,
    shown: Shown,
    /// Set while search results are shown.
    results: Option<Results>,
    /// Set by the 150 ms timer when nothing has arrived yet.
    loading_visible: bool,
    /// The cursor's row name in folders left, most recent last.
    remembered: VecDeque<((SourceId, NodeId), SharedString)>,
    started: Instant,
    first_page: Option<Duration>,
    load_task: Option<Task<()>>,
    sort_task: Option<Task<()>>,
    sort_all_task: Option<Task<()>>,
    /// The walk that opens a hit on a host folder.
    activate_task: Option<Task<()>>,
    loading_timer: Option<Task<()>>,
    /// The size table of each volume whose index is built (the app's `Sizes` hands them over).
    tables: HashMap<SourceId, Arc<IndexSizes>>,
    /// Bumped by every load: totals resolved for an older load are dropped.
    load_epoch: u64,
    /// True when the load in progress (or last) had its source's table from its first page.
    loaded_with_table: bool,
    /// The task resolving the totals of rows that were loaded before their table.
    resolve_task: Option<Task<()>>,
    /// When a re-sort by size may run (see [`ResortGuard`]).
    resort: ResortGuard,
    /// True while a re-sort by size waits for the guard.
    resort_wanted: bool,
    resort_timer: Option<Task<()>>,
    /// The folder whose hidden items the status bar revealed; it ends when the user leaves it.
    revealed: Option<(SourceId, NodeId)>,
    /// The switches `shown` was last made under.
    visibility: Visibility,
    /// The live size tree of each host source that has one (the app's `Sizes` hands them over).
    hosts: HashMap<SourceId, Arc<HostSizes>>,
    /// True while the folder rows may still gain handles into the host tree: some were not in it
    /// when they were looked up, and the folder is not finished being measured.
    live_pending: bool,
    /// True while the rows are being looked up in the host tree.
    live_resolving: bool,
    live_task: Option<Task<()>>,
    /// True while the folder rows were measured at the last re-sort (so one more follows when the
    /// measuring ends).
    live_was_running: bool,
    /// The refresh in progress that reads the folder again and merges it by name.
    merge_task: Option<Task<()>>,
    /// Bumped by every merge and by every load: an older merge result is dropped.
    merge_generation: u64,
    /// A change was reported while the folder was still loading: merge when it is loaded.
    refresh_after_load: bool,
    /// The visible rows the settle timer is waiting on, and the timer.
    visible: Range<usize>,
    visible_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Listing {
    /// Follows `navigation` and lists the folder it shows. A source that `sources` says cannot be
    /// browsed is not listed: the state says why instead.
    pub fn new(
        navigation: Entity<Navigation>,
        sources: Entity<Sources>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.subscribe(
            &navigation,
            |this, _navigation, event: &NavigationEvent, cx| match event {
                NavigationEvent::Changed { focus } => this.load(focus.clone(), cx),
            },
        );
        let mut listing = Self {
            navigation,
            sources,
            location: None,
            resource_forks: false,
            local_times: false,
            journal: None,
            host: false,
            hit_epoch: 0,
            generation: 0,
            shown: Shown::new(SortSpec::default()),
            results: None,
            loading_visible: false,
            remembered: VecDeque::new(),
            started: Instant::now(),
            first_page: None,
            load_task: None,
            sort_task: None,
            sort_all_task: None,
            activate_task: None,
            loading_timer: None,
            tables: HashMap::new(),
            load_epoch: 0,
            loaded_with_table: false,
            resolve_task: None,
            resort: ResortGuard::default(),
            resort_wanted: false,
            resort_timer: None,
            revealed: None,
            visibility: Visibility::current(cx),
            hosts: HashMap::new(),
            live_pending: false,
            live_resolving: false,
            live_task: None,
            live_was_running: false,
            merge_task: None,
            merge_generation: 0,
            refresh_after_load: false,
            visible: 0..0,
            visible_task: None,
            _subscriptions: vec![
                subscription,
                cx.observe_global::<Settings>(|this, cx| this.visibility_changed(cx)),
            ],
        };
        if listing.navigation.read(cx).current().is_some() {
            listing.load(None, cx);
        }
        listing
    }

    /// The folder listed. While results are shown, the folder the search started from.
    pub fn location(&self) -> Option<&Location> {
        self.location.as_ref()
    }

    /// True when the volume shown has resource forks: the list gets a Resource fork column.
    pub fn resource_forks(&self) -> bool {
        self.resource_forks
    }

    /// Whether the volume's dates carry no time zone and are shown as stored.
    pub fn local_times(&self) -> bool {
        self.local_times
    }

    /// What mounting did with the journal of the volume shown, when the format has one.
    pub fn journal(&self) -> Option<&JournalState> {
        self.journal.as_ref()
    }

    /// Where the load stands. Search results are always `Ready`.
    pub fn state(&self) -> &LoadState {
        &self.shown.state
    }

    /// A sentence to show with the rows when only part of the folder could be read.
    pub fn note(&self) -> Option<&SharedString> {
        self.shown.note.as_ref()
    }

    /// True while loading with no rows yet and 150 ms have passed.
    pub fn show_loading(&self) -> bool {
        self.loading_visible && self.shown.state == LoadState::Loading && self.shown.rows.is_empty()
    }

    /// Number of rows shown.
    pub fn len(&self) -> usize {
        self.shown.order.len()
    }

    /// True when no row is shown.
    pub fn is_empty(&self) -> bool {
        self.shown.order.is_empty()
    }

    /// The rule a folder load or re-sort applies now.
    fn rule(&self, cx: &App) -> Rule {
        let key = self
            .location
            .as_ref()
            .map(|location| (location.source(), location.node()));
        Rule {
            visibility: Visibility::current(cx),
            at_root: self
                .location
                .as_ref()
                .is_some_and(|location| location.node() == location.fs().root()),
            reveal: key.is_some() && key == self.revealed,
        }
    }

    /// Folder rows the switches hide (shown anyway while the folder is revealed). 0 in results.
    pub fn hidden_count(&self) -> usize {
        match self.shown.rows {
            RowSource::Folder(_) => self.shown.hidden,
            RowSource::Index(_) => 0,
        }
    }

    /// True while the status bar's button has revealed this folder's hidden items.
    pub fn revealed(&self) -> bool {
        self.results.is_none() && self.shown.rule.reveal
    }

    /// The housekeeping class of the folder row `row`, to draw it dimmed with its own kind text.
    /// Only when the rule lets housekeeping items show at all: the row is in the list.
    pub fn system_of(&self, row: &Row<'_>) -> Option<disk_atlas_lib::mac::SystemItem> {
        match self.shown.rows {
            RowSource::Folder(_) => self.shown.rule.system_of(&row.entry),
            RowSource::Index(_) => None,
        }
    }

    /// The status bar's button: shows the hidden items of this folder until the user leaves it,
    /// or hides them again.
    pub fn toggle_reveal(&mut self, cx: &mut Context<Self>) {
        let Some(location) = &self.location else {
            return;
        };
        if self.results.is_some() || self.shown.state != LoadState::Ready {
            return;
        }
        let key = (location.source(), location.node());
        self.revealed = (self.revealed != Some(key)).then_some(key);
        self.rebuild_order(cx);
    }

    /// A setting changed: when the two switches did, the folder is filtered again. Results are
    /// not (the engine query takes the setting when the search runs), and the folder put aside
    /// behind them is brought up to date when they are left.
    fn visibility_changed(&mut self, cx: &mut Context<Self>) {
        let now = Visibility::current(cx);
        if now == self.visibility {
            return;
        }
        self.visibility = now;
        if self.results.is_none() {
            self.rebuild_order(cx);
        }
    }

    /// Makes the folder's order again under the current rule. A selection of rows that may now be
    /// gone is dropped.
    fn rebuild_order(&mut self, cx: &mut Context<Self>) {
        match self.shown.state {
            LoadState::Ready => {
                self.shown.selection.reset();
                self.shown.cursor_position = None;
                self.spawn_folder_sort(cx);
            }
            LoadState::Loading => {
                self.shown.pending_focus = self.cursor_name();
                self.start_load(cx);
            }
            _ => {}
        }
    }

    /// The row at a display position. O(1) apart from the page lookup; called for visible rows
    /// during paint.
    pub fn row(&self, position: usize) -> Option<Row<'_>> {
        let id = *self.shown.order.get(position)?;
        Some(Row {
            entry: self.shown.rows.get(id)?,
            selected: self.shown.selection.is_selected(id),
            cursor: self.shown.selection.cursor() == Some(id),
            size: match &self.shown.rows {
                RowSource::Folder(rows) => rows.size_state(id),
                RowSource::Index(_) => SizeState::Unknown,
            },
        })
    }

    /// The bytes of the selected rows, for the status bar: files by size, folders by total. `None`
    /// unless a selected row is a folder with a total. Reads every row once, so the caller asks
    /// only when a selection exists.
    pub fn selection_total(&self) -> Option<u64> {
        let RowSource::Folder(rows) = &self.shown.rows else {
            return None;
        };
        if self.shown.selection.count() == 0 {
            return None;
        }
        let mut total = 0u64;
        let mut any = false;
        for (arrival, totals) in rows.totals() {
            if self.shown.selection.is_selected(*arrival) {
                total = total.saturating_add(totals.bytes);
                any = true;
            }
        }
        if !any {
            return None;
        }
        for (arrival, entry) in rows.iter() {
            if !entry.is_browsable() && self.shown.selection.is_selected(arrival) {
                total = total.saturating_add(entry.size.unwrap_or(0));
            }
        }
        Some(total)
    }

    /// The size table of `source` arrived (or went: `None`). Rows already listed get their totals
    /// from it in the background, and a list sorted by size is ordered again once.
    pub fn set_table(
        &mut self,
        source: SourceId,
        table: Option<Arc<IndexSizes>>,
        cx: &mut Context<Self>,
    ) {
        match table {
            Some(table) => self.tables.insert(source, table),
            None => self.tables.remove(&source),
        };
        if self.location.as_ref().map(Location::source) != Some(source) {
            return;
        }
        // While loading, `apply` resolves them when the last page is here.
        if self.shown.state == LoadState::Ready {
            self.spawn_resolve(cx);
        }
    }

    /// The table of the source shown.
    fn current_table(&self) -> Option<Arc<IndexSizes>> {
        let source = self.location.as_ref()?.source();
        self.tables.get(&source).cloned()
    }

    /// The live size tree of the host source `source` arrived (or went: `None`; its rows lose
    /// their handles). The rows already listed look themselves up in it in the background.
    pub fn set_host(
        &mut self,
        source: SourceId,
        sizes: Option<Arc<HostSizes>>,
        cx: &mut Context<Self>,
    ) {
        let gone = sizes.is_none();
        match sizes {
            Some(sizes) => self.hosts.insert(source, sizes),
            None => self.hosts.remove(&source),
        };
        if self.location.as_ref().map(Location::source) != Some(source) {
            return;
        }
        if gone {
            if let RowSource::Folder(rows) = &mut self.shown.rows {
                rows.set_live(None);
            }
            cx.notify();
        } else if self.shown.state == LoadState::Ready {
            // While loading, `apply` looks the rows up when the last page is here.
            self.spawn_resolve_live(cx);
        }
    }

    /// What `Sizes` follows: the host folder shown (its source, node and the number of the load
    /// that listed it, which changes on every load and every merge), or `None` for any other view.
    pub fn host_view_key(&self) -> Option<(SourceId, NodeId, u64)> {
        if !self.host || self.results.is_some() {
            return None;
        }
        let location = self.location.as_ref()?;
        Some((location.source(), location.node(), self.load_epoch))
    }

    /// The tree, source and path of the host folder shown, when it has a tree. Reads memory only.
    fn host_ctx(&self, cx: &App) -> Option<(SourceId, Arc<HostSizes>, std::path::PathBuf)> {
        if !self.host || self.results.is_some() {
            return None;
        }
        let source = self.location.as_ref()?.source();
        let sizes = Arc::clone(self.hosts.get(&source)?);
        Some((source, sizes, self.host_folder_path(cx)?))
    }

    /// Looks the folder rows up in the host tree on the background executor (one path join and
    /// one lookup per folder row), then draws them from their handles.
    fn spawn_resolve_live(&mut self, cx: &mut Context<Self>) {
        if self.live_resolving {
            return;
        }
        let Some((source, sizes, folder)) = self.host_ctx(cx) else {
            return;
        };
        let RowSource::Folder(rows) = &self.shown.rows else {
            return;
        };
        let rows = rows.clone();
        let epoch = self.load_epoch;
        self.live_resolving = true;
        self.live_task = Some(cx.spawn(async move |this, cx| {
            let resolved = cx
                .background_executor()
                .spawn(async move { resolve_live(&sizes, source, &folder, &rows) })
                .await;
            this.update(cx, |this, cx| this.apply_live(epoch, resolved, cx))
                .ok();
        }));
    }

    /// Gives the rows their handles. A list sorted by size is ordered again when more rows
    /// have one than before.
    fn apply_live(&mut self, epoch: u64, resolved: LiveResolved, cx: &mut Context<Self>) {
        if epoch != self.load_epoch {
            return;
        }
        self.live_resolving = false;
        self.live_pending = resolved.pending();
        let RowSource::Folder(rows) = &mut self.shown.rows else {
            return;
        };
        let more = resolved.live.resolved() != rows.live_count();
        rows.set_live(Some(Arc::new(resolved.live)));
        if more && self.shown.sort.column == SortColumn::Size {
            self.resort_wanted = true;
            self.resort_by_size(cx);
        }
        cx.notify();
    }

    /// The tree of `source` has news (totals moved, or a folder was queued): rows that were not
    /// in the tree when they were looked up are looked up again, a list sorted by size is ordered
    /// again while the folder is being measured (and once more when that ends), and the list
    /// repaints. `Sizes` calls it at most every `SIZE_REPAINT`.
    pub fn host_tick(&mut self, source: SourceId, cx: &mut Context<Self>) {
        if !self.host
            || self.results.is_some()
            || self.location.as_ref().map(Location::source) != Some(source)
            || self.shown.state != LoadState::Ready
        {
            return;
        }
        if self.live_pending {
            self.spawn_resolve_live(cx);
        }
        if self.shown.sort.column == SortColumn::Size
            && let RowSource::Folder(rows) = &self.shown.rows
        {
            let running = rows.live_folder_running();
            if running || self.live_was_running {
                self.resort_wanted = true;
                self.resort_by_size(cx);
            }
            self.live_was_running = running;
        }
        cx.notify();
    }

    /// The list view reports the rows on screen. Once they have stayed put for
    /// [`VISIBLE_SETTLE`], the folders among them are queued `Visible` in the host tree. The
    /// path work runs on the background executor.
    pub fn note_visible(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        if range == self.visible {
            return;
        }
        self.visible = range;
        self.visible_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(VISIBLE_SETTLE).await;
            this.update(cx, |this, cx| this.request_visible(cx)).ok();
        }));
    }

    fn request_visible(&mut self, cx: &mut Context<Self>) {
        if Mode::current(cx) == Mode::Off {
            return;
        }
        let Some((_, sizes, folder)) = self.host_ctx(cx) else {
            return;
        };
        let mut names: Vec<String> = Vec::new();
        for position in self.visible.clone() {
            if names.len() >= VISIBLE_MAX {
                break;
            }
            if let Some(row) = self.row(position)
                && row.entry.is_browsable()
            {
                names.push(row.entry.name.to_owned());
            }
        }
        if names.is_empty() {
            return;
        }
        cx.background_executor()
            .spawn(async move {
                for name in names {
                    sizes.request(&folder.join(name), Priority::Visible);
                }
            })
            .detach();
    }

    /// The watcher says the folder shown changed: reads it again in the background and swaps the
    /// result in, merged by name. The selection, the cursor and the scroll position stay on the
    /// same names; names that vanished leave the selection; new names land in sort order. No
    /// animation. Unlike a load, the rows on screen stay until the new ones are all here.
    pub fn reload_in_place(&mut self, cx: &mut Context<Self>) {
        if !self.host || self.results.is_some() {
            return;
        }
        let Some(location) = &self.location else {
            return;
        };
        match self.shown.state {
            LoadState::Ready => {}
            // The load in progress may have read the folder before the change: read it again
            // when it is done.
            LoadState::Loading => {
                self.refresh_after_load = true;
                return;
            }
            _ => return,
        }
        let fs = Arc::clone(location.fs());
        let dir = location.node();
        let sort = self.shown.sort;
        let rule = self.rule(cx);
        let live = self.host_ctx(cx);
        self.merge_generation += 1;
        let (epoch, generation) = (self.load_epoch, self.merge_generation);
        self.merge_task = Some(cx.spawn(async move |this, cx| {
            let merged = cx
                .background_executor()
                .spawn(async move {
                    let (mut rows, mut order, mut hidden, outcome) =
                        Loader::new(fs, dir, sort).with_rule(rule).run_to_end();
                    let note = match outcome {
                        Outcome::Failed(error) => {
                            // The folder is gone or unreadable: leave what is on screen.
                            if rows.is_empty() {
                                tracing::debug!(%error, "folder refresh failed");
                                return None;
                            }
                            Some(partial_text(&error))
                        }
                        Outcome::Done | Outcome::More => None,
                    };
                    let mut pending = false;
                    if let Some((source, sizes, folder)) = live {
                        let resolved = resolve_live(&sizes, source, &folder, &rows);
                        pending = resolved.pending();
                        rows.set_live(Some(Arc::new(resolved.live)));
                        if sort.column == SortColumn::Size {
                            (order, hidden) = sort_rows_visible(&rows, sort, &rule);
                        }
                    }
                    let positions = Positions::new(Arc::clone(&order));
                    Some(Merged {
                        rows,
                        order,
                        positions,
                        hidden,
                        note,
                        pending,
                    })
                })
                .await;
            this.update(cx, |this, cx| {
                this.apply_merge(epoch, generation, sort, rule, merged, cx)
            })
            .ok();
        }));
    }

    /// Swaps in the rows a refresh read, keeping the selection and the cursor on their names.
    fn apply_merge(
        &mut self,
        epoch: u64,
        generation: u64,
        sort: SortSpec,
        rule: Rule,
        merged: Option<Merged>,
        cx: &mut Context<Self>,
    ) {
        if epoch != self.load_epoch
            || generation != self.merge_generation
            || self.results.is_some()
            || self.shown.state != LoadState::Ready
        {
            return;
        }
        let Some(merged) = merged else {
            return;
        };
        let RowSource::Folder(old) = &self.shown.rows else {
            return;
        };
        let selection = merge_by_name(
            old,
            &self.shown.order,
            &self.shown.selection,
            &merged.rows,
            &merged.order,
        );
        self.shown.rows = RowSource::Folder(merged.rows);
        self.shown.order = merged.order;
        self.shown.positions = merged.positions;
        self.shown.hidden = merged.hidden;
        self.shown.rule = rule;
        self.shown.selection = selection;
        self.shown.note = merged.note;
        self.shown.resorting = false;
        self.shown.follow_cursor = false;
        self.shown.pending_focus = None;
        // The rows have new arrival numbers: whatever was in flight for the old ones is dropped.
        self.generation += 1;
        self.load_epoch += 1;
        self.sort_task = None;
        self.resolve_task = None;
        self.live_task = None;
        self.live_resolving = false;
        self.live_pending = merged.pending;
        self.shown.cursor_position = self.find_cursor();
        // The sort or the rule changed while the folder was being read.
        if sort != self.shown.sort || rule != self.rule(cx) {
            self.spawn_folder_sort(cx);
        }
        cx.notify();
    }

    /// Resolves the totals of every folder row on the background executor (the table arrived
    /// after the rows, or went).
    fn spawn_resolve(&mut self, cx: &mut Context<Self>) {
        let rows = match (&self.results, &self.shown.rows) {
            (Some(results), _) => results.stashed.rows.clone(),
            (None, RowSource::Folder(rows)) => rows.clone(),
            (None, RowSource::Index(_)) => return,
        };
        let table = self.current_table();
        let epoch = self.load_epoch;
        self.resolve_task = Some(cx.spawn(async move |this, cx| {
            let resolved = cx
                .background_executor()
                .spawn(async move {
                    let Some(table) = table else {
                        return (Vec::new(), false);
                    };
                    let mut found: Vec<(u32, Totals)> = Vec::new();
                    for (arrival, entry) in rows.iter() {
                        if !entry.is_browsable() {
                            continue;
                        }
                        if let Some(totals) = table
                            .folder_entry(entry.node)
                            .and_then(|folder| table.totals(folder))
                        {
                            found.push((arrival, totals));
                        }
                    }
                    (found, table.is_lower_bound())
                })
                .await;
            this.update(cx, |this, cx| {
                this.apply_totals(epoch, resolved.0, resolved.1, cx)
            })
            .ok();
        }));
    }

    /// Puts resolved totals on the rows, and orders them again once when the list is sorted by
    /// size.
    fn apply_totals(
        &mut self,
        epoch: u64,
        items: Vec<(u32, Totals)>,
        lower_bound: bool,
        cx: &mut Context<Self>,
    ) {
        if epoch != self.load_epoch {
            return;
        }
        if let Some(results) = self.results.as_mut() {
            results.stashed.rows.set_totals(items, lower_bound);
            if self.shown.sort.column == SortColumn::Size {
                // The folder is ordered again when the results are left.
                results.stashed.resorting = true;
            }
            return;
        }
        let RowSource::Folder(rows) = &mut self.shown.rows else {
            return;
        };
        rows.set_totals(items, lower_bound);
        self.loaded_with_table = true;
        if self.shown.sort.column == SortColumn::Size {
            self.resort_wanted = true;
            self.resort_by_size(cx);
        }
        cx.notify();
    }

    /// Orders the folder again by size when the guard allows, else when it will.
    fn resort_by_size(&mut self, cx: &mut Context<Self>) {
        if !self.resort_wanted
            || self.results.is_some()
            || self.shown.state != LoadState::Ready
            || self.shown.sort.column != SortColumn::Size
        {
            return;
        }
        let now = Instant::now();
        if let Some(wait) = self.resort.wait(now) {
            self.resort_timer = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(wait).await;
                this.update(cx, |this, cx| this.resort_by_size(cx)).ok();
            }));
            return;
        }
        self.resort_wanted = false;
        self.resort_timer = None;
        self.resort.resorted(now);
        self.spawn_folder_sort(cx);
    }

    /// The current sort.
    pub fn sort(&self) -> SortSpec {
        self.shown.sort
    }

    /// Changes the sort. A loaded folder is re-sorted in the background and keeps its old order
    /// on screen until the new one lands; a folder still loading is loaded again. A folder cannot
    /// be sorted by `Relevance` or `Folder`; results can, and `Relevance` is always ascending.
    pub fn set_sort(&mut self, spec: SortSpec, cx: &mut Context<Self>) {
        if self.results.is_some() {
            let spec = if spec.column == SortColumn::Relevance {
                RELEVANCE
            } else {
                spec
            };
            if spec != self.shown.sort {
                self.sort_results(spec, cx);
            }
            return;
        }
        if spec == self.shown.sort || !folder_column(spec.column) {
            return;
        }
        self.shown.sort = spec;
        match self.shown.state {
            LoadState::Loading => {
                // The rows go; put the cursor back on the same name when it arrives again.
                if let Some(name) = self.cursor_name() {
                    self.shown.pending_focus = Some(name);
                }
                self.start_load(cx);
            }
            LoadState::Ready => self.spawn_folder_sort(cx),
            LoadState::Idle | LoadState::Failed(_) | LoadState::Unavailable(_) => cx.notify(),
        }
    }

    /// Header click: the same column flips direction, another column starts ascending.
    /// `Relevance` never flips.
    pub fn toggle_sort(&mut self, column: SortColumn, cx: &mut Context<Self>) {
        let spec = if self.shown.sort.column == column && column != SortColumn::Relevance {
            SortSpec {
                column,
                descending: !self.shown.sort.descending,
            }
        } else {
            SortSpec {
                column,
                descending: false,
            }
        };
        self.set_sort(spec, cx);
    }

    /// Number of selected rows.
    pub fn selected_count(&self) -> usize {
        self.shown.selection.count()
    }

    /// Display position of the cursor.
    pub fn cursor(&self) -> Option<usize> {
        self.shown.cursor_position
    }

    /// The selected entry when exactly one is selected. Feeds the details pane.
    pub fn primary(&self) -> Option<DirEntry<'_>> {
        self.shown.rows.get(self.shown.selection.single()?)
    }

    /// How the selected entry is reached on the filesystem, when exactly one is selected. A hit
    /// on a host folder carries what [`Reach::resolve`] needs; anything else is its own id.
    pub fn primary_reach(&self) -> Option<Reach> {
        let id = self.shown.selection.single()?;
        let entry = self.shown.rows.get(id)?;
        if self.host
            && let (Some(results), RowSource::Index(index)) = (&self.results, &self.shown.rows)
        {
            return Some(Reach::Hit {
                index: Arc::clone(index),
                entry: id,
                epoch: self.hit_epoch,
                known: results.resolved.get(id),
            });
        }
        Some(Reach::Node(entry.node))
    }

    /// Keeps the real id a [`Reach::resolve`] found for a hit, so the next ask costs nothing. Does
    /// not notify. Ignored when the results it belongs to are gone.
    pub fn remember_resolved(&mut self, reach: &Reach, node: NodeId) {
        let Reach::Hit { epoch, entry, .. } = reach else {
            return;
        };
        if *epoch != self.hit_epoch {
            return;
        }
        if let Some(results) = self.results.as_mut() {
            results.resolved.insert(*entry, node);
        }
    }

    /// A mouse press on the row at `position`.
    pub fn click(&mut self, position: usize, click: Click, cx: &mut Context<Self>) {
        let Some(&id) = self.shown.order.get(position) else {
            return;
        };
        match click {
            Click::Replace => self.shown.selection.replace(id),
            Click::Toggle => self.shown.selection.toggle(id),
            Click::Extend => self.shown.selection.extend_to(position, &self.shown.order),
        }
        self.shown.cursor_position = Some(position);
        self.user_moved(cx);
    }

    /// Moves the cursor; `extend` selects the range from the anchor instead of only the target.
    pub fn move_cursor(&mut self, motion: Motion, extend: bool, cx: &mut Context<Self>) {
        let Some(target) =
            motion_target(self.shown.cursor_position, self.shown.order.len(), motion)
        else {
            return;
        };
        if extend {
            self.shown.selection.extend_to(target, &self.shown.order);
        } else if let Some(&id) = self.shown.order.get(target) {
            self.shown.selection.replace(id);
        }
        self.shown.cursor_position = Some(target);
        self.user_moved(cx);
    }

    /// Selects every row.
    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        self.shown.selection.select_all(&self.shown.order);
        cx.notify();
    }

    /// Selects nothing; the cursor stays.
    pub fn clear_selection(&mut self, cx: &mut Context<Self>) {
        if self.shown.selection.count() > 0 {
            self.shown.selection.clear();
            cx.notify();
        }
    }

    /// Selects the row called exactly `name` and moves the cursor there. False when there is
    /// none (yet).
    pub fn select_name(&mut self, name: &str, cx: &mut Context<Self>) -> bool {
        let found = (0..self.len())
            .find(|&position| self.row(position).is_some_and(|row| row.entry.name == name));
        let Some(position) = found else {
            return false;
        };
        let Some(&id) = self.shown.order.get(position) else {
            return false;
        };
        self.shown.selection.replace(id);
        self.shown.cursor_position = Some(position);
        self.user_moved(cx);
        true
    }

    /// Type-to-select: selects the first row in display order whose name starts with `prefix`
    /// (case-insensitive) and moves the cursor there. False when nothing matches.
    pub fn select_prefix(&mut self, prefix: &str, cx: &mut Context<Self>) -> bool {
        let Some(position) = find_prefix(
            &self.shown.rows,
            &self.shown.order,
            self.shown.positions.as_ref(),
            self.shown.sort,
            prefix,
        ) else {
            return false;
        };
        let Some(&id) = self.shown.order.get(position) else {
            return false;
        };
        self.shown.selection.replace(id);
        self.shown.cursor_position = Some(position);
        self.user_moved(cx);
        true
    }

    /// Enter or double-click: a browsable entry is opened through `Navigation::open_child`;
    /// anything else does nothing in M1. In results, a folder hit is opened by its path and a
    /// file hit opens its folder with the cursor on it; either ends the search.
    pub fn activate(&mut self, position: usize, cx: &mut Context<Self>) {
        if self.results.is_some() {
            self.activate_result(position, cx);
            return;
        }
        let Some(row) = self.row(position) else {
            return;
        };
        // A package is one item: Enter and double-click leave it to Quick Look.
        if !row.entry.is_browsable() || packages_on(cx) && is_package_entry(&row.entry) {
            return;
        }
        let node = row.entry.node;
        let name = SharedString::from(row.entry.name.to_owned());
        self.navigation
            .update(cx, |navigation, cx| navigation.open_child(node, name, cx));
    }

    /// True when the row at `position` is a package shown as one item (the `packages_as_folders`
    /// setting is off).
    pub fn is_package_row(&self, position: usize, cx: &App) -> bool {
        packages_on(cx)
            && self
                .row(position)
                .is_some_and(|row| is_package_entry(&row.entry))
    }

    /// True when the row at `position` is a symbolic link or a classic alias.
    pub fn is_link_row(&self, position: usize) -> bool {
        self.row(position).is_some_and(|row| {
            row.entry.kind == disk_atlas_lib::fs::NodeKind::Symlink
                || (row.entry.kind == disk_atlas_lib::fs::NodeKind::File
                    && row.entry.flags.contains(disk_atlas_lib::fs::EntryFlags::ALIAS))
        })
    }

    /// "Show original": finds what the link or alias under the cursor points at, on a background
    /// thread, and goes to its folder with the cursor on it.
    pub fn show_original_cursor(&mut self, cx: &mut Context<Self>) {
        let (Some(here), Some(position)) = (self.location.clone(), self.shown.cursor_position)
        else {
            return;
        };
        if self.results.is_some() || !self.is_link_row(position) {
            return;
        }
        let Some(row) = self.row(position) else {
            return;
        };
        let (node, name) = (row.entry.node, row.entry.name.to_owned());
        let from: Vec<(NodeId, String)> = here
            .crumbs()
            .iter()
            .skip(1)
            .map(|crumb| (crumb.node, crumb.name.to_string()))
            .collect();
        let Some(root_name) = here.crumbs().first().map(|crumb| crumb.name.clone()) else {
            return;
        };
        let fs = Arc::clone(here.fs());
        let generation = self.generation;
        self.activate_task = Some(cx.spawn(async move |this, cx| {
            let resolved = cx
                .background_executor()
                .spawn(async move {
                    disk_atlas_lib::mac::original::resolve(fs.as_ref(), &from, &name, node)
                })
                .await;
            this.update(cx, |this, cx| {
                if generation != this.generation {
                    return;
                }
                match resolved {
                    Ok(disk_atlas_lib::mac::original::Resolved::Found(original)) => {
                        let focus = SharedString::from(original.name);
                        this.open_result_path(original.folder, &root_name, Some(focus), cx);
                    }
                    Ok(disk_atlas_lib::mac::original::Resolved::OtherVolume(volume)) => {
                        this.shown.note =
                            Some(format!("The original is on a disk called {volume}.").into());
                        cx.notify();
                    }
                    Err(error) => {
                        this.shown.note = Some(failure_text(&error));
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
    }

    /// "Show package contents": goes inside the package under the cursor.
    pub fn open_package_cursor(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.shown.cursor_position.and_then(|p| self.row(p)) else {
            return;
        };
        if !row.entry.is_browsable() || self.results.is_some() {
            return;
        }
        let node = row.entry.node;
        let name = SharedString::from(row.entry.name.to_owned());
        self.navigation
            .update(cx, |navigation, cx| navigation.open_child(node, name, cx));
    }

    /// [`Self::activate`] on the cursor row.
    pub fn activate_cursor(&mut self, cx: &mut Context<Self>) {
        if let Some(position) = self.shown.cursor_position {
            self.activate(position, cx);
        }
    }

    /// Shows search results in place of the folder. The first call puts the folder view aside
    /// (an unfinished load is abandoned); later calls replace the results. The selection and the
    /// cursor start empty.
    ///
    /// Only the sorted head (at most [`HEAD`] rows) is put in order here, on the UI thread; the
    /// whole order and its inverse are made by the results sort on the background executor and
    /// replace the head when they land (`performance.md`: nothing proportional to the result
    /// count on the UI thread).
    pub fn show_results(
        &mut self,
        index: Arc<Index>,
        hits: Arc<Hits>,
        terms: Arc<[String]>,
        cx: &mut Context<Self>,
    ) {
        // Drops in-flight load steps, re-sorts and `sort_all`s.
        self.generation += 1;
        self.sort_task = None;
        self.sort_all_task = None;
        let head = hits.sorted_len().min(hits.len()).min(HEAD);
        let order: Arc<[u32]> = (0..head).filter_map(|p| hits.entry(p)).collect();
        let complete = order.len() == hits.len();
        if let Some(results) = self.results.as_mut() {
            results.cancel.store(true, atomic::Ordering::Relaxed);
            let same_index =
                matches!(&self.shown.rows, RowSource::Index(old) if Arc::ptr_eq(old, &index));
            if !same_index {
                results.folders.clear();
                results.resolved = ResolvedIds::default();
                self.hit_epoch += 1;
            }
            results.hits = hits;
            results.terms = terms;
            results.sorting_all = None;
        } else {
            let stashed = FolderView::take(&mut self.shown);
            self.load_task = None;
            self.loading_timer = None;
            self.loading_visible = false;
            self.shown.sort = RELEVANCE;
            self.results = Some(Results {
                stashed,
                hits,
                terms,
                folders: FolderCache::default(),
                cancel: Arc::default(),
                sorting_all: None,
                resolved: ResolvedIds::default(),
            });
            self.hit_epoch += 1;
        }
        self.shown.rows = RowSource::Index(index);
        self.shown.order = order;
        self.shown.positions = None;
        self.shown.selection.reset();
        self.shown.cursor_position = None;
        self.shown.state = LoadState::Ready;
        self.shown.note = None;
        self.shown.follow_cursor = false;
        self.shown.pending_focus = None;
        self.shown.resorting = false;
        if self.shown.sort.column != SortColumn::Relevance || !complete {
            // The relevance head shows at once; the whole order (in the column's sort, or by
            // relevance) replaces it when it lands.
            self.spawn_results_sort(cx);
        }
        cx.notify();
    }

    /// Goes back to the folder exactly as it was before the search. A no-op outside results.
    pub fn leave_results(&mut self, cx: &mut Context<Self>) {
        let Some(restored) = self.exit_results() else {
            return;
        };
        match restored {
            Restored::Shown if self.shown.rule.visibility != Visibility::current(cx) => {
                // A switch changed while the results were shown.
                self.visibility = Visibility::current(cx);
                self.rebuild_order(cx);
            }
            Restored::Shown => {
                if self.shown.cursor_position.is_some() {
                    cx.emit(ListingEvent::CursorMoved);
                }
                cx.notify();
            }
            Restored::Resort => self.spawn_folder_sort(cx),
            // `exit_results` left the cursor name in `pending_focus`.
            Restored::Reload => self.start_load(cx),
        }
    }

    /// True while search results are shown.
    pub fn in_results(&self) -> bool {
        self.results.is_some()
    }

    /// The search terms of the results shown, for match highlighting; empty outside results.
    pub fn terms(&self) -> &[String] {
        self.results.as_ref().map_or(&[], |results| &results.terms)
    }

    /// The hits shown (their count and timing for the search field).
    pub fn hits(&self) -> Option<&Arc<Hits>> {
        self.results.as_ref().map(|results| &results.hits)
    }

    /// The index the results come from.
    #[expect(dead_code, reason = "read by the explorer and Search (M4 task 10, 11)")]
    pub fn index(&self) -> Option<&Arc<Index>> {
        match (&self.results, &self.shown.rows) {
            (Some(_), RowSource::Index(index)) => Some(index),
            _ => None,
        }
    }

    /// The path of the folder holding the result at `position`, for the Folder column: root
    /// first, `/` between names, empty for the root. Cached by parent folder. `None` outside
    /// results or past the end.
    pub fn folder_of(&self, position: usize) -> Option<SharedString> {
        let results = self.results.as_ref()?;
        let RowSource::Index(index) = &self.shown.rows else {
            return None;
        };
        let entry = *self.shown.order.get(position)?;
        Some(results.folders.folder_of(index, entry))
    }

    /// Called by the view with its last visible position: once it nears the end of the sorted
    /// head of relevance-ordered results, sorts the rest on the background executor.
    pub fn ensure_sorted_through(&mut self, last_visible: usize, cx: &mut Context<Self>) {
        let generation = self.generation;
        let Some(results) = self.results.as_mut() else {
            return;
        };
        // While only the head is shown, the whole relevance order is still on its way; it would
        // land after a `sort_all` and put the unsorted tail back. The view asks again on its next
        // frame.
        if self.shown.sort.column != SortColumn::Relevance
            || self.shown.order.len() < results.hits.len()
            || results.sorting_all == Some(generation)
            || !needs_sort_all(results.hits.sorted_len(), results.hits.len(), last_visible)
        {
            return;
        }
        results.sorting_all = Some(generation);
        let hits = Arc::clone(&results.hits);
        self.sort_all_task = Some(cx.spawn(async move |this, cx| {
            // The copy, the sort, the order and its inverse are all made off the UI thread.
            let (hits, order, positions) = cx
                .background_executor()
                .spawn(async move {
                    let mut sorted = (*hits).clone();
                    sorted.sort_all();
                    let order = sorted.entries_in_order();
                    let positions = Positions::new(Arc::clone(&order));
                    (Arc::new(sorted), order, positions)
                })
                .await;
            this.update(cx, |this, cx| {
                this.apply_sort_all(generation, hits, order, positions, cx)
            })
            .ok();
        }));
    }

    /// Lists the folder `Navigation` shows now. `focus` is the row name to put the cursor on.
    /// Results shown are dropped: the search is over.
    fn load(&mut self, focus: Option<SharedString>, cx: &mut Context<Self>) {
        if let (Some(here), Some(results)) = (&self.location, &self.results)
            && let Some(name) = results.stashed.focus_name()
        {
            let key = (here.source(), here.node());
            self.remember(key, name);
        }
        if self.exit_results().is_some() {
            cx.emit(ListingEvent::ResultsDropped);
        }
        if let (Some(here), Some(name)) = (&self.location, self.cursor_name()) {
            let key = (here.source(), here.node());
            self.remember(key, name);
        }
        self.location = self.navigation.read(cx).current().cloned();
        let here = self
            .location
            .as_ref()
            .map(|location| (location.source(), location.node()));
        if self.revealed != here {
            self.revealed = None;
        }
        self.activate_task = None;
        self.host = self.location.as_ref().is_some_and(|location| {
            self.sources
                .read(cx)
                .source(location.source())
                .is_some_and(|source| matches!(source.kind, SourceKind::Host))
        });
        // `info` answers from memory, so reading it once per navigation costs nothing visible; the
        // render path reads the cached values.
        let info = self.location.as_ref().map(|location| location.fs().info());
        self.resource_forks = info.as_ref().is_some_and(|info| info.resource_forks);
        self.local_times = info.as_ref().is_some_and(|info| info.local_times);
        self.journal = info.and_then(|info| info.journal);
        // A sort by a column this volume does not have would be invisible: go back to the default.
        if !self.resource_forks && self.shown.sort.column == SortColumn::ResourceFork {
            self.shown.sort = SortSpec::default();
        }
        self.shown.pending_focus = focus.or_else(|| {
            let here = self.location.as_ref()?;
            let key = (here.source(), here.node());
            self.remembered
                .iter()
                .rev()
                .find(|(k, _)| *k == key)
                .map(|(_, name)| name.clone())
        });
        self.start_load(cx);
    }

    /// Reads the folder again (F5), keeping the cursor on its row by name. Search results are not
    /// a folder: nothing happens while they are shown.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.results.is_none() && self.location.is_some() {
            self.load(None, cx);
        }
    }

    /// The path on this computer of the file under the cursor, when the folder shown is on the
    /// host and the cursor is on a file (not in search results, whose rows are not children of
    /// the folder). Only reads memory.
    pub fn host_path_of_cursor(&self, cx: &App) -> Option<std::path::PathBuf> {
        if !self.host || self.results.is_some() {
            return None;
        }
        let location = self.location.as_ref()?;
        let row = self.row(self.shown.cursor_position?)?;
        if row.entry.is_browsable() {
            return None;
        }
        let mut path = self
            .sources
            .read(cx)
            .host_root(location.source())?
            .to_path_buf();
        for crumb in location.crumbs().iter().skip(1) {
            path.push(crumb.name.as_ref());
        }
        path.push(row.entry.name);
        Some(path)
    }

    /// The path on this computer of the folder shown, when it is on the host. Only reads memory.
    pub fn host_folder_path(&self, cx: &App) -> Option<std::path::PathBuf> {
        if !self.host {
            return None;
        }
        let location = self.location.as_ref()?;
        let mut path = self
            .sources
            .read(cx)
            .host_root(location.source())?
            .to_path_buf();
        for crumb in location.crumbs().iter().skip(1) {
            path.push(crumb.name.as_ref());
        }
        Some(path)
    }

    /// The path on this computer of the file or folder at `position`, when the folder shown is on
    /// the host. Not in search results, whose rows are not children of the folder. Only reads
    /// memory.
    pub fn host_path_of(&self, position: usize, cx: &App) -> Option<std::path::PathBuf> {
        if self.results.is_some() {
            return None;
        }
        let row = self.row(position)?;
        let mut path = self.host_folder_path(cx)?;
        path.push(row.entry.name);
        Some(path)
    }

    /// The files and devices Disk Atlas has open as disks or images: host operations must not
    /// touch them.
    pub fn open_disk_paths(&self, cx: &App) -> Vec<std::path::PathBuf> {
        self.sources
            .read(cx)
            .disks()
            .iter()
            .map(|disk| disk.path.clone())
            .collect()
    }

    /// The display positions of the selected rows, in order, at most `limit` of them.
    pub fn selected_positions(&self, limit: usize) -> Vec<usize> {
        let mut out = Vec::new();
        if self.shown.selection.count() == 0 {
            return out;
        }
        for (position, &arrival) in self.shown.order.iter().enumerate() {
            if self.shown.selection.is_selected(arrival) {
                out.push(position);
                if out.len() >= limit {
                    break;
                }
            }
        }
        out
    }

    /// The node ids of the selected rows, in display order, at most `limit` of them. In search
    /// results these are the index's ids (see [`Reach`] for a hit's real one).
    pub fn selected_nodes(&self, limit: usize) -> Vec<NodeId> {
        self.selected_positions(limit)
            .into_iter()
            .filter_map(|position| self.row(position))
            .map(|row| row.entry.node)
            .collect()
    }

    /// What a context menu needs to know of the source shown. Only reads memory.
    pub fn source_facts(&self, cx: &App) -> Option<SourceFacts> {
        let id = self.location.as_ref()?.source();
        let sources = self.sources.read(cx);
        let source = sources.source(id)?;
        Some(SourceFacts {
            host: matches!(source.kind, SourceKind::Host),
            hfs_family: source
                .format
                .as_ref()
                .is_some_and(|format| format.starts_with("HFS")),
        })
    }

    /// Remembers `name` as the cursor row of the folder `key`.
    fn remember(&mut self, key: (SourceId, NodeId), name: SharedString) {
        self.remembered.retain(|(k, _)| *k != key);
        if self.remembered.len() >= REMEMBERED_FOLDERS {
            self.remembered.pop_front();
        }
        self.remembered.push_back((key, name));
    }

    /// Clears the rows and starts reading `self.location` with the current sort.
    fn start_load(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.load_epoch += 1;
        self.resolve_task = None;
        self.live_task = None;
        self.live_resolving = false;
        self.live_pending = false;
        self.live_was_running = false;
        self.merge_task = None;
        self.merge_generation += 1;
        self.refresh_after_load = false;
        self.visible = 0..0;
        self.visible_task = None;
        self.resort_wanted = false;
        self.resort_timer = None;
        self.shown.rows = RowSource::default();
        self.shown.order = Arc::from([]);
        self.shown.positions = None;
        self.shown.selection.reset();
        self.shown.cursor_position = None;
        self.shown.note = None;
        self.shown.follow_cursor = false;
        self.shown.resorting = false;
        self.loading_visible = false;
        self.first_page = None;
        self.started = Instant::now();
        self.sort_task = None;

        let Some(location) = &self.location else {
            self.shown.state = LoadState::Idle;
            self.load_task = None;
            self.loading_timer = None;
            self.shown.pending_focus = None;
            cx.notify();
            return;
        };
        // A source that cannot be browsed has nothing to load. The generation was bumped above,
        // so a step still in flight for the previous folder is dropped on arrival.
        if let Some(state) = self
            .sources
            .read(cx)
            .source(location.source())
            .and_then(|source| unavailable_state(&source.availability))
        {
            self.shown.state = state;
            self.load_task = None;
            self.loading_timer = None;
            self.shown.pending_focus = None;
            cx.notify();
            return;
        }
        self.shown.state = LoadState::Loading;
        let generation = self.generation;
        let fs = Arc::clone(location.fs());
        let dir = location.node();
        let sort = self.shown.sort;
        let table = self.current_table();
        let rule = self.rule(cx);
        self.shown.rule = rule;
        self.shown.hidden = 0;
        self.visibility = rule.visibility;
        self.loaded_with_table = table.is_some();
        if let (Some(table), RowSource::Folder(rows)) = (&table, &mut self.shown.rows) {
            rows.set_lower_bound(table.is_lower_bound());
        }

        // Replacing the task drops the old one, and its stream with it.
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let mut loader = Loader::new(fs, dir, sort).with_sizes(table).with_rule(rule);
            loop {
                let (back, step) = cx
                    .background_executor()
                    .spawn(async move {
                        let step = loader.step();
                        (loader, step)
                    })
                    .await;
                loader = back;
                let more = matches!(step.outcome, Outcome::More);
                if this
                    .update(cx, |this, cx| this.apply(generation, step, cx))
                    .is_err()
                    || !more
                {
                    break;
                }
            }
        }));

        self.loading_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LOADING_DELAY).await;
            this.update(cx, |this, cx| {
                if this.generation == generation
                    && this.shown.state == LoadState::Loading
                    && this.shown.rows.is_empty()
                {
                    this.loading_visible = true;
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    /// Applies one loader step: push the page, swap the order, notify once.
    fn apply(&mut self, generation: u64, step: Step, cx: &mut Context<Self>) {
        if generation != self.generation {
            return;
        }
        // Results bump the generation, so a step always finds folder rows here.
        let RowSource::Folder(rows) = &mut self.shown.rows else {
            return;
        };
        let mut reveal = false;
        if let Some(page) = step.page {
            let start = rows.len();
            rows.push(Arc::clone(&page));
            rows.push_totals(step.totals);
            if self.first_page.is_none() {
                self.first_page = Some(self.started.elapsed());
            }
            if let Some(name) = &self.shown.pending_focus
                && let Some(index) = page.iter().position(|entry| entry.name == name.as_ref())
                && let Ok(arrival) = u32::try_from(start + index)
            {
                self.shown.selection.replace(arrival);
                self.shown.pending_focus = None;
                self.shown.follow_cursor = true;
                reveal = true;
            }
        }
        self.shown.order = step.order;
        self.shown.positions = None;
        self.shown.hidden = step.hidden;
        let before = self.shown.cursor_position;
        self.shown.cursor_position = self.find_cursor();
        if self.shown.follow_cursor && self.shown.cursor_position != before {
            reveal = true;
        }

        match step.outcome {
            Outcome::More => {}
            Outcome::Done => {
                self.shown.state = LoadState::Ready;
                self.finish_load();
            }
            Outcome::Failed(error) => {
                if self.shown.rows.is_empty() {
                    self.shown.state = LoadState::Failed(failure_text(&error));
                } else {
                    self.shown.state = LoadState::Ready;
                    self.shown.note = Some(partial_text(&error));
                }
                tracing::debug!(%error, "folder listing stopped");
                self.finish_load();
            }
        }
        // The table arrived while the rows were coming in: the loader had none.
        if self.shown.state == LoadState::Ready
            && !self.loaded_with_table
            && self.current_table().is_some()
        {
            self.spawn_resolve(cx);
        }
        if self.shown.state == LoadState::Ready {
            // The host's folder rows get their handles into the live tree.
            if self.host {
                self.spawn_resolve_live(cx);
            }
            // Something changed while the folder was being read: read it again.
            if std::mem::take(&mut self.refresh_after_load) {
                self.reload_in_place(cx);
            }
        }
        if reveal {
            cx.emit(ListingEvent::CursorMoved);
        }
        cx.notify();
    }

    fn finish_load(&mut self) {
        self.shown.pending_focus = None;
        self.shown.follow_cursor = false;
        self.loading_timer = None;
        let millis = |d: Duration| d.as_secs_f64() * 1e3;
        tracing::debug!(
            entries = self.shown.rows.len(),
            first_page_ms = self.first_page.map(millis),
            done_ms = millis(self.started.elapsed()),
            "listed folder"
        );
    }

    /// Re-sorts the loaded folder rows by the current sort on the background executor.
    fn spawn_folder_sort(&mut self, cx: &mut Context<Self>) {
        let Some(rows) = self.shown.rows.folder().cloned() else {
            return;
        };
        self.generation += 1;
        let generation = self.generation;
        let rule = self.rule(cx);
        self.visibility = rule.visibility;
        let spec = self.shown.sort;
        self.shown.resorting = true;
        self.sort_task = Some(cx.spawn(async move |this, cx| {
            let (order, positions, hidden) = cx
                .background_executor()
                .spawn(async move {
                    let (order, hidden) = sort_rows_visible(&rows, spec, &rule);
                    let positions = Positions::new(Arc::clone(&order));
                    (order, positions, hidden)
                })
                .await;
            this.update(cx, |this, cx| {
                this.apply_sort(generation, order, positions, Some((hidden, rule)), cx)
            })
            .ok();
        }));
        cx.notify();
    }

    /// Results: changes the sort to `spec` (not the current one). The order on screen stays until
    /// the new one lands.
    fn sort_results(&mut self, spec: SortSpec, cx: &mut Context<Self>) {
        self.shown.sort = spec;
        self.spawn_results_sort(cx);
        cx.notify();
    }

    /// Results: orders the hits by the current sort on the background executor under a fresh
    /// generation, with the inverse of that order. A column sort goes through the engine's
    /// `order_hits`; the relevance order is the hits' own (`Hits::entries_in_order`). The order on
    /// screen stays until the new one lands.
    fn spawn_results_sort(&mut self, cx: &mut Context<Self>) {
        let spec = self.shown.sort;
        let by = hit_order(spec.column);
        let (Some(results), RowSource::Index(index)) = (self.results.as_mut(), &self.shown.rows)
        else {
            return;
        };
        results.cancel.store(true, atomic::Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        results.cancel = Arc::clone(&cancel);
        let index = Arc::clone(index);
        let hits = Arc::clone(&results.hits);
        self.generation += 1;
        let generation = self.generation;
        self.sort_task = Some(cx.spawn(async move |this, cx| {
            let sorted = cx
                .background_executor()
                .spawn(async move { results_order(&index, &hits, by, spec.descending, &cancel) })
                .await;
            if let Some((order, positions)) = sorted {
                this.update(cx, |this, cx| {
                    this.apply_sort(generation, order, positions, None, cx)
                })
                .ok();
            }
        }));
    }

    /// Swaps in the order (and its inverse) from a background sort.
    fn apply_sort(
        &mut self,
        generation: u64,
        order: Arc<[u32]>,
        positions: Option<Positions>,
        folder: Option<(usize, Rule)>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.generation {
            return;
        }
        self.shown.order = order;
        if let Some((hidden, rule)) = folder {
            self.shown.hidden = hidden;
            self.shown.rule = rule;
        }
        self.shown.positions = positions;
        self.shown.resorting = false;
        self.shown.cursor_position = self.find_cursor();
        if self.shown.cursor_position.is_some() {
            cx.emit(ListingEvent::CursorMoved);
        }
        cx.notify();
    }

    /// Swaps in fully sorted hits and their order. Positions in the old sorted head do not move,
    /// so a cursor there keeps its position without a scan.
    fn apply_sort_all(
        &mut self,
        generation: u64,
        hits: Arc<Hits>,
        order: Arc<[u32]>,
        positions: Option<Positions>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.generation {
            return;
        }
        let Some(results) = self.results.as_mut() else {
            return;
        };
        let head = results.hits.sorted_len();
        results.hits = hits;
        self.shown.order = order;
        self.shown.positions = positions;
        if self
            .shown
            .cursor_position
            .is_none_or(|position| position >= head)
        {
            self.shown.cursor_position = self.find_cursor();
        }
        cx.notify();
    }

    /// Leaves results mode: stops the result sorts, drops the hits and puts the folder view back.
    /// `None` when no results were shown.
    fn exit_results(&mut self) -> Option<Restored> {
        let results = self.results.take()?;
        results.cancel.store(true, atomic::Ordering::Relaxed);
        self.generation += 1;
        self.sort_task = None;
        self.sort_all_task = None;
        Some(results.stashed.restore(&mut self.shown))
    }

    /// Results: opens the hit at `position` (see [`Self::activate`]). On a host folder the path
    /// of the hit is walked with `lookup` on the background executor first (the index's ids are
    /// not the filesystem's), under the generation of the results: a walk for results the user
    /// has left or re-sorted is dropped.
    fn activate_result(&mut self, position: usize, cx: &mut Context<Self>) {
        let (Some(here), RowSource::Index(index)) = (&self.location, &self.shown.rows) else {
            return;
        };
        let Some(&entry) = self.shown.order.get(position) else {
            return;
        };
        let Some(row) = index.entry(entry) else {
            return;
        };
        let Some(root_name) = here.crumbs().first().map(|crumb| crumb.name.clone()) else {
            return;
        };
        let (path, focus) = if row.is_browsable() {
            (index.crumbs(entry), None)
        } else {
            let Some(parent) = index.parent(entry) else {
                return;
            };
            (
                index.crumbs(parent),
                Some(SharedString::from(row.name.to_owned())),
            )
        };
        if !self.host {
            self.open_result_path(path, &root_name, focus, cx);
            return;
        }
        let fs = Arc::clone(here.fs());
        let generation = self.generation;
        self.activate_task = Some(cx.spawn(async move |this, cx| {
            let walked = cx
                .background_executor()
                .spawn(async move { reach_path(fs.as_ref(), path) })
                .await;
            this.update(cx, |this, cx| {
                if generation != this.generation || this.results.is_none() {
                    return;
                }
                match walked {
                    Ok(Some(path)) => this.open_result_path(path, &root_name, focus, cx),
                    Ok(None) => {
                        this.shown.note = Some("This folder no longer exists.".into());
                        cx.notify();
                    }
                    Err(error) => {
                        this.shown.note = Some(failure_text(&error));
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
    }

    /// Navigates to the folder at the end of `path` (ids the filesystem knows), with the cursor
    /// on `focus`.
    fn open_result_path(
        &mut self,
        path: Vec<(NodeId, String)>,
        root_name: &SharedString,
        focus: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let Some(here) = &self.location else {
            return;
        };
        let Some(to) = result_crumbs(path, root_name).and_then(|crumbs| here.with_crumbs(crumbs))
        else {
            return;
        };
        if to.same_folder(here) {
            // Navigation ignores a move to the folder already shown: list it again here.
            self.load(focus, cx);
            return;
        }
        self.navigation.update(cx, |navigation, cx| {
            if focus.is_some() {
                navigation.navigate_focus(to, focus, cx);
            } else {
                navigation.navigate(to, cx);
            }
        });
    }

    /// Bookkeeping after the user moved the cursor or the selection.
    fn user_moved(&mut self, cx: &mut Context<Self>) {
        self.resort.input(Instant::now());
        self.shown.follow_cursor = false;
        self.shown.pending_focus = None;
        cx.emit(ListingEvent::CursorMoved);
        cx.notify();
    }

    /// The display position of the cursor row: O(1) through the inverse built with the order by
    /// a background sort, a scan of the `u32` order otherwise (a folder still loading, the head of
    /// fresh results).
    fn find_cursor(&self) -> Option<usize> {
        self.shown.find_cursor()
    }

    fn cursor_name(&self) -> Option<SharedString> {
        let name = self.shown.rows.name(self.shown.selection.cursor()?)?;
        Some(SharedString::from(name.to_owned()))
    }
}

/// A folder read again by [`Listing::reload_in_place`], ready to swap in.
struct Merged {
    rows: Rows,
    order: Arc<[u32]>,
    positions: Option<Positions>,
    hidden: usize,
    /// "Only part of this folder could be listed", when the read stopped early.
    note: Option<SharedString>,
    /// Some folder rows were not in the host tree yet.
    pending: bool,
}

/// How many rows each side of a vanished cursor row are tried as its neighbour.
const NEIGHBOURS: usize = 32;

/// The selection a folder read again gives: the rows of `new` that have the names of the selected
/// rows of `old` are selected, and the cursor goes to the row with the cursor's name. When that
/// row is gone the cursor goes to the nearest row, in the old display order, that is still there
/// (the one after it first), and failing that to the same position in the new order. A selected
/// name that is gone is dropped from the selection. The anchor of a range selection is not kept:
/// it becomes the cursor.
///
/// O(rows) with one hash set of the wanted names; it reads each page once, and borrows names, so
/// it allocates nothing per row.
fn merge_by_name(
    old: &Rows,
    old_order: &[u32],
    selection: &Selection,
    new: &Rows,
    new_order: &[u32],
) -> Selection {
    let mut merged = Selection::new();
    let cursor = selection.cursor().filter(|&c| old.get(c).is_some());
    let mut selected: Vec<&str> = Vec::new();
    if selection.count() > 0 {
        selected.extend(
            old.iter()
                .filter(|(arrival, _)| selection.is_selected(*arrival))
                .map(|(_, entry)| entry.name),
        );
    }
    let cursor_name = cursor.and_then(|c| old.get(c)).map(|entry| entry.name);
    if selected.is_empty() && cursor_name.is_none() {
        return merged;
    }

    let mut wanted: HashSet<&str> = selected.iter().copied().collect();
    wanted.extend(cursor_name);
    let mut found: HashMap<&str, u32> = HashMap::with_capacity(wanted.len());
    for (arrival, entry) in new.iter() {
        if wanted.contains(entry.name) {
            found.insert(entry.name, arrival);
        }
    }
    for name in &selected {
        if let Some(&arrival) = found.get(name) {
            merged.toggle(arrival);
        }
    }

    let Some(cursor_name) = cursor_name else {
        return merged;
    };
    let mut target = found.get(cursor_name).copied();
    if target.is_none() {
        let at = old_order.iter().position(|&a| Some(a) == cursor);
        if let Some(at) = at {
            // The rows around the vanished one, nearest first, the one after it before the one
            // before it.
            let mut near: Vec<&str> = Vec::new();
            for step in 1..=NEIGHBOURS {
                let after = old_order.get(at.saturating_add(step));
                let before = at.checked_sub(step).and_then(|i| old_order.get(i));
                for &row in after.into_iter().chain(before) {
                    near.extend(old.get(row).map(|entry| entry.name));
                }
            }
            let candidates: HashSet<&str> = near.iter().copied().collect();
            let mut survivors: HashMap<&str, u32> = HashMap::new();
            for (arrival, entry) in new.iter() {
                if candidates.contains(entry.name) {
                    survivors.insert(entry.name, arrival);
                }
            }
            target = near.iter().find_map(|name| survivors.get(name).copied());
            if target.is_none() {
                let last = new_order.len().checked_sub(1);
                target = last.and_then(|last| new_order.get(at.min(last)).copied());
            }
        }
    }
    if let Some(row) = target {
        // Twice, so the row's own selected state is left as it is: the cursor goes there.
        merged.toggle(row);
        merged.toggle(row);
    }
    merged
}

/// Where `motion` takes a cursor at `current` in a list of `len` rows. `None` when the list is
/// empty. With no cursor, downward motions start at the first row and upward ones at the last.
fn motion_target(current: Option<usize>, len: usize, motion: Motion) -> Option<usize> {
    let last = len.checked_sub(1)?;
    let target = match (current, motion) {
        (None, Motion::Down | Motion::PageDown(_) | Motion::First) => 0,
        (None, Motion::Up | Motion::PageUp(_) | Motion::Last) => last,
        (Some(at), Motion::Up) => at.saturating_sub(1),
        (Some(at), Motion::Down) => at.saturating_add(1),
        (Some(at), Motion::PageUp(rows)) => at.saturating_sub(rows.max(1)),
        (Some(at), Motion::PageDown(rows)) => at.saturating_add(rows.max(1)),
        (Some(_), Motion::First) => 0,
        (Some(_), Motion::Last) => last,
    };
    Some(target.min(last))
}

/// The first display position whose name starts with `prefix`, ignoring case.
///
/// Sorted by name ascending, the folders and then the files are each in name order, and the
/// names sharing a prefix without digits sit together; a binary search per group finds the first.
/// Any other order, or a prefix with digits (digit runs compare as numbers, so "a10" sorts after
/// "a2"), is a linear scan. Search results sorted by name are in the engine's name order, which
/// matches the app's for ASCII; a miss of the binary search there falls back to the scan.
///
/// `positions`, when they are the inverse of `order`, turn the folder scan's last step into a
/// lookup per matching row instead of a pass over the order.
fn find_prefix(
    rows: &RowSource,
    order: &Arc<[u32]>,
    positions: Option<&Positions>,
    sort: SortSpec,
    prefix: &str,
) -> Option<usize> {
    if prefix.is_empty() {
        return None;
    }
    let by_name_ascending = sort.column == SortColumn::Name && !sort.descending;
    if by_name_ascending && !prefix.bytes().any(|b| b.is_ascii_digit()) {
        let found = find_prefix_by_name(rows, order, prefix);
        if found.is_some() || rows.folder().is_some() {
            return found;
        }
    }
    let positions = positions.filter(|positions| Arc::ptr_eq(positions.order(), order));
    match rows {
        RowSource::Folder(folder) => match positions {
            Some(positions) => find_prefix_by_positions(folder, positions, prefix),
            None => find_prefix_by_arrival(folder, order, prefix),
        },
        RowSource::Index(_) => order.iter().position(|&id| {
            rows.name(id)
                .is_some_and(|name| starts_with_folded(name, prefix))
        }),
    }
}

/// The binary search of [`find_prefix`] over rows sorted by name ascending, folders first.
fn find_prefix_by_name(rows: &RowSource, order: &[u32], prefix: &str) -> Option<usize> {
    let folders_end =
        order.partition_point(|&id| rows.get(id).is_some_and(|entry| entry.is_browsable()));
    let groups = [(0, folders_end), (folders_end, order.len())];
    for (start, end) in groups {
        let group = order.get(start..end)?;
        let offset = group.partition_point(|&id| {
            rows.name(id)
                .is_some_and(|name| compare_tokens(name, prefix).is_lt())
        });
        let position = start + offset;
        if position < end
            && order
                .get(position)
                .and_then(|&id| rows.name(id))
                .is_some_and(|name| starts_with_folded(name, prefix))
        {
            return Some(position);
        }
    }
    None
}

/// The linear scan of [`find_prefix`] over folder rows: read the names in arrival order
/// (sequential, no page lookups), mark the matches, then find the first marked row in display
/// order with a scan of the `u32` order.
fn find_prefix_by_arrival(rows: &Rows, order: &[u32], prefix: &str) -> Option<usize> {
    let mut matches = vec![0u64; rows.len().div_ceil(64)];
    let mut any = false;
    for (arrival, entry) in rows.iter() {
        if starts_with_folded(entry.name, prefix)
            && let Some(word) = matches.get_mut(arrival as usize / 64)
        {
            *word |= 1 << (arrival % 64);
            any = true;
        }
    }
    if !any {
        return None;
    }
    order.iter().position(|&arrival| {
        matches
            .get(arrival as usize / 64)
            .is_some_and(|word| word & (1 << (arrival % 64)) != 0)
    })
}

/// The scan of [`find_prefix`] over folder rows when the inverse of the order is known: read the
/// names in arrival order and keep the smallest display position of a match, one O(1) lookup per
/// match. No bitmap, no pass over the order.
fn find_prefix_by_positions(rows: &Rows, positions: &Positions, prefix: &str) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (arrival, entry) in rows.iter() {
        if starts_with_folded(entry.name, prefix)
            && let Some(position) = positions.get(arrival)
            && best.is_none_or(|b| position < b)
        {
            best = Some(position);
        }
    }
    best
}

/// Why a folder could not be opened, for the user.
fn failure_text(error: &Error) -> SharedString {
    match error {
        Error::AccessDenied { .. } => "You do not have permission to open this folder.".into(),
        Error::NotFound { .. } => "This folder no longer exists.".into(),
        Error::Io { source, .. } if source.kind() == io::ErrorKind::NotFound => {
            "This folder no longer exists.".into()
        }
        Error::NotADirectory { .. } => "This item is not a folder.".into(),
        other => format!("This folder could not be read ({other}).").into(),
    }
}

/// The note shown with the rows of a folder that could be read only in part.
fn partial_text(error: &Error) -> SharedString {
    format!("Part of this folder could not be read ({error}).").into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_atlas_lib::error::{IoContext, Result};
    use disk_atlas_lib::fs::native::NativeFs;
    use disk_atlas_lib::fs::{
        DirBatch, DirStream, EntryFlags, FileSystem, Metadata, NodeKind, ScanEntry, ScanSummary,
        VolumeInfo,
    };
    use disk_atlas_lib::search::{Query, build};
    use std::cell::Cell;
    use std::ops::ControlFlow;

    use super::super::loader::sort_rows;
    use super::super::order::compare_entries;
    use super::super::sizes::{RowSize, resolve_page};

    #[test]
    fn motion_without_a_cursor() {
        assert_eq!(motion_target(None, 0, Motion::Down), None);
        assert_eq!(motion_target(Some(3), 0, Motion::Up), None);
        for motion in [Motion::Down, Motion::PageDown(10), Motion::First] {
            assert_eq!(motion_target(None, 5, motion), Some(0));
        }
        for motion in [Motion::Up, Motion::PageUp(10), Motion::Last] {
            assert_eq!(motion_target(None, 5, motion), Some(4));
        }
    }

    #[test]
    fn motion_clamps() {
        assert_eq!(motion_target(Some(0), 5, Motion::Up), Some(0));
        assert_eq!(motion_target(Some(4), 5, Motion::Down), Some(4));
        assert_eq!(motion_target(Some(2), 5, Motion::Down), Some(3));
        assert_eq!(motion_target(Some(2), 5, Motion::PageDown(10)), Some(4));
        assert_eq!(motion_target(Some(2), 5, Motion::PageUp(10)), Some(0));
        assert_eq!(motion_target(Some(3), 50, Motion::PageUp(2)), Some(1));
        // A page of zero rows still moves one.
        assert_eq!(motion_target(Some(3), 50, Motion::PageDown(0)), Some(4));
        assert_eq!(motion_target(Some(3), 50, Motion::First), Some(0));
        assert_eq!(motion_target(Some(3), 50, Motion::Last), Some(49));
        // A stale cursor past the end lands on the last row.
        assert_eq!(motion_target(Some(80), 50, Motion::Up), Some(49));
        assert_eq!(motion_target(Some(usize::MAX), 50, Motion::Down), Some(49));
    }

    fn rows_of(entries: &[(&str, NodeKind)]) -> Rows {
        let mut batch = DirBatch::new();
        for (i, &(name, kind)) in entries.iter().enumerate() {
            batch.push(DirEntry {
                node: NodeId(i as u64),
                name,
                kind,
                size: None,
                resource_size: None,
                modified: None,
                flags: EntryFlags::EMPTY,
            });
        }
        let mut rows = Rows::default();
        rows.push(Arc::new(batch));
        rows
    }

    #[test]
    fn prefix_search_by_name_and_otherwise() {
        let dir = NodeKind::Directory;
        let file = NodeKind::File;
        let rows = rows_of(&[
            ("beta", file),
            ("Alpha", file),
            ("alps", dir),
            ("a2", file),
            ("a10", file),
            ("Read me", file),
            ("README", file),
            ("zeta", dir),
        ]);
        let source = RowSource::Folder(rows.clone());
        let by_name = SortSpec::default();
        let order = sort_rows(&rows, by_name);
        let name =
            |position: Option<usize>| position.map(|p| rows.get(order[p]).unwrap().name.to_owned());
        // Folders first: "alps" matches before the file "Alpha".
        assert_eq!(
            name(find_prefix(&source, &order, None, by_name, "al")),
            Some("alps".into())
        );
        assert_eq!(
            name(find_prefix(&source, &order, None, by_name, "ALP")),
            Some("alps".into())
        );
        assert_eq!(
            name(find_prefix(&source, &order, None, by_name, "alph")),
            Some("Alpha".into())
        );
        assert_eq!(
            name(find_prefix(&source, &order, None, by_name, "read")),
            Some("Read me".into())
        );
        assert_eq!(
            name(find_prefix(&source, &order, None, by_name, "readm")),
            Some("README".into())
        );
        assert_eq!(
            name(find_prefix(&source, &order, None, by_name, "z")),
            Some("zeta".into())
        );
        assert_eq!(find_prefix(&source, &order, None, by_name, "q"), None);
        assert_eq!(find_prefix(&source, &order, None, by_name, "zz"), None);
        assert_eq!(find_prefix(&source, &order, None, by_name, ""), None);
        // A prefix with digits: "a1" matches "a10", which sorts after "a2".
        assert_eq!(
            name(find_prefix(&source, &order, None, by_name, "a1")),
            Some("a10".into())
        );

        // Any other order scans: the first match in display order wins.
        let by_size = SortSpec {
            column: SortColumn::Size,
            descending: true,
        };
        let order = sort_rows(&rows, by_size);
        let found = find_prefix(&source, &order, None, by_size, "a").unwrap();
        let first_a = order
            .iter()
            .position(|&a| rows.get(a).unwrap().name.to_lowercase().starts_with('a'))
            .unwrap();
        assert_eq!(found, first_a);
    }

    #[test]
    fn prefix_search_matches_a_linear_scan_on_many_names() {
        let names: Vec<String> = (0..2_000)
            .map(|i| {
                let letters = ["ab", "Ab", "abc", "b", "B_", "\u{e4}x", "x"];
                format!("{}{}", letters[i % letters.len()], i % 37)
            })
            .collect();
        let entries: Vec<(&str, NodeKind)> = names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let kind = if i % 9 == 0 {
                    NodeKind::Directory
                } else {
                    NodeKind::File
                };
                (n.as_str(), kind)
            })
            .collect();
        let rows = rows_of(&entries);
        let source = RowSource::Folder(rows.clone());
        for spec in [
            SortSpec::default(),
            SortSpec {
                column: SortColumn::Size,
                descending: true,
            },
            SortSpec {
                column: SortColumn::Kind,
                descending: false,
            },
        ] {
            let order = sort_rows(&rows, spec);
            let positions = Positions::new(Arc::clone(&order)).unwrap();
            // Built for another order: must not be used.
            let stale = Positions::new(Arc::from(order.iter().rev().copied().collect::<Vec<_>>()));
            for prefix in [
                "a", "AB", "abc", "b", "b_", "\u{c4}", "x", "y", "ab1", "B_3", "",
            ] {
                let linear = if prefix.is_empty() {
                    None
                } else {
                    order
                        .iter()
                        .position(|&a| starts_with_folded(rows.get(a).unwrap().name, prefix))
                };
                let label = format!("{spec:?} {prefix:?}");
                assert_eq!(
                    find_prefix(&source, &order, None, spec, prefix),
                    linear,
                    "{label}"
                );
                assert_eq!(
                    find_prefix(&source, &order, Some(&positions), spec, prefix),
                    linear,
                    "{label}"
                );
                assert_eq!(
                    find_prefix(&source, &order, stale.as_ref(), spec, prefix),
                    linear,
                    "{label}"
                );
            }
        }
    }

    #[test]
    fn cursor_lookup_through_positions() {
        let mut shown = folder_shown();
        let order = Arc::clone(&shown.order);
        let last = *order.last().unwrap();
        shown.selection.replace(last);
        // No inverse: the scan.
        assert_eq!(shown.find_cursor(), Some(order.len() - 1));
        // The inverse of this order: the same answer.
        shown.positions = Positions::new(Arc::clone(&order));
        assert_eq!(shown.find_cursor(), Some(order.len() - 1));
        // A new order with the old inverse (out of step): the scan of the new one answers.
        let reversed: Arc<[u32]> = order.iter().rev().copied().collect();
        shown.order = Arc::clone(&reversed);
        assert_eq!(shown.find_cursor(), Some(0));
        // In step again.
        shown.positions = Positions::new(Arc::clone(&reversed));
        assert_eq!(shown.find_cursor(), Some(0));
        // A cursor row the order does not hold.
        shown.selection.replace(1_000);
        assert_eq!(shown.find_cursor(), None);
    }

    #[test]
    fn results_order_in_the_background_task() {
        let index = small_index();
        let never = AtomicBool::new(false);
        let query = Query {
            text: "o".into(),
            ..Query::default()
        };
        let hits = index.search(&query, None, &never).expect("not cancelled");
        assert!(hits.len() > 1);
        // Relevance: the hits' own order, with its inverse.
        let (order, positions) = results_order(&index, &hits, None, false, &never).unwrap();
        assert_eq!(order, hits.entries_in_order());
        let positions = positions.unwrap();
        assert!(Arc::ptr_eq(positions.order(), &order));
        for (position, &entry) in order.iter().enumerate() {
            assert_eq!(positions.get(entry), Some(position));
        }
        // A column: the engine's order, with its inverse.
        let (order, positions) =
            results_order(&index, &hits, Some(HitOrder::Name), true, &never).unwrap();
        assert_eq!(
            order,
            order_hits(&index, &hits, HitOrder::Name, true, &never).unwrap()
        );
        let positions = positions.unwrap();
        for (position, &entry) in order.iter().enumerate() {
            assert_eq!(positions.get(entry), Some(position));
        }
        // Cancelled (newer results or a newer sort): nothing to apply.
        let cancelled = AtomicBool::new(true);
        assert!(results_order(&index, &hits, None, false, &cancelled).is_none());
        assert!(results_order(&index, &hits, Some(HitOrder::Size), false, &cancelled).is_none());
    }

    #[test]
    fn only_browsable_sources_are_loaded() {
        assert_eq!(unavailable_state(&Availability::Browsable), None);
        let unsupported = Availability::Unsupported {
            format: "HFS+".into(),
        };
        assert_eq!(
            unavailable_state(&unsupported),
            Some(LoadState::Unavailable(unsupported))
        );
        assert_eq!(
            unavailable_state(&Availability::NeedsElevation),
            Some(LoadState::Unavailable(Availability::NeedsElevation))
        );
        let failed = Availability::Failed("This image could not be opened (x).".into());
        assert_eq!(
            unavailable_state(&failed),
            Some(LoadState::Unavailable(failed))
        );
    }

    #[test]
    fn failure_texts() {
        let context = IoContext::Read { offset: 0, len: 1 };
        assert_eq!(
            failure_text(&Error::AccessDenied {
                context: context.clone()
            }),
            "You do not have permission to open this folder."
        );
        assert_eq!(
            failure_text(&Error::NotFound { node: 3 }),
            "This folder no longer exists."
        );
        assert_eq!(
            failure_text(&Error::io(
                context.clone(),
                io::Error::from(io::ErrorKind::NotFound)
            )),
            "This folder no longer exists."
        );
        assert_eq!(
            failure_text(&Error::NotADirectory { node: 3 }),
            "This item is not a folder."
        );
        assert_eq!(
            failure_text(&Error::unsupported("compressed folders")),
            "This folder could not be read (not supported: compressed folders)."
        );
        assert_eq!(
            partial_text(&Error::unsupported("x")),
            "Part of this folder could not be read (not supported: x)."
        );
    }

    /// A filesystem that only scans: `entries` (node, parent node, name, kind) in that order,
    /// then `generated` folders of `files` files each. The root is node 1.
    struct ScanFs {
        info: VolumeInfo,
        entries: Vec<(u64, u64, String, NodeKind)>,
        generated: Option<(u64, u64)>,
    }

    impl ScanFs {
        fn new(entries: Vec<(u64, u64, String, NodeKind)>, generated: Option<(u64, u64)>) -> Self {
            // `VolumeInfo` is `#[non_exhaustive]`: borrow one from a real filesystem.
            let info = NativeFs::new(std::env::temp_dir(), "Volume")
                .expect("temp dir opens")
                .info();
            Self {
                info,
                entries,
                generated,
            }
        }

        fn index(self) -> Index {
            build(&self, &AtomicBool::new(false), &mut |_| {})
                .expect("scan succeeds")
                .expect("not cancelled")
        }
    }

    fn scan_entry(node: u64, parent: u64, name: &str, kind: NodeKind) -> ScanEntry<'_> {
        ScanEntry {
            node: NodeId(node),
            parent: NodeId(parent),
            name,
            kind,
            size: (kind == NodeKind::File).then_some(10),
            resource_size: None,
            modified: None,
            flags: EntryFlags::EMPTY,
            finder_type: None,
        }
    }

    impl FileSystem for ScanFs {
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
            Err(Error::NotADirectory { node: dir.0 })
        }
        fn scan(
            &self,
            visit: &mut dyn FnMut(ScanEntry<'_>) -> ControlFlow<()>,
        ) -> Result<ScanSummary> {
            let mut summary = ScanSummary::default();
            for (node, parent, name, kind) in &self.entries {
                summary.entries += 1;
                if visit(scan_entry(*node, *parent, name, *kind)).is_break() {
                    return Ok(summary);
                }
            }
            let Some((folders, files)) = self.generated else {
                return Ok(summary);
            };
            let mut name = String::new();
            for d in 0..folders {
                name.clear();
                name.push_str(&format!("d{d}"));
                summary.entries += 1;
                let _ = visit(scan_entry(1_000 + d, 1, &name, NodeKind::Directory));
            }
            for d in 0..folders {
                for f in 0..files {
                    name.clear();
                    name.push_str(&format!("f{d}_{f}"));
                    summary.entries += 1;
                    let node = 100_000_000 + d * files + f;
                    let _ = visit(scan_entry(node, 1_000 + d, &name, NodeKind::File));
                }
            }
            Ok(summary)
        }
    }

    /// Root (node 1) / Photos (2) / 2024 (3) / a.jpg (4), b.jpg (5); root / notes.txt (6);
    /// root / Projects (7) / photo list.txt (8).
    fn small_index() -> Index {
        let dir = NodeKind::Directory;
        let file = NodeKind::File;
        let e = |node: u64, parent: u64, name: &str, kind| (node, parent, name.to_owned(), kind);
        ScanFs::new(
            vec![
                e(2, 1, "Photos", dir),
                e(6, 1, "notes.txt", file),
                e(7, 1, "Projects", dir),
                e(3, 2, "2024", dir),
                e(8, 7, "photo list.txt", file),
                e(4, 3, "a.jpg", file),
                e(5, 3, "b.jpg", file),
            ],
            None,
        )
        .index()
    }

    /// The entry number of the entry named `name`.
    fn entry_named(index: &Index, name: &str) -> u32 {
        (0..index.len())
            .find(|&i| index.name(i) == name)
            .unwrap_or_else(|| panic!("no entry {name:?}"))
    }

    /// The names of `rows`, in arrival order.
    fn names(rows: &RowSource) -> Vec<String> {
        let rows = rows.folder().expect("folder rows");
        rows.iter()
            .map(|(_, entry)| entry.name.to_owned())
            .collect()
    }

    /// A folder view sorted by size, two rows selected, the cursor on the second.
    fn folder_shown() -> Shown {
        let dir = NodeKind::Directory;
        let file = NodeKind::File;
        let rows = rows_of(&[
            ("a", file),
            ("b", dir),
            ("c", file),
            ("d", file),
            ("e", file),
        ]);
        let sort = SortSpec {
            column: SortColumn::Size,
            descending: true,
        };
        let order = sort_rows(&rows, sort);
        let mut shown = Shown::new(sort);
        shown.rows = RowSource::Folder(rows);
        shown.selection.replace(order[1]);
        shown.selection.toggle(order[3]);
        shown.cursor_position = Some(3);
        shown.order = order;
        shown.state = LoadState::Ready;
        shown.note = Some("Part of this folder could not be read.".into());
        shown
    }

    #[test]
    fn stash_then_restore_gives_back_the_folder() {
        let mut shown = folder_shown();
        let before_names = names(&shown.rows);
        let before_order = Arc::clone(&shown.order);
        let before_selection = shown.selection.clone();
        let sort = shown.sort;

        let stash = FolderView::take(&mut shown);
        assert!(shown.rows.is_empty() && shown.order.is_empty());
        assert_eq!(shown.selection.count(), 0);
        assert_eq!(shown.cursor_position, None);
        assert_eq!(text(&stash.focus_name()), Some(names_at(&stash, 3)));

        // Results in between change everything.
        let index = Arc::new(small_index());
        shown.rows = RowSource::Index(Arc::clone(&index));
        shown.order = Arc::from(vec![1, 2, 3]);
        shown.sort = RELEVANCE;
        shown.selection.replace(2);
        shown.cursor_position = Some(1);

        assert_eq!(stash.restore(&mut shown), Restored::Shown);
        assert_eq!(names(&shown.rows), before_names);
        assert!(Arc::ptr_eq(&shown.order, &before_order));
        assert_eq!(shown.sort, sort);
        assert_eq!(shown.cursor_position, Some(3));
        assert_eq!(shown.state, LoadState::Ready);
        assert_eq!(
            text(&shown.note),
            Some("Part of this folder could not be read.")
        );
        let selection = &shown.selection;
        assert_eq!(selection.count(), before_selection.count());
        assert_eq!(selection.cursor(), before_selection.cursor());
        for id in 0..8 {
            assert_eq!(
                selection.is_selected(id),
                before_selection.is_selected(id),
                "{id}"
            );
        }
        // The anchor is private: a range selection from it gives the same rows.
        let (mut restored, mut original) = (selection.clone(), before_selection);
        restored.extend_to(0, &shown.order);
        original.extend_to(0, &shown.order);
        for id in 0..8 {
            assert_eq!(restored.is_selected(id), original.is_selected(id), "{id}");
        }
    }

    fn text(s: &Option<SharedString>) -> Option<&str> {
        s.as_ref().map(|s| s.as_ref())
    }

    fn names_at(stash: &FolderView, position: usize) -> &str {
        stash.rows.get(stash.order[position]).unwrap().name
    }

    #[test]
    fn a_stash_taken_while_loading_restores_as_reload() {
        let mut shown = folder_shown();
        shown.state = LoadState::Loading;
        let cursor_name = shown
            .rows
            .name(shown.selection.cursor().unwrap())
            .unwrap()
            .to_owned();
        let sort = shown.sort;
        let stash = FolderView::take(&mut shown);
        shown.sort = RELEVANCE;
        assert_eq!(stash.restore(&mut shown), Restored::Reload);
        assert_eq!(text(&shown.pending_focus), Some(cursor_name.as_str()));
        assert_eq!(shown.sort, sort);
        assert!(shown.rows.is_empty() && shown.order.is_empty());
        assert_eq!(shown.selection.count(), 0);

        // No cursor yet: the name the load was waiting for is kept.
        let mut shown = folder_shown();
        shown.state = LoadState::Loading;
        shown.selection.reset();
        shown.pending_focus = Some("zeta".into());
        let stash = FolderView::take(&mut shown);
        assert_eq!(stash.restore(&mut shown), Restored::Reload);
        assert_eq!(text(&shown.pending_focus), Some("zeta"));
    }

    #[test]
    fn a_stash_taken_while_resorting_restores_as_resort() {
        let mut shown = folder_shown();
        shown.resorting = true;
        let stash = FolderView::take(&mut shown);
        assert!(!shown.resorting);
        assert_eq!(stash.restore(&mut shown), Restored::Resort);
        // The rows are back; the caller starts the re-sort, which sets the flag again.
        assert_eq!(names(&shown.rows).len(), 5);
        assert!(!shown.resorting);
    }

    #[test]
    fn index_rows_past_the_end_are_none() {
        let index = Arc::new(small_index());
        let len = index.len();
        assert_eq!(len, 8);
        let rows = RowSource::Index(Arc::clone(&index));
        assert_eq!(rows.len(), 8);
        assert!(!rows.is_empty());
        assert!(rows.folder().is_none());
        for id in [len, len + 1, u32::MAX] {
            assert!(rows.get(id).is_none(), "{id}");
            assert!(rows.name(id).is_none(), "{id}");
        }
        let last = rows.get(len - 1).unwrap();
        assert_eq!(rows.name(len - 1), Some(last.name));
        let a = entry_named(&index, "a.jpg");
        assert_eq!(rows.get(a).unwrap().node, NodeId(4));
    }

    #[test]
    fn results_columns() {
        assert_eq!(hit_order(SortColumn::Name), Some(HitOrder::Name));
        assert_eq!(hit_order(SortColumn::Modified), Some(HitOrder::Modified));
        assert_eq!(hit_order(SortColumn::Size), Some(HitOrder::Size));
        assert_eq!(
            hit_order(SortColumn::ResourceFork),
            Some(HitOrder::ResourceSize)
        );
        assert_eq!(hit_order(SortColumn::Kind), Some(HitOrder::Kind));
        assert_eq!(hit_order(SortColumn::Folder), Some(HitOrder::Folder));
        assert_eq!(hit_order(SortColumn::Relevance), None);

        assert!(!folder_column(SortColumn::Relevance));
        assert!(!folder_column(SortColumn::Folder));
        for column in [
            SortColumn::Name,
            SortColumn::Modified,
            SortColumn::Size,
            SortColumn::ResourceFork,
            SortColumn::Kind,
        ] {
            assert!(folder_column(column), "{column:?}");
        }
    }

    /// The rows `Photos` (node 2), `Projects` (7) and `notes.txt` (6) as one page.
    fn page_of_three() -> Arc<DirBatch> {
        let entry = |node: u64, name: &'static str, kind, size| DirEntry {
            node: NodeId(node),
            name,
            kind,
            size,
            resource_size: None,
            modified: None,
            flags: EntryFlags::EMPTY,
        };
        let mut page = DirBatch::new();
        page.push(entry(6, "notes.txt", NodeKind::File, Some(5)));
        page.push(entry(2, "Photos", NodeKind::Directory, None));
        page.push(entry(7, "Projects", NodeKind::Directory, None));
        Arc::new(page)
    }

    fn sized_rows() -> (Rows, Arc<IndexSizes>) {
        let index = small_index();
        let table =
            Arc::new(IndexSizes::build(&index, &AtomicBool::new(false)).expect("not cancelled"));
        let page = page_of_three();
        let mut rows = Rows::default();
        rows.push(Arc::clone(&page));
        rows.push_totals(resolve_page(&table, 0, &page));
        (rows, table)
    }

    #[test]
    fn a_folder_row_shows_its_total_after_the_build() {
        let (rows, _) = sized_rows();
        // Photos holds a.jpg and b.jpg, Projects one file; notes.txt is a file.
        assert_eq!(rows.row_size(1).bytes(), Some(20));
        assert_eq!(rows.row_size(2).bytes(), Some(10));
        assert_eq!(rows.row_size(0), RowSize::None);
        assert_eq!(rows.row_size(1).state(), SizeState::Exact(20));
        // Before the table arrives the same rows have no total.
        let mut bare = Rows::default();
        bare.push(page_of_three());
        assert_eq!(bare.row_size(1).state(), SizeState::Unknown);
        // A table flagged lower bound reads "over".
        let mut bound = rows.clone();
        bound.set_totals(rows.totals().to_vec(), true);
        assert_eq!(bound.row_size(1).state(), SizeState::AtLeast(20));
    }

    #[test]
    fn sorting_by_size_orders_folders_by_their_totals() {
        let (rows, _) = sized_rows();
        let by_size = |descending| SortSpec {
            column: SortColumn::Size,
            descending,
        };
        // Folders first, the smaller total first; then the file.
        assert_eq!(&*sort_rows(&rows, by_size(false)), &[2, 1, 0]);
        assert_eq!(&*sort_rows(&rows, by_size(true)), &[1, 2, 0]);
        // Without totals the folders tie and fall back to their names.
        let mut bare = Rows::default();
        bare.push(page_of_three());
        assert_eq!(&*sort_rows(&bare, by_size(false)), &[1, 2, 0]);
    }

    #[test]
    fn sort_all_trigger() {
        // Everything sorted: never.
        assert!(!needs_sort_all(4096, 4096, 5000));
        assert!(!needs_sort_all(0, 0, 0));
        // Within 512 rows of the head's end.
        assert!(!needs_sort_all(4096, 10_000, 3583));
        assert!(needs_sort_all(4096, 10_000, 3584));
        assert!(needs_sort_all(4096, 10_000, 9_999));
        assert!(needs_sort_all(4096, 10_000, usize::MAX));
        assert!(needs_sort_all(0, 5, 0));
    }

    #[test]
    fn folder_cache_never_holds_more_than_its_cap() {
        let cache = FolderCache::default();
        let made = Cell::new(0usize);
        for key in 0..=FOLDER_CACHE_CAP as u32 {
            let path = cache.get_or_make(key, || {
                made.set(made.get() + 1);
                format!("p{key}").into()
            });
            assert_eq!(path, format!("p{key}"));
            assert!(cache.len() <= FOLDER_CACHE_CAP);
        }
        // The 4097th parent emptied the full cache before going in.
        assert_eq!(cache.len(), 1);
        assert_eq!(made.get(), FOLDER_CACHE_CAP + 1);
        // A cached path is not made again.
        let path = cache.get_or_make(FOLDER_CACHE_CAP as u32, || unreachable!());
        assert_eq!(path, format!("p{FOLDER_CACHE_CAP}"));
        cache.clear();
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn folder_paths_of_results() {
        let index = small_index();
        let root = index.name(0).to_owned();
        let cache = FolderCache::default();
        let a = entry_named(&index, "a.jpg");
        let b = entry_named(&index, "b.jpg");
        let notes = entry_named(&index, "notes.txt");
        let photos = entry_named(&index, "Photos");
        assert_eq!(cache.folder_of(&index, a), format!("{root}/Photos/2024"));
        assert_eq!(cache.folder_of(&index, b), format!("{root}/Photos/2024"));
        // Both files share their parent's cache entry.
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.folder_of(&index, notes), root);
        assert_eq!(cache.folder_of(&index, photos), root);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.folder_of(&index, 0), "");
        assert_eq!(cache.folder_of(&index, index.len()), "");
        assert_eq!(cache.folder_of(&index, u32::MAX), "");
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn result_crumbs_rename_the_root() {
        let label = SharedString::from("Backup disk");
        assert!(result_crumbs(Vec::new(), &label).is_none());

        let index = small_index();
        let a = entry_named(&index, "a.jpg");
        let parent = index.parent(a).unwrap();
        let crumbs = result_crumbs(index.crumbs(parent), &label).unwrap();
        let names: Vec<&str> = crumbs.iter().map(|c| c.name.as_ref()).collect();
        assert_eq!(names, ["Backup disk", "Photos", "2024"]);
        let nodes: Vec<NodeId> = crumbs.iter().map(|c| c.node).collect();
        assert_eq!(nodes, [NodeId(1), NodeId(2), NodeId(3)]);

        // The root alone is the source's root.
        let crumbs = result_crumbs(index.crumbs(0), &label).unwrap();
        assert_eq!(crumbs.len(), 1);
        assert_eq!(crumbs[0].name, label);
        // Out of range: nothing to navigate to.
        assert!(result_crumbs(index.crumbs(index.len()), &label).is_none());
    }

    /// A fresh folder under the system temp folder, removed when dropped.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "disk-atlas-listing-test-{label}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("creates the folder");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A host folder `top/a/b/c.txt`, `top/a/x.txt`, `top/n.txt`, with its filesystem and the
    /// index of a scan of it (scan-local ids).
    fn host_fixture(label: &str) -> (TempDir, NativeFs, Arc<Index>) {
        let dir = TempDir::new(label);
        std::fs::create_dir_all(dir.0.join("a").join("b")).unwrap();
        std::fs::write(dir.0.join("a").join("b").join("c.txt"), b"c").unwrap();
        std::fs::write(dir.0.join("a").join("x.txt"), b"x").unwrap();
        std::fs::write(dir.0.join("n.txt"), b"n").unwrap();
        let fs = NativeFs::new(dir.0.clone(), "Home").expect("folder opens");
        let index = build(&fs, &AtomicBool::new(false), &mut |_| {})
            .expect("scan succeeds")
            .expect("not cancelled");
        (dir, fs, Arc::new(index))
    }

    fn hit(index: &Arc<Index>, name: &str) -> Reach {
        Reach::Hit {
            index: Arc::clone(index),
            entry: entry_named(index, name),
            epoch: 1,
            known: None,
        }
    }

    #[test]
    fn a_nested_hit_resolves_to_the_id_a_direct_lookup_gives() {
        let (_dir, fs, index) = host_fixture("nested");
        let c = hit(&index, "c.txt");
        // The index's id for it is scan-local: the filesystem does not know it.
        let local = c.provisional();
        assert!(local >= disk_atlas_lib::fs::native::FIRST_SCAN_ID);
        assert!(fs.metadata(local).is_err());

        let a = fs.lookup(fs.root(), "a").unwrap().unwrap();
        let b = fs.lookup(a, "b").unwrap().unwrap();
        let direct = fs.lookup(b, "c.txt").unwrap().unwrap();
        assert_eq!(c.resolve(&fs).unwrap(), Some(direct));
        assert!(fs.metadata(direct).is_ok());
        // A hit at the top, a folder hit, and the root itself.
        let n = fs.lookup(fs.root(), "n.txt").unwrap().unwrap();
        assert_eq!(hit(&index, "n.txt").resolve(&fs).unwrap(), Some(n));
        assert_eq!(hit(&index, "b").resolve(&fs).unwrap(), Some(b));
        let root = Reach::Hit {
            index: Arc::clone(&index),
            entry: 0,
            epoch: 1,
            known: None,
        };
        assert_eq!(root.resolve(&fs).unwrap(), Some(fs.root()));
    }

    #[test]
    fn a_vanished_hit_is_gone() {
        let (dir, fs, index) = host_fixture("vanished");
        std::fs::remove_file(dir.0.join("a").join("x.txt")).unwrap();
        assert_eq!(hit(&index, "x.txt").resolve(&fs).unwrap(), None);
        // The rest still resolves.
        assert!(hit(&index, "c.txt").resolve(&fs).unwrap().is_some());
    }

    #[test]
    fn a_hit_whose_parent_was_renamed_is_gone() {
        let (dir, fs, index) = host_fixture("renamed");
        std::fs::rename(dir.0.join("a"), dir.0.join("a2")).unwrap();
        assert_eq!(hit(&index, "c.txt").resolve(&fs).unwrap(), None);
        assert_eq!(hit(&index, "b").resolve(&fs).unwrap(), None);
        // Outside the renamed folder nothing changed.
        assert!(hit(&index, "n.txt").resolve(&fs).unwrap().is_some());
    }

    #[test]
    fn a_known_id_and_a_plain_node_need_no_lookup() {
        let (_dir, fs, index) = host_fixture("known");
        let mut c = hit(&index, "c.txt");
        assert_eq!(c.known(), None);
        let Reach::Hit { known, .. } = &mut c else {
            unreachable!()
        };
        *known = Some(NodeId(42));
        assert_eq!(c.known(), Some(NodeId(42)));
        assert_eq!(c.resolve(&fs).unwrap(), Some(NodeId(42)));
        assert_eq!(c.provisional(), NodeId(42));

        let node = Reach::Node(NodeId(7));
        assert_eq!(node.known(), Some(NodeId(7)));
        assert_eq!(node.resolve(&fs).unwrap(), Some(NodeId(7)));
        // The key tells hits of other results or entries apart.
        let other = Reach::Hit {
            index: Arc::clone(&index),
            entry: entry_named(&index, "c.txt"),
            epoch: 2,
            known: None,
        };
        assert_ne!(other.key(), hit(&index, "c.txt").key());
        assert_ne!(hit(&index, "n.txt").key(), hit(&index, "c.txt").key());
        assert_eq!(c.key(), hit(&index, "c.txt").key());
    }

    #[test]
    fn reach_path_takes_real_ids_along_the_way() {
        let (_dir, fs, index) = host_fixture("path");
        let c = entry_named(&index, "c.txt");
        let walked = reach_path(&fs, index.crumbs(c)).unwrap().unwrap();
        let names: Vec<&str> = walked.iter().map(|(_, name)| name.as_str()).collect();
        assert_eq!(names, ["Home", "a", "b", "c.txt"]);
        assert_eq!(walked[0].0, fs.root());
        for (node, name) in &walked {
            assert!(fs.metadata(*node).is_ok(), "{name}");
        }
        // The crumbs of a location are those ids.
        let label = SharedString::from("This computer");
        let crumbs = result_crumbs(walked.clone(), &label).unwrap();
        assert_eq!(crumbs.last().unwrap().node, walked[3].0);
        // An empty path (an entry too deep to name) walks nowhere.
        assert!(reach_path(&fs, Vec::new()).unwrap().unwrap().is_empty());
    }

    #[test]
    fn resolved_ids_stay_under_their_cap() {
        let mut ids = ResolvedIds::default();
        for entry in 0..RESOLVED_CAP as u32 {
            ids.insert(entry, NodeId(u64::from(entry) + 1));
        }
        assert_eq!(ids.nodes.len(), RESOLVED_CAP);
        assert_eq!(ids.get(10), Some(NodeId(11)));
        // Replacing one does not empty the table.
        ids.insert(10, NodeId(99));
        assert_eq!(ids.nodes.len(), RESOLVED_CAP);
        assert_eq!(ids.get(10), Some(NodeId(99)));
        // A new entry in a full table empties it first.
        ids.insert(RESOLVED_CAP as u32, NodeId(5));
        assert_eq!(ids.nodes.len(), 1);
        assert_eq!(ids.get(10), None);
        assert_eq!(ids.get(RESOLVED_CAP as u32), Some(NodeId(5)));
    }

    #[test]
    fn prefix_search_in_results() {
        let index = Arc::new(small_index());
        let rows = RowSource::Index(Arc::clone(&index));
        let hits: Vec<u32> = (1..index.len()).collect();
        let linear = |order: &[u32], prefix: &str| {
            order
                .iter()
                .position(|&id| starts_with_folded(index.name(id), prefix))
        };

        // Relevance (any order) scans, with or without the inverse.
        let order: Arc<[u32]> = hits.iter().rev().copied().collect();
        let positions = Positions::new(Arc::clone(&order));
        for prefix in ["ph", "PHOTO", "a", "n", "2", "x"] {
            assert_eq!(
                find_prefix(&rows, &order, None, RELEVANCE, prefix),
                linear(&order[..], prefix),
                "{prefix:?}"
            );
            assert_eq!(
                find_prefix(&rows, &order, positions.as_ref(), RELEVANCE, prefix),
                linear(&order[..], prefix),
                "{prefix:?}"
            );
        }

        // By name: the binary search finds the first match.
        let by_name = SortSpec::default();
        let mut sorted = hits.clone();
        sorted.sort_by(|&x, &y| {
            compare_entries(by_name, &index.entry(x).unwrap(), &index.entry(y).unwrap())
        });
        let order: Arc<[u32]> = Arc::from(sorted);
        for prefix in ["ph", "PHOTO", "photo ", "a", "b", "n", "p", "x"] {
            assert_eq!(
                find_prefix(&rows, &order, None, by_name, prefix),
                linear(&order[..], prefix),
                "{prefix:?}"
            );
        }

        // An order that does not follow the app's name order: a miss of the binary search falls
        // back to the scan in results (a folder would answer `None`).
        let scrambled: Arc<[u32]> = Arc::from(vec![
            entry_named(&index, "b.jpg"),
            entry_named(&index, "notes.txt"),
            entry_named(&index, "a.jpg"),
        ]);
        assert_eq!(find_prefix(&rows, &scrambled, None, by_name, "a"), Some(2));
    }

    /// Best of `runs` timings of `f`, in milliseconds.
    fn best_ms(runs: usize, mut f: impl FnMut()) -> f64 {
        let mut best = Duration::MAX;
        for _ in 0..runs {
            let started = Instant::now();
            f();
            best = best.min(started.elapsed());
        }
        best.as_secs_f64() * 1e3
    }

    /// At 5 million hits: what `show_results` still does on the UI thread (the head order), what
    /// moved to the background task (`Hits::entries_in_order` and the inverse), the cursor lookup
    /// after a sort with and without the inverse, and the background `sort_all` for comparison.
    /// Run with
    /// `cargo test -p disk-atlas --release -- --ignored perf_entries_in_order_5m --nocapture`.
    #[test]
    #[ignore]
    #[allow(clippy::print_stdout)]
    fn perf_entries_in_order_5m() {
        let index = ScanFs::new(Vec::new(), Some((5_000, 1_000))).index();
        let query = Query {
            text: "f".into(),
            ..Query::default()
        };
        let never = AtomicBool::new(false);
        let hits = index.search(&query, None, &never).expect("not cancelled");
        assert_eq!(hits.len(), 5_000_000);
        // The UI thread's part of `show_results`.
        let head_ms = best_ms(5, || {
            let head = hits.sorted_len().min(hits.len()).min(HEAD);
            let order: Arc<[u32]> = (0..head).filter_map(|p| hits.entry(p)).collect();
            assert_eq!(order.len(), HEAD);
        });
        // The background task's part.
        let in_order_ms = best_ms(5, || {
            assert_eq!(hits.entries_in_order().len(), hits.len());
        });
        let mut made = None;
        let background_ms = best_ms(5, || {
            made = results_order(&index, &hits, None, false, &never);
        });
        let (order, positions) = made.unwrap();
        let positions = positions.unwrap();
        // The cursor on the last row: the worst case of the scan.
        let cursor = *order.last().unwrap();
        let scan_ms = best_ms(5, || {
            assert_eq!(position_in(&order, None, cursor), Some(order.len() - 1));
        });
        let lookup_ms = best_ms(5, || {
            assert_eq!(
                position_in(&order, Some(&positions), cursor),
                Some(order.len() - 1)
            );
        });
        let started = Instant::now();
        let mut sorted = hits.clone();
        sorted.sort_all();
        let sort_all = started.elapsed();
        assert_eq!(sorted.sorted_len(), sorted.len());
        println!(
            "perf_entries_in_order_5m: UI thread: head order {head_ms:.3} ms, cursor lookup \
             {:.2} us with the inverse ({scan_ms:.2} ms by scan); background: \
             entries_in_order {in_order_ms:.2} ms, order + inverse {background_ms:.2} ms, \
             clone + sort_all {:.2} ms; search {:.2} ms (best of 5 each)",
            lookup_ms * 1e3,
            sort_all.as_secs_f64() * 1e3,
            hits.elapsed.as_secs_f64() * 1e3
        );
    }

    /// Times type-to-select over 100,000 rows, binary search and linear scan. Run with
    /// `cargo test -p disk-atlas --profile profiling -- --ignored perf_prefix_100k --nocapture`.
    #[test]
    #[ignore]
    #[allow(clippy::print_stdout)]
    fn perf_prefix_100k() {
        let names: Vec<String> = (0..100_000u64)
            .map(|i| {
                let x = i.wrapping_mul(0x9e37_79b9_7f4a_7c15);
                format!("{:x}file{}", x >> 40, i)
            })
            .collect();
        let entries: Vec<(&str, NodeKind)> =
            names.iter().map(|n| (n.as_str(), NodeKind::File)).collect();
        let rows = rows_of(&entries);
        let source = RowSource::Folder(rows.clone());
        for spec in [
            SortSpec::default(),
            SortSpec {
                column: SortColumn::Size,
                descending: false,
            },
        ] {
            let order = sort_rows(&rows, spec);
            let positions = Positions::new(Arc::clone(&order)).unwrap();
            // A prefix that matches nothing: the worst case for the scan.
            let started = Instant::now();
            let found = find_prefix(&source, &order, None, spec, "zz");
            let elapsed = started.elapsed();
            assert_eq!(found, None);
            let with_inverse = best_ms(5, || {
                assert_eq!(
                    find_prefix(&source, &order, Some(&positions), spec, "zz"),
                    None
                );
            });
            // The cursor lookup after a sort, cursor on the last row.
            let cursor = *order.last().unwrap();
            let scan = best_ms(5, || {
                assert_eq!(position_in(&order, None, cursor), Some(order.len() - 1));
            });
            let lookup = best_ms(5, || {
                assert_eq!(
                    position_in(&order, Some(&positions), cursor),
                    Some(order.len() - 1)
                );
            });
            println!(
                "perf_prefix_100k {spec:?}: {:.3} ms ({with_inverse:.3} ms with the inverse, best \
                 of 5); cursor lookup {:.2} us with the inverse, {scan:.3} ms by scan",
                elapsed.as_secs_f64() * 1e3,
                lookup * 1e3
            );
        }
    }

    // The merge of a folder read again, by name.

    fn files(names: &[&str]) -> Rows {
        let entries: Vec<(&str, NodeKind)> =
            names.iter().map(|&name| (name, NodeKind::File)).collect();
        rows_of(&entries)
    }

    /// The names of the rows selected in `selection`, in arrival order, and the cursor's name.
    fn selected_names(rows: &Rows, selection: &Selection) -> (Vec<String>, Option<String>) {
        let names = rows
            .iter()
            .filter(|(arrival, _)| selection.is_selected(*arrival))
            .map(|(_, entry)| entry.name.to_owned())
            .collect();
        let cursor = selection
            .cursor()
            .and_then(|c| rows.get(c))
            .map(|entry| entry.name.to_owned());
        (names, cursor)
    }

    fn identity(rows: &Rows) -> Vec<u32> {
        (0..rows.len() as u32).collect()
    }

    #[test]
    fn a_merge_keeps_the_selection_and_cursor_on_their_names() {
        let old = files(&["a", "b", "c", "d"]);
        let mut selection = Selection::new();
        selection.replace(1);
        selection.toggle(3);
        // "a" is gone, "bb" and "e" are new: the surviving names land on other arrivals.
        let new = files(&["bb", "b", "c", "d", "e"]);
        let merged = merge_by_name(&old, &identity(&old), &selection, &new, &identity(&new));
        let (names, cursor) = selected_names(&new, &merged);
        assert_eq!(names, ["b", "d"]);
        assert_eq!(cursor.as_deref(), Some("d"));
        assert_eq!(merged.count(), 2);
    }

    #[test]
    fn a_selected_name_that_vanished_leaves_the_selection() {
        let old = files(&["a", "b", "c"]);
        let mut selection = Selection::new();
        selection.replace(0);
        selection.toggle(1);
        let new = files(&["a", "c"]);
        let merged = merge_by_name(&old, &identity(&old), &selection, &new, &identity(&new));
        let (names, cursor) = selected_names(&new, &merged);
        assert_eq!(names, ["a"]);
        assert_eq!(merged.count(), 1);
        // The cursor was on "b": it falls to the row after it.
        assert_eq!(cursor.as_deref(), Some("c"));
    }

    #[test]
    fn a_vanished_cursor_goes_to_a_neighbour_then_to_the_same_position() {
        let old = files(&["a", "b", "c", "d"]);
        let mut selection = Selection::new();
        selection.replace(3);
        // The last row vanished: the one before it.
        let new = files(&["a", "b", "c"]);
        let merged = merge_by_name(&old, &identity(&old), &selection, &new, &identity(&new));
        assert_eq!(selected_names(&new, &merged), (vec![], Some("c".into())));
        // Nothing around it survived: the same position in the new order.
        let new = files(&["x", "y"]);
        let merged = merge_by_name(&old, &identity(&old), &selection, &new, &identity(&new));
        assert_eq!(selected_names(&new, &merged), (vec![], Some("y".into())));
        // Nothing left at all: no cursor.
        let new = files(&[]);
        let merged = merge_by_name(&old, &identity(&old), &selection, &new, &[]);
        assert_eq!(merged.cursor(), None);
    }

    #[test]
    fn a_cursor_without_a_selection_stays_unselected() {
        let old = files(&["a", "b"]);
        let mut selection = Selection::new();
        selection.replace(1);
        selection.clear();
        let new = files(&["b", "a"]);
        let merged = merge_by_name(&old, &identity(&old), &selection, &new, &identity(&new));
        assert_eq!(merged.count(), 0);
        assert_eq!(selected_names(&new, &merged), (vec![], Some("b".into())));
    }

    #[test]
    fn nothing_selected_merges_to_nothing() {
        let old = files(&["a", "b"]);
        let new = files(&["a", "b", "c"]);
        let merged = merge_by_name(
            &old,
            &identity(&old),
            &Selection::new(),
            &new,
            &identity(&new),
        );
        assert_eq!((merged.count(), merged.cursor()), (0, None));
    }
}
