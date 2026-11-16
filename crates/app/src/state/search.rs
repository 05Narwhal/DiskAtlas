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

//! Search state: per-source indexes, the build queue, the running query.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Write};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use disk_atlas_lib::error::Error;
use disk_atlas_lib::fs::{FileSystem, NodeId, VolumeIdentity};
use disk_atlas_lib::search::kind::KindSet;
use disk_atlas_lib::search::{Hits, Index, Query, build, persist, terms};
use gpui::{App, Context, Entity, EventEmitter, SharedString, Subscription, Task};

use super::listing::{Listing, ListingEvent, LoadState};
use super::navigation::{Location, Navigation, NavigationEvent};
use super::settings::Settings;
use super::sources::{Source, SourceId, SourceKind, Sources, SourcesEvent};
use super::visibility::Visibility;

/// How often the UI side reads a running build's entry count.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Most saved indexes kept in the cache folder.
const MAX_SAVED: usize = 32;

/// Most bytes all saved indexes may take together.
const MAX_SAVED_BYTES: u64 = 2 << 30;

/// Where a source's index stands.
pub enum IndexState {
    /// Not built yet; the source is queued.
    Absent,
    /// A build is running; this many entries have been scanned.
    Building { entries: u64 },
    /// Ready to search.
    Ready(Arc<Index>),
    /// The index could not be built. The text says why, for the user.
    Failed(SharedString),
}

/// What narrows a search besides its text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filters {
    /// Where: false is the whole volume, true is the folder shown when the search began.
    pub this_folder: bool,
    /// The kinds to admit; empty admits any.
    pub kinds: KindSet,
    /// Data size in bytes.
    pub size: Option<RangeInclusive<u64>>,
    /// Modification time in Unix nanoseconds.
    pub modified: Option<RangeInclusive<i64>>,
    /// Also admit hidden entries. A view option, not a filter.
    pub include_hidden: bool,
}

impl Filters {
    /// True when anything but `include_hidden` is set.
    pub fn is_active(&self) -> bool {
        self.this_folder || !self.kinds.is_empty() || self.size.is_some() || self.modified.is_some()
    }
}

/// What changed in [`Search`].
pub enum SearchEvent {
    /// The text, the filters, an index state or the results changed: the field and the footer
    /// redraw.
    Changed,
    /// An index was built or loaded: the folder sizes of the volume can be worked out from it.
    IndexReady {
        /// The source the index belongs to.
        source: SourceId,
        /// The index, finished.
        index: Arc<Index>,
    },
}

impl EventEmitter<SearchEvent> for Search {}

/// The search entity: one index per source, the build queue and the running query.
pub struct Search {
    sources: Entity<Sources>,
    navigation: Entity<Navigation>,
    listing: Entity<Listing>,
    indexes: HashMap<SourceId, IndexState>,
    queue: BuildQueue,
    /// The source whose build is running. At most one at a time.
    running: Option<SourceId>,
    build_tasks: HashMap<SourceId, BuildJob>,
    next_serial: u64,
    text: SharedString,
    filters: Filters,
    /// Bumped by every query and by every end of the search; results of an older one are dropped.
    generation: u64,
    /// Set when the running query is superseded.
    cancel: Arc<AtomicBool>,
    query_task: Option<Task<()>>,
    last: Option<LastSearch>,
    active: bool,
    /// The folder shown when the search began.
    origin: Option<Location>,
    /// `show_hidden` as last seen: a search starts with its Hidden items filter at this, and
    /// follows it when it changes.
    show_hidden: bool,
    _subscriptions: Vec<Subscription>,
}

/// A build in flight: its tasks and the flag that stops it.
struct BuildJob {
    serial: u64,
    cancel: Arc<AtomicBool>,
    _build: Task<()>,
    _progress: Task<()>,
}

impl Drop for BuildJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// The last query and its hits, kept so the next keystroke can narrow them.
struct LastSearch {
    index: Arc<Index>,
    query: Query,
    hits: Arc<Hits>,
}

impl Search {
    /// Creates the entity and queues the indexes of the sources already listed.
    pub fn new(
        sources: Entity<Sources>,
        navigation: Entity<Navigation>,
        listing: Entity<Listing>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.subscribe(
                &sources,
                |this, _sources, event: &SourcesEvent, cx| match event {
                    SourcesEvent::Changed => this.sync_sources(cx),
                    SourcesEvent::Selected(_) => {}
                },
            ),
            cx.subscribe(
                &listing,
                |this, _listing, event: &ListingEvent, cx| match event {
                    // A navigation ended the search; the listing has already left the results.
                    ListingEvent::ResultsDropped => this.end(cx),
                    ListingEvent::CursorMoved => {}
                },
            ),
            // The first listing goes first: a build held back for it starts when it is done.
            cx.observe(&listing, |this, _listing, cx| this.start_next(cx)),
            // "Read disks when opened" turned on queues the volumes that are listed.
            cx.observe_global::<Settings>(|this, cx| {
                this.sync_sources(cx);
                this.follow_show_hidden(cx);
            }),
            cx.subscribe(
                &navigation,
                |this, _navigation, event: &NavigationEvent, cx| match event {
                    NavigationEvent::Changed { .. } => this.navigated(cx),
                },
            ),
        ];
        let mut search = Self {
            sources,
            navigation,
            listing,
            indexes: HashMap::new(),
            queue: BuildQueue::default(),
            running: None,
            build_tasks: HashMap::new(),
            next_serial: 0,
            text: SharedString::default(),
            filters: Filters::default(),
            generation: 0,
            cancel: Arc::default(),
            query_task: None,
            last: None,
            active: false,
            origin: None,
            show_hidden: Visibility::current(cx).show_hidden,
            _subscriptions: subscriptions,
        };
        search.sync_sources(cx);
        search
    }

    /// Starts a search (Ctrl+F): remembers the folder shown, makes sure the current source gets
    /// an index soon, and runs the query if one is already set.
    pub fn activate(&mut self, cx: &mut Context<Self>) {
        if !self.active {
            self.active = true;
            self.filters.include_hidden = self.show_hidden;
            self.origin = self.navigation.read(cx).current().cloned();
        }
        if let Some(source) = self.current_source(cx) {
            self.want_index(source, cx);
        }
        cx.emit(SearchEvent::Changed);
        self.run_query(cx);
    }

    /// The `show_hidden` setting changed: the Hidden items filter of a search under way follows it.
    fn follow_show_hidden(&mut self, cx: &mut Context<Self>) {
        let now = Visibility::current(cx).show_hidden;
        if now == self.show_hidden {
            return;
        }
        self.show_hidden = now;
        if self.active {
            let filters = Filters {
                include_hidden: now,
                ..self.filters.clone()
            };
            self.set_filters(filters, cx);
        }
    }

    /// Ends the search and shows the folder again.
    pub fn leave(&mut self, cx: &mut Context<Self>) {
        self.reset();
        self.listing
            .update(cx, |listing, cx| listing.leave_results(cx));
        cx.emit(SearchEvent::Changed);
    }

    /// True while a search is under way.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// The text typed in the search field.
    pub fn text(&self) -> &SharedString {
        &self.text
    }

    /// Sets the text and runs the query.
    pub fn set_text(&mut self, text: SharedString, cx: &mut Context<Self>) {
        if self.text == text {
            return;
        }
        self.text = text;
        cx.emit(SearchEvent::Changed);
        self.run_query(cx);
    }

    /// The filters set.
    pub fn filters(&self) -> &Filters {
        &self.filters
    }

    /// Sets the filters and runs the query.
    pub fn set_filters(&mut self, filters: Filters, cx: &mut Context<Self>) {
        if self.filters == filters {
            return;
        }
        self.filters = filters;
        cx.emit(SearchEvent::Changed);
        self.run_query(cx);
    }

    /// Clears every filter. `include_hidden` is a view option and stays as it is.
    pub fn clear_filters(&mut self, cx: &mut Context<Self>) {
        let cleared = Filters {
            include_hidden: self.filters.include_hidden,
            ..Filters::default()
        };
        self.set_filters(cleared, cx);
    }

    /// Where the index of `source` stands. `None` for a source that is not indexed (and for a
    /// host source nobody searched yet).
    pub fn index_state(&self, source: SourceId) -> Option<&IndexState> {
        self.indexes.get(&source)
    }

    /// The finished index of `source`, shared; `None` until it is ready.
    pub fn index(&self, source: SourceId) -> Option<Arc<Index>> {
        match self.indexes.get(&source) {
            Some(IndexState::Ready(index)) => Some(Arc::clone(index)),
            _ => None,
        }
    }

    /// [`Self::index_state`] for the source `Navigation` shows.
    pub fn current_index_state(&self, cx: &App) -> Option<&IndexState> {
        let source = self.current_source(cx)?;
        self.indexes.get(&source)
    }

    fn current_source(&self, cx: &App) -> Option<SourceId> {
        self.navigation
            .read(cx)
            .current()
            .map(|location| location.source())
    }

    /// Ends the search without touching the listing: the caller knows it already left.
    fn end(&mut self, cx: &mut Context<Self>) {
        if !self.active {
            return;
        }
        self.reset();
        cx.emit(SearchEvent::Changed);
    }

    /// Drops the query, the text and the filters and goes inactive.
    fn reset(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::default();
        self.generation += 1;
        self.query_task = None;
        self.text = SharedString::default();
        self.filters = Filters::default();
        self.active = false;
        self.last = None;
        self.origin = None;
    }

    /// The user moved: a search under way is over, and the source shown now gets its turn.
    fn navigated(&mut self, cx: &mut Context<Self>) {
        self.end(cx);
        if let Some(source) = self.current_source(cx) {
            self.queue.prioritize(source);
        }
    }

    /// Follows the list of sources: queues new volumes, forgets the ones that are gone.
    fn sync_sources(&mut self, cx: &mut Context<Self>) {
        let on_open = index_on_open(cx);
        let listed: Vec<(SourceId, bool)> = self
            .sources
            .read(cx)
            .items()
            .iter()
            .map(|source| (source.id, indexed_on_open(source, on_open)))
            .collect();
        let present: HashSet<SourceId> = listed.iter().map(|(id, _)| *id).collect();

        let gone: Vec<SourceId> = self
            .indexes
            .keys()
            .chain(self.build_tasks.keys())
            .filter(|id| !present.contains(id))
            .copied()
            .collect();
        for id in gone {
            self.indexes.remove(&id);
            self.queue.remove(id);
            // Dropping the job sets its cancel flag.
            self.build_tasks.remove(&id);
            if self.running == Some(id) {
                self.running = None;
            }
        }

        for (id, eager) in listed {
            if eager && !self.indexes.contains_key(&id) {
                self.indexes.insert(id, IndexState::Absent);
                self.queue.push(id);
            }
        }
        if let Some(current) = self.current_source(cx) {
            self.queue.prioritize(current);
        }
        self.start_next(cx);
    }

    /// Makes sure `source` has, or is about to get, an index, and moves it up the queue.
    fn want_index(&mut self, source: SourceId, cx: &mut Context<Self>) {
        if !self.indexes.contains_key(&source) {
            let can_index = self.sources.read(cx).source(source).is_some_and(indexable);
            if !can_index {
                return;
            }
            self.indexes.insert(source, IndexState::Absent);
            self.queue.push(source);
        }
        self.queue.prioritize(source);
        self.start_next(cx);
    }

    /// The storage bar's "Calculate sizes": queues the index of `source` and moves it to the
    /// front, without starting a search.
    pub fn calculate_sizes(&mut self, source: SourceId, cx: &mut Context<Self>) {
        self.want_index(source, cx);
        cx.emit(SearchEvent::Changed);
    }

    /// Builds the index of `source` again: its volume changed under the old one. The old index
    /// and its saved copy (which no longer matches the volume's identity) are not used.
    pub fn rebuild(&mut self, source: SourceId, cx: &mut Context<Self>) {
        if self.running == Some(source)
            || !matches!(self.indexes.get(&source), Some(IndexState::Ready(_)))
        {
            return;
        }
        self.last = None;
        self.indexes.insert(source, IndexState::Absent);
        self.queue.push(source);
        self.queue.prioritize(source);
        cx.emit(SearchEvent::Changed);
        self.start_next(cx);
    }

    /// Starts the first queued build if none is running. Until the folder shown has been listed,
    /// only a build the user asked for by searching starts.
    fn start_next(&mut self, cx: &mut Context<Self>) {
        if self.running.is_some() {
            return;
        }
        if !self.active && self.listing.read(cx).state() == &LoadState::Loading {
            return;
        }
        while let Some(id) = self.queue.pop() {
            let source = self.sources.read(cx).source(id).cloned();
            match source {
                Some(source) => {
                    self.start_build(source, cx);
                    return;
                }
                None => {
                    self.indexes.remove(&id);
                }
            }
        }
    }

    /// Starts building (or loading) the index of `source` on the background executor.
    fn start_build(&mut self, source: Source, cx: &mut Context<Self>) {
        let id = source.id;
        let volume = matches!(source.kind, SourceKind::Volume { .. });
        let fs = source.fs;
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));
        self.next_serial += 1;
        let serial = self.next_serial;

        let build_task = {
            let cancel = Arc::clone(&cancel);
            let progress = Arc::clone(&progress);
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move { run_job(fs.as_ref(), volume, &cancel, &progress) })
                    .await;
                this.update(cx, |this, cx| this.finish_build(id, serial, result, cx))
                    .ok();
            })
        };
        let progress_task = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(PROGRESS_INTERVAL).await;
                let alive = this.update(cx, |this, cx| {
                    this.apply_progress(id, serial, progress.load(Ordering::Relaxed), cx)
                });
                if !matches!(alive, Ok(true)) {
                    break;
                }
            }
        });

        self.indexes.insert(id, IndexState::Building { entries: 0 });
        self.running = Some(id);
        self.build_tasks.insert(
            id,
            BuildJob {
                serial,
                cancel,
                _build: build_task,
                _progress: progress_task,
            },
        );
        cx.emit(SearchEvent::Changed);
    }

    /// Copies a running build's count into its state. False once that build is over.
    fn apply_progress(
        &mut self,
        id: SourceId,
        serial: u64,
        entries: u64,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.build_tasks.get(&id).map(|job| job.serial) != Some(serial) {
            return false;
        }
        if let Some(IndexState::Building { entries: shown }) = self.indexes.get_mut(&id)
            && *shown != entries
        {
            *shown = entries;
            cx.emit(SearchEvent::Changed);
        }
        true
    }

    /// A build ended: store its outcome, start the next one, and run a waiting query.
    fn finish_build(
        &mut self,
        id: SourceId,
        serial: u64,
        result: Result<Option<Index>, Error>,
        cx: &mut Context<Self>,
    ) {
        if self.build_tasks.get(&id).map(|job| job.serial) != Some(serial) {
            return;
        }
        self.build_tasks.remove(&id);
        if self.running == Some(id) {
            self.running = None;
        }
        match result {
            Ok(Some(index)) => {
                let index = Arc::new(index);
                self.indexes
                    .insert(id, IndexState::Ready(Arc::clone(&index)));
                cx.emit(SearchEvent::IndexReady { source: id, index });
            }
            Ok(None) => {
                self.indexes.remove(&id);
            }
            Err(error) => {
                tracing::warn!(%error, "building a search index failed");
                self.indexes
                    .insert(id, IndexState::Failed(failure_text(&error)));
            }
        }
        cx.emit(SearchEvent::Changed);
        self.start_next(cx);
        if self.active && self.current_source(cx) == Some(id) {
            self.run_query(cx);
        }
    }

    /// Runs the query for the text and filters set, in place of the one under way.
    fn run_query(&mut self, cx: &mut Context<Self>) {
        if !self.active {
            return;
        }
        if terms(&self.text).is_empty() && !self.filters.is_active() {
            self.cancel.store(true, Ordering::Relaxed);
            self.cancel = Arc::default();
            self.generation += 1;
            self.query_task = None;
            self.listing
                .update(cx, |listing, cx| listing.leave_results(cx));
            return;
        }
        let Some(source) = self.current_source(cx) else {
            return;
        };
        let Some(IndexState::Ready(index)) = self.indexes.get(&source) else {
            // It runs when the index arrives.
            return;
        };
        let index = Arc::clone(index);

        self.cancel.store(true, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Arc::clone(&cancel);
        self.generation += 1;
        let generation = self.generation;

        let text = self.text.clone();
        let filters = self.filters.clone();
        // An index over a host folder has scan-local ids, so its folder is found by name.
        let host = self
            .sources
            .read(cx)
            .source(source)
            .is_some_and(|source| matches!(source.kind, SourceKind::Host));
        let origin = if filters.this_folder {
            self.origin
                .as_ref()
                .filter(|origin| origin.source() == source)
                .map(|origin| Scope::of(origin, host))
        } else {
            None
        };
        let last = self
            .last
            .as_ref()
            .filter(|last| Arc::ptr_eq(&last.index, &index))
            .map(|last| (last.query.clone(), Arc::clone(&last.hits)));

        self.query_task = Some(cx.spawn(async move |this, cx| {
            let search_index = Arc::clone(&index);
            let (query, hits) = cx
                .background_executor()
                .spawn(async move {
                    // A scope that is not in the index gives no hits.
                    let scope = origin.map(|scope| scope.entry(&search_index));
                    let query = make_query(&text, &filters, scope);
                    let previous = last.as_ref().map(|(query, hits)| (query, hits.as_ref()));
                    let hits = search_index.search(&query, previous, &cancel);
                    (query, hits)
                })
                .await;
            let Some(hits) = hits else {
                return;
            };
            this.update(cx, |this, cx| {
                this.apply_hits(generation, index, query, hits, cx)
            })
            .ok();
        }));
    }

    /// Hands the hits of the query of `generation` to the listing; older ones are dropped.
    fn apply_hits(
        &mut self,
        generation: u64,
        index: Arc<Index>,
        query: Query,
        hits: Hits,
        cx: &mut Context<Self>,
    ) {
        if generation != self.generation || !self.active {
            return;
        }
        let hits = Arc::new(hits);
        let shown_terms: Arc<[String]> = Arc::from(terms(&self.text));
        self.last = Some(LastSearch {
            index: Arc::clone(&index),
            query,
            hits: Arc::clone(&hits),
        });
        self.listing.update(cx, |listing, cx| {
            listing.show_results(index, hits, shown_terms, cx)
        });
        cx.emit(SearchEvent::Changed);
    }
}

/// The folder a "This folder" search is limited to, before it is found in an index.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Scope {
    /// A folder of a volume, whose index holds the volume's own ids.
    Node(NodeId),
    /// A folder of the host, by the names from the root down (the root's own name left out):
    /// the index holds scan-local ids that mean nothing outside it.
    Names(Vec<String>),
}

/// The entry number that stands for a scope no index entry matches: no hits.
const NO_SCOPE: u32 = u32::MAX;

impl Scope {
    /// The scope of the folder `origin`; `host` is true when its source is the host.
    fn of(origin: &Location, host: bool) -> Self {
        if host {
            Self::Names(
                origin
                    .crumbs()
                    .iter()
                    .skip(1)
                    .map(|crumb| crumb.name.to_string())
                    .collect(),
            )
        } else {
            Self::Node(origin.node())
        }
    }

    /// The entry of `index` this scope names; [`NO_SCOPE`] (no hits) when the folder is not in
    /// the index. Linear in the index, so call it off the UI thread.
    fn entry(&self, index: &Index) -> u32 {
        match self {
            Self::Node(node) => index.find_node(*node),
            Self::Names(names) => find_by_names(index, names),
        }
        .unwrap_or(NO_SCOPE)
    }
}

/// The entry reached from the root by `names`, matching each name exactly among the children of
/// the entry before. The index has no child-by-name lookup: each level scans the subtree of the
/// folder found so far, which is at most the whole index, so the cost is bounded by depth times
/// entries. An empty `names` is the root. The first match wins if names repeat.
fn find_by_names<S: AsRef<str>>(index: &Index, names: &[S]) -> Option<u32> {
    if index.is_empty() {
        return None;
    }
    let mut at = 0;
    for name in names {
        let below = index.subtree(at);
        at = (below.start.saturating_add(1)..below.end)
            .find(|&entry| index.parent(entry) == Some(at) && index.name(entry) == name.as_ref())?;
    }
    Some(at)
}

/// True for a source that gets an index as soon as it is listed: a browsable volume.
fn indexed_eagerly(source: &Source) -> bool {
    matches!(source.kind, SourceKind::Volume { .. }) && source.availability.is_browsable()
}

/// True for a source that is indexed when it is listed: `on_open` is the setting "Read disks
/// when opened".
fn indexed_on_open(source: &Source, on_open: bool) -> bool {
    on_open && indexed_eagerly(source)
}

/// The setting "Read disks when opened": whether a volume is indexed as soon as it is listed.
/// On when there are no settings (tests).
fn index_on_open(cx: &App) -> bool {
    cx.try_global::<Settings>()
        .is_none_or(|settings| settings.bool_of("index_on_open"))
}

/// True for a source that can be indexed at all: a browsable volume, or a host folder on demand.
fn indexable(source: &Source) -> bool {
    matches!(source.kind, SourceKind::Volume { .. } | SourceKind::Host)
        && source.availability.is_browsable()
}

/// What the field says when an index could not be built.
fn failure_text(error: &Error) -> SharedString {
    format!("This volume could not be indexed ({error}), so it cannot be searched.").into()
}

/// The query for the text and filters, with `scope` already resolved to an entry number.
fn make_query(text: &str, filters: &Filters, scope: Option<u32>) -> Query {
    Query {
        text: text.to_owned(),
        scope,
        kinds: filters.kinds,
        size: filters.size.clone(),
        modified: filters.modified.clone(),
        include_hidden: filters.include_hidden,
        include_system: false,
    }
}

/// Sources waiting for their index, first in line first. A source appears once.
#[derive(Default)]
struct BuildQueue {
    waiting: VecDeque<SourceId>,
}

impl BuildQueue {
    /// Adds `id` at the back unless it is queued already.
    fn push(&mut self, id: SourceId) {
        if !self.waiting.contains(&id) {
            self.waiting.push_back(id);
        }
    }

    /// Moves `id` to the front if it is queued.
    fn prioritize(&mut self, id: SourceId) {
        if let Some(at) = self.waiting.iter().position(|&queued| queued == id) {
            self.waiting.remove(at);
            self.waiting.push_front(id);
        }
    }

    /// Takes the first.
    fn pop(&mut self) -> Option<SourceId> {
        self.waiting.pop_front()
    }

    /// Takes `id` out.
    fn remove(&mut self, id: SourceId) {
        self.waiting.retain(|&queued| queued != id);
    }
}

/// The operating systems whose cache folder differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Os {
    Windows,
    Mac,
    Other,
}

impl Os {
    fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::Mac
        } else {
            Self::Other
        }
    }
}

/// The folder holding saved indexes. `None` when the variables it comes from are missing: then
/// nothing is saved and the user is told nothing.
fn cache_dir(os: Os, env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let var = |name: &str| env(name).filter(|value| !value.is_empty());
    match os {
        Os::Windows => Some(
            PathBuf::from(var("LOCALAPPDATA")?)
                .join("DiskAtlas")
                .join("index"),
        ),
        Os::Mac => Some(
            PathBuf::from(var("HOME")?)
                .join("Library")
                .join("Caches")
                .join("DiskAtlas")
                .join("index"),
        ),
        Os::Other => {
            let xdg = var("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute());
            let base = match xdg {
                Some(xdg) => xdg,
                None => PathBuf::from(var("HOME")?).join(".cache"),
            };
            Some(base.join("disk-atlas").join("index"))
        }
    }
}

/// The saved index file of a volume: its id in lowercase hex, then `.idx`.
fn cache_file_name(id: &[u8; 16]) -> String {
    use std::fmt::Write as _;
    let mut name = String::with_capacity(36);
    for byte in id {
        let _ = write!(name, "{byte:02x}");
    }
    name.push_str(".idx");
    name
}

/// True for a name [`cache_file_name`] could have made.
fn is_cache_file_name(name: &str) -> bool {
    name.strip_suffix(".idx").is_some_and(|stem| {
        stem.len() == 32 && stem.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// Which saved indexes to delete. `files` are (path, length, modified) of what the cache folder
/// holds. Only cache file names are considered. The newest are kept while there are fewer than
/// [`MAX_SAVED`] and they fit [`MAX_SAVED_BYTES`] together; everything older goes. `keep`, the
/// file just saved, is never deleted and counts towards both limits.
fn prune_plan(files: Vec<(PathBuf, u64, SystemTime)>, keep: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = files
        .into_iter()
        .filter(|(path, ..)| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_cache_file_name)
        })
        .collect();
    files.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));

    let mut count = 0usize;
    let mut total = 0u64;
    if let Some((_, len, _)) = files.iter().find(|(path, ..)| path == keep) {
        count = 1;
        total = *len;
    }
    let mut full = false;
    let mut doomed = Vec::new();
    for (path, len, _) in files {
        if path == keep {
            continue;
        }
        if !full {
            let next = total.saturating_add(len);
            if count < MAX_SAVED && next <= MAX_SAVED_BYTES {
                count += 1;
                total = next;
                continue;
            }
            full = true;
        }
        doomed.push(path);
    }
    doomed
}

/// Loads the saved index at `path` if it is there and matches `expect`. Anything else gives
/// `None`: a missing file silently, an unreadable one with a debug line.
fn load_saved(path: &Path, expect: VolumeIdentity) -> Option<Index> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) => {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::debug!(%error, path = %path.display(), "a saved index could not be opened");
            }
            return None;
        }
    };
    let len = match file.metadata() {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            tracing::debug!(%error, path = %path.display(), "a saved index could not be measured");
            return None;
        }
    };
    match persist::load(&mut BufReader::new(file), len, expect) {
        Ok(index) => index,
        Err(error) => {
            tracing::debug!(%error, path = %path.display(), "a saved index was not usable");
            None
        }
    }
}

/// Writes `index` to `<dir>/<name>` through `<name>.tmp` and a rename, so a crash never leaves
/// half a file under the real name. Creates `dir`. This and [`prune`] are the only places the
/// app writes besides copying files out, and they stay inside the cache folder.
fn save_to_cache(dir: &Path, name: &str, index: &Index) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let target = dir.join(name);
    let temp = dir.join(format!("{name}.tmp"));
    let written = (|| {
        let mut out = BufWriter::new(File::create(&temp)?);
        persist::save(index, &mut out).map_err(|error| io::Error::other(error.to_string()))?;
        out.flush()?;
        // The file closes here, before the rename.
        drop(out);
        fs::rename(&temp, &target)
    })();
    if let Err(error) = written {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    Ok(target)
}

/// Deletes the saved indexes in `dir` that [`prune_plan`] names. Failures are logged.
fn prune(dir: &Path, keep: &Path) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(%error, "the index cache folder could not be listed");
            return;
        }
    };
    let files = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            metadata.is_file().then_some(())?;
            Some((entry.path(), metadata.len(), metadata.modified().ok()?))
        })
        .collect();
    for path in prune_plan(files, keep) {
        if let Err(error) = fs::remove_file(&path) {
            tracing::warn!(%error, path = %path.display(), "an old saved index could not be deleted");
        }
    }
}

/// Gets the index of a filesystem: from the cache when a volume has a current saved one,
/// otherwise by scanning it (then saving a volume's). `Ok(None)`: cancelled. Runs on the
/// background executor.
fn run_job(
    fs: &dyn FileSystem,
    volume: bool,
    cancel: &AtomicBool,
    progress: &AtomicU64,
) -> Result<Option<Index>, Error> {
    let identity = fs.identity().filter(|_| volume);
    let target = identity
        .zip(cache_dir(Os::current(), |name| std::env::var_os(name)))
        .map(|(identity, dir)| (identity, cache_file_name(&identity.id), dir));

    if let Some((identity, name, dir)) = &target
        && let Some(index) = load_saved(&dir.join(name), *identity)
    {
        return Ok(Some(index));
    }

    let built = build(fs, cancel, &mut |step| {
        progress.store(step.entries, Ordering::Relaxed);
    })?;
    let Some(index) = built else {
        return Ok(None);
    };

    if let Some((_, name, dir)) = target {
        match save_to_cache(&dir, &name, &index) {
            Ok(saved) => prune(&dir, &saved),
            Err(error) => tracing::warn!(%error, "the search index could not be saved"),
        }
    }
    Ok(Some(index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_atlas_lib::error::Result;
    use disk_atlas_lib::fs::native::NativeFs;
    use disk_atlas_lib::fs::{
        DirStream, EntryFlags, Metadata, NodeId, NodeKind, ScanEntry, ScanSummary, VolumeInfo,
    };
    use disk_atlas_lib::search::kind::KindClass;
    use std::ops::ControlFlow;

    fn env_of<'a>(pairs: &'a [(&'a str, OsString)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        }
    }

    /// An absolute path on the platform the tests run on.
    fn absolute(rest: &str) -> OsString {
        if cfg!(windows) {
            format!("C:\\{rest}").into()
        } else {
            format!("/{rest}").into()
        }
    }

    fn volume_source(n: u32) -> Source {
        use super::super::sources::{Availability, DiskId, SourceGroup};
        let fs = NativeFs::new(std::env::temp_dir(), "test").expect("temp dir opens");
        Source {
            id: SourceId::for_test(n),
            group: SourceGroup::Disks,
            label: "Disk".into(),
            format: None,
            fs: Arc::new(fs),
            kind: SourceKind::Volume {
                disk: DiskId::for_test(0),
                start: 0,
                len: 0,
            },
            availability: Availability::Browsable,
            system: None,
            role: None,
        }
    }

    #[test]
    fn a_volume_is_indexed_on_open_only_when_the_setting_is_on() {
        let source = volume_source(1);
        assert!(indexed_on_open(&source, true));
        // With it off, nothing is queued: the volume is indexed when it is searched.
        assert!(!indexed_on_open(&source, false));
    }

    #[test]
    fn cache_dir_windows() {
        let local = absolute("Users\\Me\\AppData\\Local");
        let expected = PathBuf::from(&local).join("DiskAtlas").join("index");
        let env = [("LOCALAPPDATA", local)];
        assert_eq!(cache_dir(Os::Windows, env_of(&env)), Some(expected));
        assert_eq!(cache_dir(Os::Windows, env_of(&[])), None);
        let empty = [("LOCALAPPDATA", OsString::new())];
        assert_eq!(cache_dir(Os::Windows, env_of(&empty)), None);
    }

    #[test]
    fn cache_dir_macos() {
        let home = absolute("Users/me");
        let expected = PathBuf::from(&home)
            .join("Library")
            .join("Caches")
            .join("DiskAtlas")
            .join("index");
        let env = [("HOME", home)];
        assert_eq!(cache_dir(Os::Mac, env_of(&env)), Some(expected));
        assert_eq!(cache_dir(Os::Mac, env_of(&[])), None);
        let empty = [("HOME", OsString::new())];
        assert_eq!(cache_dir(Os::Mac, env_of(&empty)), None);
    }

    #[test]
    fn cache_dir_other_prefers_an_absolute_xdg_cache_home() {
        let xdg = absolute("var/cache");
        let home = absolute("home/me");
        let both = [("XDG_CACHE_HOME", xdg.clone()), ("HOME", home.clone())];
        assert_eq!(
            cache_dir(Os::Other, env_of(&both)),
            Some(PathBuf::from(&xdg).join("disk-atlas").join("index"))
        );

        // Unset, empty or relative: fall back to the home folder.
        let fallback = Some(
            PathBuf::from(&home)
                .join(".cache")
                .join("disk-atlas")
                .join("index"),
        );
        let only_home = [("HOME", home.clone())];
        assert_eq!(cache_dir(Os::Other, env_of(&only_home)), fallback);
        let empty = [("XDG_CACHE_HOME", OsString::new()), ("HOME", home.clone())];
        assert_eq!(cache_dir(Os::Other, env_of(&empty)), fallback);
        let relative = [("XDG_CACHE_HOME", "cache".into()), ("HOME", home)];
        assert_eq!(cache_dir(Os::Other, env_of(&relative)), fallback);
    }

    #[test]
    fn cache_dir_other_without_a_home_is_none() {
        assert_eq!(cache_dir(Os::Other, env_of(&[])), None);
        let empty_home = [("HOME", OsString::new())];
        assert_eq!(cache_dir(Os::Other, env_of(&empty_home)), None);
        // An absolute XDG_CACHE_HOME needs no home.
        let xdg = [("XDG_CACHE_HOME", absolute("c"))];
        assert!(cache_dir(Os::Other, env_of(&xdg)).is_some());
    }

    #[test]
    fn cache_file_name_is_lowercase_hex() {
        let mut id = [0u8; 16];
        for (i, byte) in id.iter_mut().enumerate() {
            *byte = u8::try_from(i * 17).unwrap();
        }
        id[0] = 0xab;
        id[15] = 0x0f;
        let name = cache_file_name(&id);
        assert_eq!(name, "ab112233445566778899aabbccddee0f.idx");
        assert_eq!(name.len(), 36);
        assert!(is_cache_file_name(&name));
        assert_eq!(cache_file_name(&[0; 16]), format!("{}.idx", "0".repeat(32)));
    }

    #[test]
    fn cache_file_names_are_recognised_strictly() {
        let good = format!("{}.idx", "0123456789abcdef".repeat(2));
        assert!(is_cache_file_name(&good));
        assert!(!is_cache_file_name("notes.txt"));
        assert!(!is_cache_file_name(&format!("{}.tmp", "0".repeat(32))));
        assert!(!is_cache_file_name(&format!("{}.idx", "0".repeat(31))));
        assert!(!is_cache_file_name(&format!("{}.idx", "g".repeat(32))));
        assert!(!is_cache_file_name(&format!("{}.idx", "A".repeat(32))));
    }

    fn cache_path(n: u32) -> PathBuf {
        PathBuf::from("cache").join(format!("{n:032x}.idx"))
    }

    fn secs(n: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(n)
    }

    /// `count` files of one byte, file `n` modified at second `n`.
    fn small_files(count: u32) -> Vec<(PathBuf, u64, SystemTime)> {
        (0..count)
            .map(|n| (cache_path(n), 1, secs(u64::from(n))))
            .collect()
    }

    #[test]
    fn prune_keeps_everything_under_both_caps() {
        let files = small_files(5);
        assert!(prune_plan(files, &cache_path(0)).is_empty());
        let exactly = small_files(u32::try_from(MAX_SAVED).unwrap());
        assert!(prune_plan(exactly, &cache_path(0)).is_empty());
    }

    #[test]
    fn prune_keeps_the_32_newest_of_40() {
        let mut doomed = prune_plan(small_files(40), &cache_path(39));
        doomed.sort();
        let expected: Vec<PathBuf> = (0..8).map(cache_path).collect();
        assert_eq!(doomed, expected);
    }

    #[test]
    fn prune_deletes_the_oldest_beyond_the_size_cap() {
        let gib = 1u64 << 30;
        let files = vec![
            (cache_path(1), gib, secs(10)),
            (cache_path(2), gib, secs(20)),
            (cache_path(3), gib, secs(30)),
        ];
        // 2 GiB fits exactly; the oldest third does not.
        assert_eq!(prune_plan(files, &cache_path(3)), vec![cache_path(1)]);
    }

    #[test]
    fn prune_stops_keeping_once_a_file_does_not_fit() {
        let gib = 1u64 << 30;
        let files = vec![
            (cache_path(1), 1, secs(10)),
            (cache_path(2), 2 * gib, secs(20)),
            (cache_path(3), 1, secs(30)),
        ];
        // The big one pushes past the cap, so the small older one goes with it.
        let mut doomed = prune_plan(files, &cache_path(3));
        doomed.sort();
        assert_eq!(doomed, vec![cache_path(1), cache_path(2)]);
    }

    #[test]
    fn prune_never_deletes_the_file_just_saved() {
        let gib = 1u64 << 30;
        // Old and huge: still kept, and the rest has to fit around it.
        let files = vec![
            (cache_path(1), 3 * gib, secs(1)),
            (cache_path(2), 1, secs(20)),
            (cache_path(3), 1, secs(30)),
        ];
        let mut doomed = prune_plan(files, &cache_path(1));
        doomed.sort();
        assert_eq!(doomed, vec![cache_path(2), cache_path(3)]);

        // Old with many newer files: the keep file takes one of the 32 places.
        let doomed = prune_plan(small_files(40), &cache_path(0));
        assert!(!doomed.contains(&cache_path(0)));
        assert_eq!(doomed.len(), 8);
        assert!(doomed.contains(&cache_path(1)) && doomed.contains(&cache_path(8)));
        assert!(!doomed.contains(&cache_path(9)));
    }

    #[test]
    fn prune_ignores_foreign_names() {
        let mut files = small_files(34);
        files.push((PathBuf::from("cache").join("notes.txt"), 1 << 40, secs(0)));
        files.push((
            PathBuf::from("cache").join(format!("{}.idx.tmp", "0".repeat(32))),
            1,
            secs(0),
        ));
        let doomed = prune_plan(files, &cache_path(33));
        assert_eq!(doomed.len(), 2);
        assert!(
            doomed
                .iter()
                .all(|path| is_cache_file_name(path.file_name().and_then(|n| n.to_str()).unwrap()))
        );
    }

    fn id(n: u32) -> SourceId {
        SourceId::for_test(n)
    }

    #[test]
    fn queue_has_no_duplicates() {
        let mut queue = BuildQueue::default();
        queue.push(id(1));
        queue.push(id(2));
        queue.push(id(1));
        assert_eq!(queue.pop(), Some(id(1)));
        assert_eq!(queue.pop(), Some(id(2)));
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn queue_prioritize_moves_to_the_front_only_when_queued() {
        let mut queue = BuildQueue::default();
        for n in 1..=3 {
            queue.push(id(n));
        }
        queue.prioritize(id(3));
        queue.prioritize(id(9));
        assert_eq!(queue.pop(), Some(id(3)));
        assert_eq!(queue.pop(), Some(id(1)));
        assert_eq!(queue.pop(), Some(id(2)));
        // 9 was never queued and still is not.
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn queue_remove_takes_one_out() {
        let mut queue = BuildQueue::default();
        for n in 1..=3 {
            queue.push(id(n));
        }
        queue.remove(id(2));
        assert_eq!(queue.pop(), Some(id(1)));
        assert_eq!(queue.pop(), Some(id(3)));
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn only_filters_other_than_hidden_are_active() {
        assert!(!Filters::default().is_active());
        let hidden = Filters {
            include_hidden: true,
            ..Filters::default()
        };
        assert!(!hidden.is_active());
        let folder = Filters {
            this_folder: true,
            ..Filters::default()
        };
        assert!(folder.is_active());
        let kinds = Filters {
            kinds: KindSet::EMPTY.with(KindClass::Image),
            ..Filters::default()
        };
        assert!(kinds.is_active());
        let size = Filters {
            size: Some(1..=10),
            ..Filters::default()
        };
        assert!(size.is_active());
        let modified = Filters {
            modified: Some(0..=10),
            ..Filters::default()
        };
        assert!(modified.is_active());
    }

    #[test]
    fn the_query_carries_text_filters_and_scope() {
        let filters = Filters {
            this_folder: true,
            kinds: KindSet::EMPTY.with(KindClass::Document),
            size: Some(5..=50),
            modified: Some(-3..=7),
            include_hidden: true,
        };
        assert_eq!(
            make_query("report", &filters, Some(12)),
            Query {
                text: "report".into(),
                scope: Some(12),
                kinds: KindSet::EMPTY.with(KindClass::Document),
                size: Some(5..=50),
                modified: Some(-3..=7),
                include_hidden: true,
                include_system: false,
            }
        );
        // The scope comes from the caller, not from `this_folder`.
        assert_eq!(make_query("x", &filters, None).scope, None);
        assert_eq!(
            make_query("x", &Filters::default(), None),
            Query {
                text: "x".into(),
                ..Query::default()
            }
        );
    }

    /// A filesystem that only scans `entries` (node, parent node, name, kind), with a volume
    /// identity. The root is node 1.
    struct ScanFs {
        info: VolumeInfo,
        entries: Vec<(u64, u64, &'static str, NodeKind)>,
        identity: Option<VolumeIdentity>,
    }

    impl ScanFs {
        fn new(identity: Option<VolumeIdentity>) -> Self {
            // `VolumeInfo` is `#[non_exhaustive]`: borrow one from a real filesystem.
            let info = NativeFs::new(std::env::temp_dir(), "Volume")
                .expect("temp dir opens")
                .info();
            let dir = NodeKind::Directory;
            let file = NodeKind::File;
            Self {
                info,
                entries: vec![
                    (2, 1, "Photos", dir),
                    (3, 2, "holiday.jpg", file),
                    (4, 1, "notes.txt", file),
                ],
                identity,
            }
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
                let entry = ScanEntry {
                    node: NodeId(*node),
                    parent: NodeId(*parent),
                    name,
                    kind: *kind,
                    size: (*kind == NodeKind::File).then_some(10),
                    resource_size: None,
                    modified: None,
                    flags: EntryFlags::EMPTY,
                    finder_type: None,
                };
                if visit(entry).is_break() {
                    break;
                }
            }
            Ok(summary)
        }
        fn identity(&self) -> Option<VolumeIdentity> {
            self.identity
        }
    }

    fn identity(change: u8) -> VolumeIdentity {
        VolumeIdentity {
            id: [7; 16],
            change: [change; 16],
        }
    }

    fn built(fs: &ScanFs) -> Index {
        build(fs, &AtomicBool::new(false), &mut |_| {})
            .expect("scan succeeds")
            .expect("not cancelled")
    }

    /// A fresh folder under the system temp folder, removed when dropped.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "disk-atlas-search-test-{label}-{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A host folder `a/b/c.txt`, `a/x.txt`, `b/c.txt`, `n.txt` and the index of a scan of it.
    fn host_index(dir: &TempDir) -> Index {
        fs::create_dir_all(dir.0.join("a").join("b")).unwrap();
        fs::create_dir_all(dir.0.join("b")).unwrap();
        fs::write(dir.0.join("a").join("b").join("c.txt"), b"c").unwrap();
        fs::write(dir.0.join("a").join("x.txt"), b"x").unwrap();
        fs::write(dir.0.join("b").join("c.txt"), b"c").unwrap();
        fs::write(dir.0.join("n.txt"), b"n").unwrap();
        let host = NativeFs::new(dir.0.clone(), "Home").expect("folder opens");
        build(&host, &AtomicBool::new(false), &mut |_| {})
            .expect("scan succeeds")
            .expect("not cancelled")
    }

    #[test]
    fn a_host_folder_is_found_by_its_names() {
        let dir = TempDir::new("scope-names");
        let index = host_index(&dir);
        let path = |entry: u32| -> Vec<String> {
            index
                .crumbs(entry)
                .into_iter()
                .skip(1)
                .map(|(_, name)| name)
                .collect()
        };
        // Every entry is found by its own path, the nested `b` apart from the top one.
        for entry in 0..index.len() {
            assert_eq!(find_by_names(&index, &path(entry)), Some(entry));
        }
        let a_b = find_by_names(&index, &["a", "b"]).unwrap();
        let top_b = find_by_names(&index, &["b"]).unwrap();
        assert_ne!(a_b, top_b);
        assert_eq!(index.parent(a_b), find_by_names(&index, &["a"]));
        assert_eq!(index.parent(top_b), Some(0));
        // The root is the empty path.
        assert_eq!(find_by_names::<&str>(&index, &[]), Some(0));
        // Missing: no such name, the right name in the wrong folder, another case.
        assert_eq!(find_by_names(&index, &["nope"]), None);
        assert_eq!(find_by_names(&index, &["a", "nope"]), None);
        assert_eq!(find_by_names(&index, &["a", "c.txt"]), None);
        assert_eq!(find_by_names(&index, &["a", "b", "c.txt", "deeper"]), None);
        assert_eq!(find_by_names(&index, &["A"]), None);
    }

    #[test]
    fn a_scope_finds_its_folder_or_matches_nothing() {
        let dir = TempDir::new("scope-entry");
        let index = host_index(&dir);
        let names = |names: &[&str]| Scope::Names(names.iter().map(|n| (*n).to_owned()).collect());
        let a = names(&["a"]).entry(&index);
        assert_ne!(a, NO_SCOPE);
        assert_eq!(index.name(a), "a");
        assert_eq!(names(&[]).entry(&index), 0);
        // A folder that is not in the index: no hits, as before.
        assert_eq!(names(&["gone"]).entry(&index), NO_SCOPE);
        // A volume's folder is found by its id; on a host index a real id is not there.
        assert_eq!(Scope::Node(NodeId(2)).entry(&index), NO_SCOPE);
        let entry = index.entry(a).unwrap().node;
        assert_eq!(Scope::Node(entry).entry(&index), a);
        // The scope narrows a search to the folder's subtree.
        let query = make_query("c.txt", &Filters::default(), Some(a));
        let hits = index
            .search(&query, None, &AtomicBool::new(false))
            .expect("not cancelled");
        assert_eq!(hits.len(), 1);
        let whole = make_query("c.txt", &Filters::default(), None);
        let hits = index
            .search(&whole, None, &AtomicBool::new(false))
            .expect("not cancelled");
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn a_saved_index_loads_back_and_a_stale_one_does_not() {
        let dir = TempDir::new("round-trip");
        // The folder does not exist yet: saving creates it.
        let cache = dir.0.join("nested").join("index");
        let index = built(&ScanFs::new(Some(identity(1))));
        let name = cache_file_name(&identity(1).id);

        let saved = save_to_cache(&cache, &name, &index).expect("saves");
        assert_eq!(saved, cache.join(&name));
        assert!(!cache.join(format!("{name}.tmp")).exists());

        let loaded = load_saved(&saved, identity(1)).expect("loads");
        assert_eq!(loaded.len(), index.len());
        for i in 0..index.len() {
            assert_eq!(loaded.name(i), index.name(i));
        }

        // The volume changed since: the file is stale.
        assert!(load_saved(&saved, identity(2)).is_none());
        // A different volume.
        let other = VolumeIdentity {
            id: [8; 16],
            ..identity(1)
        };
        assert!(load_saved(&saved, other).is_none());
        // No file at all.
        assert!(load_saved(&cache.join("missing.idx"), identity(1)).is_none());
    }

    #[test]
    fn saving_an_index_without_an_identity_fails_and_leaves_nothing() {
        let dir = TempDir::new("no-identity");
        let index = built(&ScanFs::new(None));
        assert!(save_to_cache(&dir.0, "x.idx", &index).is_err());
        assert!(!dir.0.join("x.idx").exists());
        assert!(!dir.0.join("x.idx.tmp").exists());
    }

    #[test]
    fn saving_replaces_an_older_file() {
        let dir = TempDir::new("replace");
        let name = cache_file_name(&identity(1).id);
        let first = built(&ScanFs::new(Some(identity(1))));
        save_to_cache(&dir.0, &name, &first).expect("saves");
        let second = built(&ScanFs::new(Some(identity(2))));
        let saved = save_to_cache(&dir.0, &name, &second).expect("saves again");
        assert!(load_saved(&saved, identity(2)).is_some());
        assert!(load_saved(&saved, identity(1)).is_none());
    }

    #[test]
    fn pruning_deletes_what_the_plan_names_and_spares_the_rest() {
        let dir = TempDir::new("prune");
        fs::create_dir_all(&dir.0).unwrap();
        let keep = dir.0.join(cache_file_name(&[1; 16]));
        let foreign = dir.0.join("notes.txt");
        fs::write(&keep, b"k").unwrap();
        fs::write(&foreign, b"f").unwrap();
        // 33 more saved indexes; with `keep`, two more than the limit allows.
        for n in 0..33u8 {
            fs::write(dir.0.join(cache_file_name(&[100 + n; 16])), b"x").unwrap();
        }
        prune(&dir.0, &keep);
        let remaining = fs::read_dir(&dir.0)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| is_cache_file_name(entry.file_name().to_str().unwrap_or("")))
            .count();
        assert_eq!(remaining, MAX_SAVED);
        assert!(keep.exists() && foreign.exists());
    }
}
