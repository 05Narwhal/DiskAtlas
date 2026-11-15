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

//! Folder sizes in the app (design `r10-everyday.md` §3.4).
//!
//! Two halves. For a volume Disk Atlas reads, [`Sizes`] turns a finished search index into an
//! [`IndexSizes`] table on the background executor and hands it to the [`Listing`], which resolves
//! each folder row once when it loads (see [`resolve_page`]). That table never changes, so a row
//! keeps its [`Totals`] and nothing is searched while painting.
//!
//! For the host (this computer's own drives, R14.3) [`Sizes`] keeps one [`HostSizes`] tree per
//! source, created on the first folder shown, and a [`Watcher`] while the setting `watch_changes`
//! is on. A folder row keeps a [`HostDir`] handle ([`RowSize::Live`]) and reads its state through
//! it when drawn. The engine calls back from its own threads; those callbacks only push onto a
//! [`Signal`], and one foreground task drains it at most every [`SIZE_REPAINT`]. Everything that
//! reads a directory (`invalidate`, `request`, creating the tree or the watcher) runs on the
//! background executor, never on the UI thread.
//!
//! The pure parts, [`RowSize`], [`ResortGuard`], [`Stamp`], [`Validated`] and [`plan`], carry no
//! GPUI types.

use std::collections::{HashMap, HashSet};
use std::future::poll_fn;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Poll, Waker};
use std::time::{Duration, Instant, SystemTime};

use disk_atlas_lib::error::Error;
use disk_atlas_lib::fs::{DirBatch, FileSystem, NodeId, VolumeIdentity};
use disk_atlas_lib::search::Index;
use disk_atlas_lib::sizes::watch::{
    MAX_BATCH_DIRS, MAX_LINUX_WATCHES, REVALIDATE_AFTER, WatchBatch, Watcher,
};
use disk_atlas_lib::sizes::{
    ClassBytes, HostDir, HostSizes, IndexSizes, Priority, SizeState as HostState, Totals,
    is_network_drive,
};
use gpui::{App, Context, Entity, SharedString, Subscription, Task};

use super::listing::Listing;
use super::loader::Rows;
use super::search::{Search, SearchEvent};
use super::settings::Settings;
use super::sources::{SourceGroup, SourceId, SourceKind, Sources, SourcesEvent};

/// A folder's rows are re-sorted by size at most this often while totals arrive. A table of an
/// index arrives once, so this only spaces that one re-sort from the one before it; the host's
/// live tables use it for every re-sort.
pub const RESORT_INTERVAL: Duration = Duration::from_secs(1);

/// The Size column repaints at most this often while totals arrive (`r10-everyday.md` §3.4).
pub const SIZE_REPAINT: Duration = Duration::from_millis(250);

/// A listed host folder is read again this long after the watcher reported it changed, so a burst
/// of changes becomes one refresh.
pub const LIST_REFRESH: Duration = Duration::from_millis(200);

/// Folder rows that stay still this long after scrolling are measured next (`Priority::Visible`).
pub const VISIBLE_SETTLE: Duration = Duration::from_millis(60);

/// The most folder rows one `Visible` request names: a screenful and then some.
pub const VISIBLE_MAX: usize = 256;

/// No re-sort within this long of a key press or a mouse press: the rows under the user's hand
/// stay where they are.
pub const KEY_GUARD: Duration = Duration::from_millis(500);

/// The tooltip of a lower-bound folder size.
pub const LOWER_BOUND_TIP: &str =
    "Part of this volume could not be read, so sizes are a lower bound.";

/// What a folder row keeps. Resolved once, when the listing loads or refreshes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RowSize {
    /// A file, or a place where sizes are off or not built yet.
    #[default]
    None,
    /// A volume Disk Atlas reads: final. The flag is true for a lower bound.
    Fixed(Totals, bool),
    /// The host: a handle into the source's [`HostSizes`] tree, read through when the row is
    /// drawn ([`Rows::size_state`]).
    Live(SourceId, HostDir),
}

/// How the Size column shows a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeState {
    /// The total, drawn like a file's size.
    Exact(u64),
    /// The total so far, drawn in the quiet ink with "…" after it.
    Running(u64),
    /// "over 3.1 GB", with [`LOWER_BOUND_TIP`].
    AtLeast(u64),
    /// Nothing is shown.
    Unknown,
}

impl SizeState {
    /// How the engine's state of a host folder reads in the Size column.
    pub fn from_host(state: HostState) -> Self {
        match state {
            HostState::Unknown => Self::Unknown,
            HostState::Running(totals) => Self::Running(totals.bytes),
            HostState::Exact(totals) => Self::Exact(totals.bytes),
            HostState::AtLeast(totals) => Self::AtLeast(totals.bytes),
        }
    }

    /// The bytes the state stands for, when there are any (to order by).
    pub fn bytes(self) -> Option<u64> {
        match self {
            Self::Exact(bytes) | Self::Running(bytes) | Self::AtLeast(bytes) => Some(bytes),
            Self::Unknown => None,
        }
    }
}

impl RowSize {
    /// O(1), for drawing. A [`RowSize::Live`] needs its tree: it reads [`SizeState::Unknown`]
    /// here, and [`Rows::size_state`] reads it through the handle.
    pub fn state(self) -> SizeState {
        match self {
            Self::None | Self::Live(..) => SizeState::Unknown,
            Self::Fixed(totals, false) => SizeState::Exact(totals.bytes),
            Self::Fixed(totals, true) => SizeState::AtLeast(totals.bytes),
        }
    }

    /// The total bytes of a volume Disk Atlas reads, when there is a total.
    pub fn bytes(self) -> Option<u64> {
        match self {
            Self::None | Self::Live(..) => None,
            Self::Fixed(totals, _) => Some(totals.bytes),
        }
    }
}

/// The handles of the folder rows of one listed host folder, with the tree they read.
pub struct LiveSizes {
    source: SourceId,
    sizes: Arc<HostSizes>,
    /// The listed folder itself, when the tree knew it.
    folder: Option<HostDir>,
    /// (arrival index, handle) of the folder rows the tree knew, arrivals ascending.
    dirs: Vec<(u32, HostDir)>,
}

impl LiveSizes {
    /// The handle of the row `arrival`, as a [`RowSize`].
    pub fn row(&self, arrival: u32) -> Option<RowSize> {
        let at = self.dirs.binary_search_by_key(&arrival, |(a, _)| *a).ok()?;
        self.dirs
            .get(at)
            .map(|(_, dir)| RowSize::Live(self.source, *dir))
    }

    /// How many folder rows have a handle.
    pub fn resolved(&self) -> usize {
        self.dirs.len()
    }

    /// The state behind a handle of this tree. O(1) (one short lock).
    pub fn state(&self, source: SourceId, dir: HostDir) -> SizeState {
        if source == self.source {
            SizeState::from_host(self.sizes.state_of(dir))
        } else {
            SizeState::Unknown
        }
    }

    /// True while the listed folder itself is still being measured.
    pub fn folder_running(&self) -> bool {
        self.folder
            .is_some_and(|dir| matches!(self.sizes.state_of(dir), HostState::Running(_)))
    }
}

/// What [`resolve_live`] found.
pub struct LiveResolved {
    /// The handles.
    pub live: LiveSizes,
    /// Folder rows the tree did not know yet.
    pub unresolved: usize,
    /// True when the listed folder is measured (exact or lower bound): every folder row it holds
    /// that is going to be in the tree is.
    pub folder_done: bool,
}

impl LiveResolved {
    /// True when a later pass may find more: some rows were not in the tree, and the listed
    /// folder is not finished (so they may still arrive).
    pub fn pending(&self) -> bool {
        self.unresolved > 0 && !self.folder_done
    }
}

/// Finds each folder row of `rows` (the children of `folder`) in `sizes`. One path join and one
/// lookup per folder row, under the tree's lock for a moment each: background executor only.
pub fn resolve_live(
    sizes: &Arc<HostSizes>,
    source: SourceId,
    folder: &Path,
    rows: &Rows,
) -> LiveResolved {
    let mut dirs = Vec::new();
    let mut unresolved = 0usize;
    let mut path = folder.to_path_buf();
    for (arrival, entry) in rows.iter() {
        if !entry.is_browsable() {
            continue;
        }
        path.push(entry.name);
        match sizes.dir(&path) {
            Some(dir) => dirs.push((arrival, dir)),
            None => unresolved += 1,
        }
        path.pop();
    }
    LiveResolved {
        live: LiveSizes {
            source,
            sizes: Arc::clone(sizes),
            folder: sizes.dir(folder),
            dirs,
        },
        unresolved,
        folder_done: matches!(
            sizes.state(folder),
            HostState::Exact(_) | HostState::AtLeast(_)
        ),
    }
}

/// The folder rows of a page that have a total: (arrival index, totals), arrivals ascending.
/// `base` is the arrival index of the page's first entry. Each folder is found by its node
/// through [`IndexSizes::folder_entry`], never by an entry number of a link. A folder the table
/// does not know is left out.
///
/// Blocking only in the sense of CPU: O(folders log folders). Background executor.
pub fn resolve_page(table: &IndexSizes, base: u32, page: &DirBatch) -> Vec<(u32, Totals)> {
    let mut out = Vec::new();
    for (arrival, entry) in (base..).zip(page.iter()) {
        if !entry.is_browsable() {
            continue;
        }
        if let Some(totals) = table
            .folder_entry(entry.node)
            .and_then(|folder| table.totals(folder))
        {
            out.push((arrival, totals));
        }
    }
    out
}

/// When a size re-sort may run: not within [`KEY_GUARD`] of a key or mouse press, and not within
/// [`RESORT_INTERVAL`] of the last re-sort.
#[derive(Debug, Default, Clone, Copy)]
pub struct ResortGuard {
    last_input: Option<Instant>,
    last_resort: Option<Instant>,
}

impl ResortGuard {
    /// A key or the mouse was pressed at `now`.
    pub fn input(&mut self, now: Instant) {
        self.last_input = Some(now);
    }

    /// A re-sort started at `now`.
    pub fn resorted(&mut self, now: Instant) {
        self.last_resort = Some(now);
    }

    /// How long to wait before a re-sort may run; `None` when it may run now.
    pub fn wait(&self, now: Instant) -> Option<Duration> {
        let left = |since: Option<Instant>, span: Duration| {
            since
                .map(|at| span.saturating_sub(now.saturating_duration_since(at)))
                .filter(|left| !left.is_zero())
        };
        match (
            left(self.last_input, KEY_GUARD),
            left(self.last_resort, RESORT_INTERVAL),
        ) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }
}

/// What a table was built from, to tell a volume that changed under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    /// `VolumeIdentity::change`; `None` when the format has no identity.
    change: Option<[u8; 16]>,
    /// For an image: the file's length and modified time.
    image: Option<(u64, Option<SystemTime>)>,
}

impl Stamp {
    /// Reads the stamp. Does file system calls: background executor only.
    pub fn read(fs: &dyn FileSystem, image: Option<&PathBuf>) -> Self {
        Self {
            change: fs.identity().map(|VolumeIdentity { change, .. }| change),
            image: image
                .and_then(|path| std::fs::metadata(path).ok())
                .map(|meta| (meta.len(), meta.modified().ok())),
        }
    }

    /// True when `now` differs from `self`. A part that could not be read now (or then) is not a
    /// difference: the volume is taken as unchanged.
    pub fn differs(&self, now: &Self) -> bool {
        let change = matches!((self.change, now.change), (Some(a), Some(b)) if a != b);
        let image = matches!((self.image, now.image), (Some(a), Some(b)) if a != b);
        change || image
    }
}

/// A table with what it was built from.
struct Built {
    /// The table itself, for the Properties window's "What is inside".
    table: Arc<IndexSizes>,
    stamp: Stamp,
    image: Option<PathBuf>,
    /// The volume's bytes per kind, for the storage bar.
    classes: ClassBytes,
    /// What the bar multiplies the kinds by to fit the used space: 1.0 unless the files claim
    /// more than the volume has.
    scale: f32,
    /// Some sizes are a lower bound.
    lower_bound: bool,
}

/// What the storage bar multiplies `classes` by: `used / sum` when the files claim more than the
/// volume's used space, else 1.0.
fn fit_scale(classes: &ClassBytes, used: Option<u64>, overcommitted: bool) -> f32 {
    let sum = classes.total();
    match used {
        Some(used) if overcommitted && sum > used && sum > 0 => {
            (used as f64 / sum as f64).clamp(0.0, 1.0) as f32
        }
        _ => 1.0,
    }
}

/// The setting `folder_sizes`: what is measured on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The folder you are looking at, visible rows first (the default).
    OpenFolders,
    /// A whole drive, as soon as one of its folders is shown.
    WholeDrives,
    /// Nothing.
    Off,
}

impl Mode {
    fn of(word: &str) -> Self {
        match word {
            "whole_drives" => Self::WholeDrives,
            "off" => Self::Off,
            _ => Self::OpenFolders,
        }
    }

    /// The mode in the app's settings; the default where there are none (tests).
    pub fn current(cx: &App) -> Self {
        Switches::current(cx).mode
    }
}

/// The three settings the host sizes follow, as last seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Switches {
    mode: Mode,
    /// `folder_sizes_network`.
    network: bool,
    /// `watch_changes`.
    watch: bool,
}

impl Switches {
    fn of(settings: &Settings) -> Self {
        Self {
            mode: Mode::of(settings.choice_of("folder_sizes")),
            network: settings.bool_of("folder_sizes_network"),
            watch: settings.bool_of("watch_changes"),
        }
    }

    fn current(cx: &App) -> Self {
        cx.try_global::<Settings>().map_or(
            Self {
                mode: Mode::OpenFolders,
                network: false,
                watch: true,
            },
            Self::of,
        )
    }
}

/// When each folder of a host source was last shown or reported changed, to apply
/// [`REVALIDATE_AFTER`]: `notify` never reports an overflowed change buffer on Windows, a Linux
/// watcher covers only so many folders, and a drive may not be watched at all, so numbers older
/// than that are measured again when their folder is shown.
#[derive(Debug)]
pub struct Validated {
    /// When the tree was created: the stamp of a folder never seen since.
    since: Instant,
    seen: HashMap<PathBuf, Instant>,
}

/// Most stamps kept; the map is emptied when it is full (the stamps fall back to `since`).
const VALIDATED_CAP: usize = 4096;

impl Validated {
    /// A tree created at `now`.
    pub fn new(now: Instant) -> Self {
        Self {
            since: now,
            seen: HashMap::new(),
        }
    }

    /// `dir` was shown (or reported changed) at `now`. True when its numbers were last validated
    /// more than [`REVALIDATE_AFTER`] ago: it is to be read again.
    pub fn touch(&mut self, dir: &Path, now: Instant) -> bool {
        let last = self.seen.get(dir).copied().unwrap_or(self.since);
        let stale = now.saturating_duration_since(last) > REVALIDATE_AFTER;
        self.mark(dir, now);
        stale
    }

    /// The whole tree was queued to be measured again at `now`: every stamp is `now`.
    pub fn mark_all(&mut self, now: Instant) {
        self.since = now;
        self.seen.clear();
    }

    /// `dir` was validated at `now` (the watcher reported it, and it was read again).
    pub fn mark(&mut self, dir: &Path, now: Instant) {
        if self.seen.len() >= VALIDATED_CAP && !self.seen.contains_key(dir) {
            self.seen.clear();
        }
        self.seen.insert(dir.to_path_buf(), now);
    }
}

/// What showing a host folder asks of its tree.
#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    /// Read the folder's entries again first (its numbers are old).
    pub invalidate: bool,
    /// Folders to queue, in this order.
    pub requests: Vec<(PathBuf, Priority)>,
    /// Drop the unfinished background work outside these folders. `None`: keep it all.
    pub refocus: Option<Vec<PathBuf>>,
}

/// What showing `dir` on the drive `root` asks of the tree under `mode` (`r10-everyday.md` §3.2).
/// `stale` is [`Validated::touch`]'s answer.
pub fn plan(mode: Mode, root: &Path, dir: &Path, stale: bool) -> Plan {
    match mode {
        Mode::Off => Plan {
            invalidate: false,
            requests: Vec::new(),
            refocus: None,
        },
        Mode::OpenFolders => Plan {
            invalidate: stale,
            requests: vec![(dir.to_path_buf(), Priority::Listed)],
            refocus: Some(vec![dir.to_path_buf()]),
        },
        // The drive is queued first, then the shown folder is raised above it so it finishes
        // first; nothing is dropped.
        Mode::WholeDrives => Plan {
            invalidate: stale,
            requests: vec![
                (root.to_path_buf(), Priority::Background),
                (dir.to_path_buf(), Priority::Listed),
            ],
            refocus: None,
        },
    }
}

/// The folders a Linux watcher should cover, nearest the open folder first: the folder, the
/// folders above it, then the sub-folders the tree knows. Other systems watch a whole tree with
/// one handle and ignore the list.
fn watch_focus(sizes: &HostSizes, root: &Path, dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![dir.to_path_buf()];
    let mut up = dir.parent();
    while let Some(parent) = up {
        if parent != root && parent.starts_with(root) {
            out.push(parent.to_path_buf());
        }
        up = parent.parent();
    }
    for (name, _) in sizes.children(dir) {
        if out.len() >= MAX_LINUX_WATCHES {
            break;
        }
        out.push(dir.join(name));
    }
    out
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What one pass of the batch thread does for a source (`r10-everyday.md` §3.3, "Batches never
/// pile up").
#[derive(Debug, PartialEq, Eq)]
pub enum Apply {
    /// Read these folders again, the one a window lists first.
    Dirs(Vec<PathBuf>),
    /// Everything may have changed: `invalidate_all` once, no folder read.
    All,
}

/// Merges every batch that was waiting into one pass. Any `Rescan`, or more than
/// [`MAX_BATCH_DIRS`] different folders, makes it [`Apply::All`]; otherwise the folders are
/// collapsed to one set in first-seen order with `listed` (the folder a window lists) moved to
/// the front. `None` when there is nothing to do. So a pass reads at most `MAX_BATCH_DIRS`
/// folders, and a long burst ends in one background re-measure, never a growing queue.
pub fn merge_batches(waiting: Vec<WatchBatch>, listed: Option<&Path>) -> Option<Apply> {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    for batch in waiting {
        match batch {
            WatchBatch::Rescan => return Some(Apply::All),
            WatchBatch::Dirs(batch_dirs) => {
                for dir in batch_dirs {
                    if seen.contains(&dir) {
                        continue;
                    }
                    if dirs.len() >= MAX_BATCH_DIRS {
                        return Some(Apply::All);
                    }
                    seen.insert(dir.clone());
                    dirs.push(dir);
                }
            }
        }
    }
    if dirs.is_empty() {
        return None;
    }
    if let Some(at) = listed.and_then(|listed| dirs.iter().position(|dir| dir == listed)) {
        dirs[..=at].rotate_right(1);
    }
    Some(Apply::Dirs(dirs))
}

/// Where a watcher's batches go: the tree to correct and what to tell the foreground.
struct Route {
    source: SourceId,
    root: PathBuf,
    sizes: Arc<HostSizes>,
    validated: Arc<Mutex<Validated>>,
}

/// The batch thread's input: a batch and the source it came from.
type BatchMessage = (Arc<Route>, WatchBatch);

/// The one thread that applies watcher batches (`invalidate` reads a folder on the calling
/// thread, so it must never be the UI thread). It takes every batch waiting at once, merges them
/// per source ([`merge_batches`]), corrects the tree, and tells the foreground when the listed
/// folder changed. It ends when every sender is gone.
fn apply_batches(
    batches: &std::sync::mpsc::Receiver<BatchMessage>,
    listed: &Mutex<Option<PathBuf>>,
    signal: &Signal,
) {
    while let Ok(first) = batches.recv() {
        let mut waiting = vec![first];
        waiting.extend(batches.try_iter());
        let mut routes: Vec<(Arc<Route>, Vec<WatchBatch>)> = Vec::new();
        for (route, batch) in waiting {
            match routes.iter_mut().find(|(r, _)| r.source == route.source) {
                Some((_, group)) => group.push(batch),
                None => routes.push((route, vec![batch])),
            }
        }
        let listed = lock(listed).clone();
        for (route, group) in routes {
            let here = listed.as_deref().filter(|dir| dir.starts_with(&route.root));
            // One bad folder must not end the thread.
            let applied = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                apply_one(&route, merge_batches(group, here), here, signal);
            }));
            if applied.is_err() {
                tracing::warn!("applying a watcher batch panicked");
            }
        }
    }
}

/// Applies one source's merged pass. The listed folder is read first, and the foreground hears of
/// it before the rest is read.
fn apply_one(route: &Route, apply: Option<Apply>, listed: Option<&Path>, signal: &Signal) {
    match apply {
        None => {}
        Some(Apply::All) => {
            lock(&route.validated).mark_all(Instant::now());
            route.sizes.invalidate_all();
            if listed.is_some() {
                signal.refresh(route.source);
            }
        }
        Some(Apply::Dirs(dirs)) => {
            let now = Instant::now();
            {
                let mut validated = lock(&route.validated);
                for dir in &dirs {
                    validated.mark(dir, now);
                }
            }
            let hit = listed.is_some_and(|listed| dirs.first().is_some_and(|d| d == listed));
            for (n, dir) in dirs.iter().enumerate() {
                route.sizes.invalidate(dir);
                if hit && n == 0 {
                    signal.refresh(route.source);
                }
            }
        }
    }
}

#[derive(Default)]
struct Pending {
    changed: HashSet<SourceId>,
    /// Sources whose listed folder was reported changed.
    refresh: HashSet<SourceId>,
    waker: Option<Waker>,
}

/// What the foreground task takes from a [`Signal`].
struct Taken {
    changed: HashSet<SourceId>,
    refresh: HashSet<SourceId>,
}

/// The engine's callbacks (`HostSizes::on_change`) and the batch thread run off the UI thread.
/// They push here, and one foreground task waits on it. There is no polling: a push wakes the
/// task. Nothing here blocks beyond a short lock.
#[derive(Default)]
pub struct Signal {
    pending: Mutex<Pending>,
}

impl Signal {
    fn push(&self, f: impl FnOnce(&mut Pending)) {
        let waker = {
            let mut pending = lock(&self.pending);
            f(&mut pending);
            pending.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Totals changed in the tree of `source`.
    fn changed(&self, source: SourceId) {
        self.push(|pending| {
            pending.changed.insert(source);
        });
    }

    /// The folder listed from `source` was reported changed (and read again in its tree).
    fn refresh(&self, source: SourceId) {
        self.push(|pending| {
            pending.refresh.insert(source);
        });
    }

    /// Resolves with what has been pushed, as soon as anything has.
    async fn wait(&self) -> Taken {
        poll_fn(|cx| {
            let mut pending = lock(&self.pending);
            if pending.changed.is_empty() && pending.refresh.is_empty() {
                pending.waker = Some(cx.waker().clone());
                Poll::Pending
            } else {
                Poll::Ready(Taken {
                    changed: std::mem::take(&mut pending.changed),
                    refresh: std::mem::take(&mut pending.refresh),
                })
            }
        })
        .await
    }
}

/// How a host source's watcher stands.
enum WatcherSlot {
    /// Not wanted (`watch_changes` is off).
    Off,
    /// Being created on the background executor; the number says which request.
    Starting(u64),
    /// Running.
    On(Arc<Watcher>),
    /// It could not start: logged once, sizes still work.
    Failed,
}

/// The tree of one host source and what goes with it.
struct HostTable {
    sizes: Arc<HostSizes>,
    root: PathBuf,
    watcher: WatcherSlot,
    /// Shared with the batch thread, which marks what the watcher reported.
    validated: Arc<Mutex<Validated>>,
    /// Serialises the jobs that queue work in the tree, so a slow one cannot land after a newer.
    gate: Arc<Mutex<()>>,
    /// The number of the newest such job: an older one skips its `request` and `refocus`.
    latest: Arc<AtomicU64>,
}

enum HostSlot {
    /// The tree is being created on the background executor.
    Creating,
    /// A network drive (or one whose type cannot be told) while `folder_sizes_network` is off:
    /// nothing is measured, nothing is watched.
    Skipped,
    Ready(HostTable),
}

/// Hands a table's parts to the background executor to stop (they join threads).
fn retire(sizes: Arc<HostSizes>, watcher: Option<Arc<Watcher>>, cx: &mut Context<Sizes>) {
    cx.background_executor()
        .spawn(async move {
            drop(watcher);
            sizes.shutdown();
        })
        .detach();
}

/// A table build in flight; dropping it stops it.
struct Job {
    cancel: Arc<AtomicBool>,
    _task: Task<()>,
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// The size tables of the sources whose index is built, the trees of the host sources, and the
/// volume check.
pub struct Sizes {
    sources: Entity<Sources>,
    search: Entity<Search>,
    listing: Entity<Listing>,
    tables: HashMap<SourceId, Built>,
    jobs: HashMap<SourceId, Job>,
    check: Option<Task<()>>,
    /// The tree and watcher of each host source that has shown a folder.
    hosts: HashMap<SourceId, HostSlot>,
    /// The folder the listing shows, for the watcher's `wanted`: it asks from its own thread, so
    /// this is a plain shared value, locked for a moment.
    listed: Arc<Mutex<Option<PathBuf>>>,
    /// What the listing showed when it last changed: (source, folder, load number).
    seen: Option<(SourceId, NodeId, u64)>,
    /// The host folder shown now.
    shown: Option<(SourceId, PathBuf)>,
    /// The settings the host sizes follow, as last seen.
    switches: Switches,
    /// True while a listing refresh waits out [`LIST_REFRESH`].
    refresh_pending: bool,
    /// Numbers the watcher starts, so an answer for one that was replaced is dropped.
    watcher_serial: u64,
    signal: Arc<Signal>,
    /// The watchers' sinks send here; one thread applies what arrives.
    batches: std::sync::mpsc::Sender<BatchMessage>,
    _drain: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl Sizes {
    /// Follows `search` for finished indexes, `sources` for sources that went away, `listing` for
    /// the host folder shown, and the settings for `folder_sizes` and `watch_changes`.
    pub fn new(
        sources: Entity<Sources>,
        search: Entity<Search>,
        listing: Entity<Listing>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.subscribe(&search, |this, _search, event: &SearchEvent, cx| {
                if let SearchEvent::IndexReady { source, index } = event {
                    this.index_ready(*source, Arc::clone(index), cx);
                }
            }),
            cx.subscribe(&sources, |this, _sources, event: &SourcesEvent, cx| {
                if matches!(event, SourcesEvent::Changed) {
                    this.forget_gone(cx);
                }
            }),
            cx.observe(&listing, |this, _listing, cx| this.listing_changed(cx)),
            cx.observe_global::<Settings>(|this, cx| this.settings_changed(cx)),
        ];
        let signal = Arc::new(Signal::default());
        let waiting = Arc::clone(&signal);
        // One foreground task takes what the engine's threads push, and repaints at most every
        // `SIZE_REPAINT`.
        let drain = cx.spawn(async move |this, cx| {
            loop {
                let taken = waiting.wait().await;
                if this.update(cx, |this, cx| this.drain(taken, cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(SIZE_REPAINT).await;
            }
        });
        // The one thread that applies watcher batches. It ends when the last sender is dropped
        // (the app's, and one in each running watcher's sink).
        let listed = Arc::new(Mutex::new(None));
        let (batches, inbox) = std::sync::mpsc::channel::<BatchMessage>();
        let (for_thread, wake) = (Arc::clone(&listed), Arc::clone(&signal));
        if let Err(error) = std::thread::Builder::new()
            .name("disk-atlas-batches".into())
            .spawn(move || apply_batches(&inbox, &for_thread, &wake))
        {
            tracing::warn!(%error, "no thread for watcher batches: changes are not followed");
        }
        Self {
            sources,
            search,
            listing,
            tables: HashMap::new(),
            jobs: HashMap::new(),
            check: None,
            hosts: HashMap::new(),
            listed,
            seen: None,
            shown: None,
            switches: Switches::current(cx),
            refresh_pending: false,
            watcher_serial: 0,
            signal,
            batches,
            _drain: drain,
            _subscriptions: subscriptions,
        }
    }

    /// The listing changed: when it shows another host folder, or loaded it again, the folder is
    /// asked of its tree (`folder_shown`). Cheap for every other change.
    fn listing_changed(&mut self, cx: &mut Context<Self>) {
        let key = self.listing.read(cx).host_view_key();
        if key == self.seen {
            return;
        }
        self.seen = key;
        let folder = key.and_then(|(source, ..)| {
            self.listing
                .read(cx)
                .host_folder_path(cx)
                .map(|path| (source, path))
        });
        match folder {
            Some((source, path)) => self.folder_shown(source, path, cx),
            None => {
                self.shown = None;
                *lock(&self.listed) = None;
            }
        }
    }

    /// From the listing, when a host folder is shown: creates the tree (and the watcher) on first
    /// use, queues the folder as the setting `folder_sizes` says, and drops the unfinished work of
    /// the folder that was left. A folder whose numbers are older than [`REVALIDATE_AFTER`] is
    /// read again first. Nothing here touches a disk on the calling thread.
    fn folder_shown(&mut self, source: SourceId, dir: PathBuf, cx: &mut Context<Self>) {
        *lock(&self.listed) = Some(dir.clone());
        self.shown = Some((source, dir.clone()));
        match self.hosts.get(&source) {
            None => self.create_host(source, cx),
            Some(HostSlot::Ready(_)) => self.measure(source, dir, cx),
            // Creating: `host_created` measures the folder shown by then. Skipped: no sizes.
            Some(HostSlot::Creating | HostSlot::Skipped) => {}
        }
    }

    /// Creates the tree of `source` on the background executor: it asks the drive's type, which
    /// reads the system, and the engine reads the root's device.
    fn create_host(&mut self, source: SourceId, cx: &mut Context<Self>) {
        let Some(root) = self
            .sources
            .read(cx)
            .host_root(source)
            .map(Path::to_path_buf)
        else {
            return;
        };
        self.hosts.insert(source, HostSlot::Creating);
        let network_ok = self.switches.network;
        let signal = Arc::clone(&self.signal);
        cx.spawn(async move |this, cx| {
            let place = root.clone();
            let made = cx
                .background_executor()
                .spawn(async move {
                    if !network_ok && is_network_drive(&place) {
                        return None;
                    }
                    Some(Arc::new(HostSizes::new(
                        place,
                        Box::new(move || signal.changed(source)),
                    )))
                })
                .await;
            this.update(cx, |this, cx| this.host_created(source, root, made, cx))
                .ok();
        })
        .detach();
    }

    /// The background executor made (or refused to make) the tree of `source`.
    fn host_created(
        &mut self,
        source: SourceId,
        root: PathBuf,
        made: Option<Arc<HostSizes>>,
        cx: &mut Context<Self>,
    ) {
        let still_wanted = matches!(self.hosts.get(&source), Some(HostSlot::Creating));
        let Some(sizes) = made else {
            if still_wanted {
                tracing::debug!(root = %root.display(), "folder sizes are not measured on this drive");
                self.hosts.insert(source, HostSlot::Skipped);
            }
            return;
        };
        if !still_wanted {
            // The source went away while the tree was being made.
            retire(sizes, None, cx);
            return;
        }
        self.hosts.insert(
            source,
            HostSlot::Ready(HostTable {
                sizes: Arc::clone(&sizes),
                root,
                watcher: WatcherSlot::Off,
                validated: Arc::new(Mutex::new(Validated::new(Instant::now()))),
                gate: Arc::new(Mutex::new(())),
                latest: Arc::new(AtomicU64::new(0)),
            }),
        );
        self.listing
            .update(cx, |listing, cx| listing.set_host(source, Some(sizes), cx));
        if self.switches.watch {
            self.start_watcher(source, cx);
        }
        if let Some((shown, dir)) = self.shown.clone()
            && shown == source
        {
            self.measure(source, dir, cx);
        }
    }

    /// Asks the tree of `source` for `dir` as [`plan`] says, on the background executor, then
    /// lets the listing look its rows up again.
    fn measure(&mut self, source: SourceId, dir: PathBuf, cx: &mut Context<Self>) {
        let mode = self.switches.mode;
        let Some(HostSlot::Ready(table)) = self.hosts.get_mut(&source) else {
            return;
        };
        let stale = lock(&table.validated).touch(&dir, Instant::now());
        let plan = plan(mode, &table.root, &dir, stale);
        let sizes = Arc::clone(&table.sizes);
        let root = table.root.clone();
        let watcher = match &table.watcher {
            WatcherSlot::On(watcher) => Some(Arc::clone(watcher)),
            _ => None,
        };
        let gate = Arc::clone(&table.gate);
        let latest = Arc::clone(&table.latest);
        let turn = latest.fetch_add(1, Ordering::SeqCst) + 1;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .spawn(async move {
                    let _turn = lock(&gate);
                    if plan.invalidate {
                        sizes.invalidate(&dir);
                    }
                    // A newer job has taken over: its own plan covers this folder's successor.
                    if latest.load(Ordering::SeqCst) == turn {
                        for (path, priority) in &plan.requests {
                            sizes.request(path, *priority);
                        }
                        if let Some(keep) = &plan.refocus {
                            sizes.refocus(keep);
                        }
                    }
                    if let Some(watcher) = watcher
                        && cfg!(target_os = "linux")
                    {
                        watcher.focus(&watch_focus(&sizes, &root, &dir));
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.listing
                    .update(cx, |listing, cx| listing.host_tick(source, cx));
            })
            .ok();
        })
        .detach();
    }

    /// Creates the watcher of `source` on the background executor, if there is none.
    fn start_watcher(&mut self, source: SourceId, cx: &mut Context<Self>) {
        let Some(HostSlot::Ready(table)) = self.hosts.get_mut(&source) else {
            return;
        };
        if !matches!(table.watcher, WatcherSlot::Off | WatcherSlot::Failed) {
            return;
        }
        self.watcher_serial += 1;
        let token = self.watcher_serial;
        table.watcher = WatcherSlot::Starting(token);
        let root = table.root.clone();
        let sizes = Arc::clone(&table.sizes);
        let listed = Arc::clone(&self.listed);
        let route = Arc::new(Route {
            source,
            root: root.clone(),
            sizes: Arc::clone(&sizes),
            validated: Arc::clone(&table.validated),
        });
        let batches = self.batches.clone();
        cx.spawn(async move |this, cx| {
            let place = root.clone();
            let made = cx
                .background_executor()
                .spawn(async move {
                    Watcher::new(
                        &place,
                        Box::new(move |folder| {
                            sizes.knows(folder) || lock(&listed).as_deref() == Some(folder)
                        }),
                        // The sink only hands the batch to the batch thread.
                        Box::new(move |batch| {
                            batches.send((Arc::clone(&route), batch)).ok();
                        }),
                    )
                })
                .await;
            this.update(cx, |this, cx| {
                this.watcher_started(source, token, &root, made, cx)
            })
            .ok();
        })
        .detach();
    }

    /// The watcher of `source` started, or could not.
    fn watcher_started(
        &mut self,
        source: SourceId,
        token: u64,
        root: &Path,
        made: Result<Watcher, Error>,
        cx: &mut Context<Self>,
    ) {
        let current = matches!(
            self.hosts.get(&source),
            Some(HostSlot::Ready(table)) if matches!(table.watcher, WatcherSlot::Starting(t) if t == token)
        );
        let watcher = match made {
            Ok(watcher) => Arc::new(watcher),
            Err(error) => {
                if current {
                    // Once per drive: nothing else retries until the setting changes.
                    tracing::warn!(root = %root.display(), %error, "changes on this drive are not watched");
                    if let Some(HostSlot::Ready(table)) = self.hosts.get_mut(&source) {
                        table.watcher = WatcherSlot::Failed;
                    }
                }
                return;
            }
        };
        if !current {
            // Switched off (or the source went) while it started: stop it where it may block.
            cx.background_executor()
                .spawn(async move { drop(watcher) })
                .detach();
            return;
        }
        if let Some(HostSlot::Ready(table)) = self.hosts.get_mut(&source) {
            table.watcher = WatcherSlot::On(watcher);
        }
        // On Linux the watcher covers the open folder's neighbourhood: tell it which.
        if let Some((shown, dir)) = self.shown.clone()
            && shown == source
        {
            self.measure(source, dir, cx);
        }
    }

    /// Stops the watcher of `source` (setting off), on the background executor.
    fn stop_watcher(&mut self, source: SourceId, cx: &mut Context<Self>) {
        let Some(HostSlot::Ready(table)) = self.hosts.get_mut(&source) else {
            return;
        };
        if let WatcherSlot::On(watcher) = std::mem::replace(&mut table.watcher, WatcherSlot::Off) {
            cx.background_executor()
                .spawn(async move { drop(watcher) })
                .detach();
        }
    }

    /// `folder_sizes`, `folder_sizes_network` or `watch_changes` may have changed.
    fn settings_changed(&mut self, cx: &mut Context<Self>) {
        let now = Switches::current(cx);
        let before = std::mem::replace(&mut self.switches, now);
        if now == before {
            return;
        }
        if now.watch != before.watch {
            let ready: Vec<SourceId> = self
                .hosts
                .iter()
                .filter(|(_, slot)| matches!(slot, HostSlot::Ready(_)))
                .map(|(id, _)| *id)
                .collect();
            for id in ready {
                if now.watch {
                    self.start_watcher(id, cx);
                } else {
                    self.stop_watcher(id, cx);
                }
            }
        }
        if now.network != before.network {
            // A drive that was skipped is asked again.
            self.hosts
                .retain(|_, slot| !matches!(slot, HostSlot::Skipped));
        }
        if (now.network != before.network || now.mode != before.mode)
            && let Some((source, dir)) = self.shown.clone()
        {
            self.folder_shown(source, dir, cx);
        }
    }

    /// Takes what the engine's threads pushed: repaints the rows, and reads the listing again when
    /// the batch thread says its folder changed. The batches themselves never come here: the
    /// batch thread applies them.
    fn drain(&mut self, taken: Taken, cx: &mut Context<Self>) {
        if !taken.changed.is_empty() {
            for source in taken.changed {
                self.listing
                    .update(cx, |listing, cx| listing.host_tick(source, cx));
            }
            cx.notify();
        }
        for source in taken.refresh {
            if self
                .shown
                .as_ref()
                .is_some_and(|(shown, _)| *shown == source)
            {
                self.schedule_list_refresh(cx);
            }
        }
    }

    /// Reads the listed folder again after [`LIST_REFRESH`], once for however many batches come
    /// before then.
    fn schedule_list_refresh(&mut self, cx: &mut Context<Self>) {
        if self.refresh_pending {
            return;
        }
        self.refresh_pending = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LIST_REFRESH).await;
            this.update(cx, |this, cx| {
                this.refresh_pending = false;
                this.listing
                    .update(cx, |listing, cx| listing.reload_in_place(cx));
            })
            .ok();
        })
        .detach();
    }

    /// From `Search`, when an index was built or loaded: builds the table on the background
    /// executor and gives it to the listing when it is in place.
    fn index_ready(&mut self, source: SourceId, index: Arc<Index>, cx: &mut Context<Self>) {
        // The host's folder sizes come from its live tree, never from a search index (which is
        // built only for search, with scan-local ids, and is not kept up to date).
        if self
            .sources
            .read(cx)
            .source(source)
            .is_some_and(|item| matches!(item.kind, SourceKind::Host))
        {
            return;
        }
        let Some((fs, image)) = self.fs_and_image(source, cx) else {
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        let task = cx.spawn(async move |this, cx| {
            let built = cx
                .background_executor()
                .spawn(async move {
                    let stamp = Stamp::read(fs.as_ref(), image.as_ref());
                    let info = fs.info();
                    let table =
                        IndexSizes::build(&index, &flag)?.with_volume_total(info.total_bytes);
                    let classes = table.volume_classes();
                    let used = info
                        .total_bytes
                        .zip(info.free_bytes)
                        .map(|(total, free)| total.saturating_sub(free));
                    let facts = (
                        classes,
                        fit_scale(&classes, used, table.is_overcommitted()),
                        table.is_lower_bound(),
                    );
                    Some((Arc::new(table), stamp, image, facts))
                })
                .await;
            this.update(cx, |this, cx| {
                this.jobs.remove(&source);
                if let Some((table, stamp, image, (classes, scale, lower_bound))) = built {
                    this.listing.update(cx, |listing, cx| {
                        listing.set_table(source, Some(Arc::clone(&table)), cx)
                    });
                    this.tables.insert(
                        source,
                        Built {
                            table,
                            stamp,
                            image,
                            classes,
                            scale,
                            lower_bound,
                        },
                    );
                    cx.notify();
                }
            })
            .ok();
        });
        self.jobs.insert(
            source,
            Job {
                cancel,
                _task: task,
            },
        );
    }

    /// The volume's bytes per kind and the factor that fits them into the used space (1.0 unless
    /// the files claim more than the volume has, see `IndexSizes::is_overcommitted`). `None`
    /// until the table of `source` exists. O(1); the storage bar reads it while painting.
    pub fn classes(&self, source: SourceId) -> Option<(ClassBytes, f32)> {
        self.tables
            .get(&source)
            .map(|built| (built.classes, built.scale))
    }

    /// The size table of `source`, once built. Querying it (`classes_of`, `children`) is O(subtree):
    /// do that on the background executor.
    pub fn table(&self, source: SourceId) -> Option<Arc<IndexSizes>> {
        self.tables
            .get(&source)
            .map(|built| Arc::clone(&built.table))
    }

    /// The measured tree of a host source, once made. Querying it (`classes`, `children`) is
    /// O(subtree): do that on the background executor.
    pub fn host(&self, source: SourceId) -> Option<Arc<HostSizes>> {
        match self.hosts.get(&source) {
            Some(HostSlot::Ready(table)) => Some(Arc::clone(&table.sizes)),
            _ => None,
        }
    }

    /// True when the table of `source` is a lower bound: the bar says "at least X counted".
    pub fn is_lower_bound(&self, source: SourceId) -> bool {
        self.tables
            .get(&source)
            .is_some_and(|built| built.lower_bound)
    }

    /// The filesystem of `source`, and the image file it lies in when it is a volume of an image.
    fn fs_and_image(
        &self,
        source: SourceId,
        cx: &Context<Self>,
    ) -> Option<(Arc<dyn FileSystem>, Option<PathBuf>)> {
        let sources = self.sources.read(cx);
        let item = sources.source(source)?;
        let image = match item.kind {
            SourceKind::Volume { disk, .. } => sources
                .disk(disk)
                .filter(|disk| disk.group == SourceGroup::DiskImages)
                .map(|disk| disk.path.clone()),
            _ => None,
        };
        Some((Arc::clone(&item.fs), image))
    }

    /// Drops the tables of sources that are no longer listed, and stops the watcher and workers of
    /// a host source that went.
    fn forget_gone(&mut self, cx: &mut Context<Self>) {
        let sources = self.sources.read(cx);
        let gone: Vec<SourceId> = self
            .tables
            .keys()
            .chain(self.jobs.keys())
            .chain(self.hosts.keys())
            .filter(|id| sources.source(**id).is_none())
            .copied()
            .collect();
        for id in gone {
            self.tables.remove(&id);
            self.jobs.remove(&id);
            self.listing.update(cx, |listing, cx| {
                listing.set_table(id, None, cx);
                listing.set_host(id, None, cx);
            });
            self.source_closed(id, cx);
        }
        cx.notify();
    }

    /// The host source `id` went away: its tree and watcher are stopped off the UI thread.
    fn source_closed(&mut self, id: SourceId, cx: &mut Context<Self>) {
        if let Some(HostSlot::Ready(table)) = self.hosts.remove(&id) {
            let HostTable { sizes, watcher, .. } = table;
            let watcher = match watcher {
                WatcherSlot::On(watcher) => Some(watcher),
                _ => None,
            };
            retire(sizes, watcher, cx);
        }
        if self.shown.as_ref().is_some_and(|(shown, _)| *shown == id) {
            self.shown = None;
            *lock(&self.listed) = None;
        }
    }

    /// When the window becomes active, and on F5: compares what each table was built from with
    /// the volume now. A volume that changed gets its table dropped, its index built again and,
    /// when it is the one shown, its listing read again. Disk work runs in the background.
    pub fn recheck(&mut self, cx: &mut Context<Self>) {
        let mut work = Vec::new();
        for (&id, built) in &self.tables {
            if let Some((fs, _)) = self.fs_and_image(id, cx) {
                work.push((id, fs, built.image.clone(), built.stamp.clone()));
            }
        }
        if work.is_empty() {
            return;
        }
        self.check = Some(cx.spawn(async move |this, cx| {
            let changed: Vec<SourceId> = cx
                .background_executor()
                .spawn(async move {
                    work.into_iter()
                        .filter(|(_, fs, image, stamp)| {
                            stamp.differs(&Stamp::read(fs.as_ref(), image.as_ref()))
                        })
                        .map(|(id, ..)| id)
                        .collect()
                })
                .await;
            this.update(cx, |this, cx| this.changed(&changed, cx)).ok();
        }));
    }

    /// These volumes changed: build their sizes again from a fresh index.
    fn changed(&mut self, changed: &[SourceId], cx: &mut Context<Self>) {
        for &id in changed {
            self.tables.remove(&id);
            self.listing
                .update(cx, |listing, cx| listing.set_table(id, None, cx));
            self.search.update(cx, |search, cx| search.rebuild(id, cx));
        }
        cx.notify();
        let shown = self
            .listing
            .read(cx)
            .location()
            .map(|location| location.source());
        if shown.is_some_and(|shown| changed.contains(&shown)) {
            self.listing.update(cx, |listing, cx| listing.refresh(cx));
        }
    }
}

/// The text of a folder's size in the Size column: like a file's, "3.1 GB…" while it is still
/// being measured, or "over 3.1 GB". `None` for [`SizeState::Unknown`].
pub fn size_text(state: SizeState) -> Option<SharedString> {
    match state {
        SizeState::Exact(bytes) => Some(crate::format::size(bytes).into()),
        SizeState::Running(bytes) => Some(format!("{}…", crate::format::size(bytes)).into()),
        SizeState::AtLeast(bytes) => Some(format!("over {}", crate::format::size(bytes)).into()),
        SizeState::Unknown => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn totals(bytes: u64) -> Totals {
        Totals {
            bytes,
            files: 1,
            folders: 0,
        }
    }

    #[test]
    fn a_row_size_reads_like_a_file_or_a_lower_bound() {
        assert_eq!(RowSize::None.state(), SizeState::Unknown);
        let exact = RowSize::Fixed(totals(3 << 30), false);
        assert_eq!(exact.state(), SizeState::Exact(3 << 30));
        let bound = RowSize::Fixed(totals(3 << 30), true);
        assert_eq!(bound.state(), SizeState::AtLeast(3 << 30));
        assert_eq!(
            size_text(exact.state()).map(String::from),
            Some("3.0 GB".into())
        );
        assert_eq!(
            size_text(bound.state()).map(String::from),
            Some("over 3.0 GB".into())
        );
        assert_eq!(size_text(SizeState::Unknown), None);
    }

    #[test]
    fn no_re_sort_within_500_ms_of_a_key() {
        let start = Instant::now();
        let mut guard = ResortGuard::default();
        assert_eq!(guard.wait(start), None);
        guard.input(start);
        assert_eq!(
            guard.wait(start + Duration::from_millis(100)),
            Some(Duration::from_millis(400))
        );
        assert_eq!(guard.wait(start + KEY_GUARD), None);
    }

    #[test]
    fn re_sorts_are_a_second_apart() {
        let start = Instant::now();
        let mut guard = ResortGuard::default();
        guard.resorted(start);
        assert_eq!(
            guard.wait(start + Duration::from_millis(250)),
            Some(Duration::from_millis(750))
        );
        assert_eq!(guard.wait(start + RESORT_INTERVAL), None);
        // The longer of the two waits wins.
        guard.input(start + Duration::from_millis(900));
        assert_eq!(
            guard.wait(start + Duration::from_millis(950)),
            Some(Duration::from_millis(450))
        );
    }

    #[test]
    fn a_changed_identity_or_image_is_a_difference() {
        let stamp = |change: Option<u8>, len: Option<u64>| Stamp {
            change: change.map(|b| [b; 16]),
            image: len.map(|len| (len, None)),
        };
        let built = stamp(Some(1), Some(100));
        assert!(!built.differs(&stamp(Some(1), Some(100))));
        assert!(built.differs(&stamp(Some(2), Some(100))));
        assert!(built.differs(&stamp(Some(1), Some(101))));
        // What could not be read is not a difference.
        assert!(!built.differs(&stamp(None, None)));
        assert!(!stamp(None, None).differs(&stamp(Some(2), Some(5))));
    }

    // The host's live sizes: the pure parts.

    #[test]
    fn the_engines_states_read_in_the_size_column() {
        let t = |bytes| totals(bytes);
        assert_eq!(
            SizeState::from_host(HostState::Running(t(3 << 30))),
            SizeState::Running(3 << 30)
        );
        assert_eq!(
            SizeState::from_host(HostState::Exact(t(5))),
            SizeState::Exact(5)
        );
        assert_eq!(
            SizeState::from_host(HostState::AtLeast(t(7))),
            SizeState::AtLeast(7)
        );
        assert_eq!(SizeState::from_host(HostState::Unknown), SizeState::Unknown);
        let text = |state| size_text(state).map(String::from);
        assert_eq!(text(SizeState::Running(3 << 30)), Some("3.0 GB…".into()));
        assert_eq!(text(SizeState::Exact(3 << 30)), Some("3.0 GB".into()));
        assert_eq!(
            text(SizeState::AtLeast(3 << 30)),
            Some("over 3.0 GB".into())
        );
        assert_eq!(text(SizeState::Unknown), None);
        // A running total is ordered by what is known so far; nothing known is not ordered.
        assert_eq!(SizeState::Running(9).bytes(), Some(9));
        assert_eq!(SizeState::Unknown.bytes(), None);
    }

    #[test]
    fn numbers_older_than_a_minute_are_read_again_when_the_folder_is_shown() {
        let t0 = Instant::now();
        let dir = Path::new("/data/photos");
        let mut validated = Validated::new(t0);
        // Never seen: judged by when the tree was made.
        assert!(!validated.touch(dir, t0 + Duration::from_secs(10)));
        // Seen 61 s ago: stale; the answer stamps it, so the next show is not.
        assert!(validated.touch(dir, t0 + Duration::from_secs(71)));
        assert!(!validated.touch(dir, t0 + Duration::from_secs(72)));
        // Exactly the limit is not past it.
        assert!(!validated.touch(dir, t0 + Duration::from_secs(72) + REVALIDATE_AFTER));
        // A folder the watcher reported is as fresh as that report.
        validated.mark(dir, t0 + Duration::from_secs(200));
        assert!(!validated.touch(dir, t0 + Duration::from_secs(250)));
        // A folder never seen, in a tree made long ago, is stale at once.
        assert!(validated.touch(Path::new("/data/other"), t0 + Duration::from_secs(300)));
        // A whole re-measure makes every folder fresh.
        validated.mark_all(t0 + Duration::from_secs(400));
        assert!(!validated.touch(dir, t0 + Duration::from_secs(430)));
    }

    #[test]
    fn showing_a_folder_asks_the_tree_as_the_setting_says() {
        let root = Path::new("/data");
        let dir = Path::new("/data/photos");
        // Off: nothing at all, not even for stale numbers.
        assert_eq!(
            plan(Mode::Off, root, dir, true),
            Plan {
                invalidate: false,
                requests: vec![],
                refocus: None
            }
        );
        // Open folders: the folder at `Listed`, and the one left is dropped; stale numbers are
        // read again first.
        let open = plan(Mode::OpenFolders, root, dir, true);
        assert!(open.invalidate);
        assert_eq!(open.requests, [(dir.to_path_buf(), Priority::Listed)]);
        assert_eq!(open.refocus, Some(vec![dir.to_path_buf()]));
        assert!(!plan(Mode::OpenFolders, root, dir, false).invalidate);
        // Whole drives: the drive, then the folder above it; never a refocus.
        let whole = plan(Mode::WholeDrives, root, dir, false);
        assert_eq!(
            whole.requests,
            [
                (root.to_path_buf(), Priority::Background),
                (dir.to_path_buf(), Priority::Listed)
            ]
        );
        assert_eq!(whole.refocus, None);
    }

    // What the batch thread merges (`r10-everyday.md` §3.3, "Batches never pile up").

    fn dirs(names: &[&str]) -> WatchBatch {
        WatchBatch::Dirs(names.iter().map(PathBuf::from).collect())
    }

    #[test]
    fn nothing_waiting_does_nothing() {
        assert_eq!(merge_batches(vec![], None), None);
        assert_eq!(merge_batches(vec![dirs(&[])], Some(Path::new("/a"))), None);
    }

    #[test]
    fn waiting_batches_become_one_set_with_the_listed_folder_first() {
        let merged = merge_batches(
            vec![
                dirs(&["/a", "/b"]),
                dirs(&["/b", "/c", "/a"]),
                dirs(&["/d"]),
            ],
            Some(Path::new("/c")),
        );
        assert_eq!(
            merged,
            Some(Apply::Dirs(
                ["/c", "/a", "/b", "/d"].iter().map(PathBuf::from).collect()
            ))
        );
        // Without a listed folder (or one not in the set) the order is the first-seen one.
        let merged = merge_batches(vec![dirs(&["/b", "/a"]), dirs(&["/b"])], None);
        assert_eq!(
            merged,
            Some(Apply::Dirs(
                ["/b", "/a"].iter().map(PathBuf::from).collect()
            ))
        );
        let merged = merge_batches(vec![dirs(&["/b", "/a"])], Some(Path::new("/z")));
        assert_eq!(
            merged,
            Some(Apply::Dirs(
                ["/b", "/a"].iter().map(PathBuf::from).collect()
            ))
        );
    }

    fn many(from: usize, count: usize) -> WatchBatch {
        WatchBatch::Dirs(
            (from..from + count)
                .map(|n| PathBuf::from(format!("/d{n}")))
                .collect(),
        )
    }

    #[test]
    fn exactly_the_limit_stays_folders_and_one_more_is_everything() {
        let at_limit = merge_batches(vec![many(0, MAX_BATCH_DIRS)], None);
        assert!(matches!(&at_limit, Some(Apply::Dirs(d)) if d.len() == MAX_BATCH_DIRS));
        // Two batches that overlap: the union counts, not the sum.
        let overlapping = merge_batches(vec![many(0, MAX_BATCH_DIRS), many(0, 100)], None);
        assert!(matches!(&overlapping, Some(Apply::Dirs(d)) if d.len() == MAX_BATCH_DIRS));
        // One more different folder: a single `invalidate_all`, no folder read.
        let over = merge_batches(vec![many(0, MAX_BATCH_DIRS), many(MAX_BATCH_DIRS, 1)], None);
        assert_eq!(over, Some(Apply::All));
        let over = merge_batches(vec![many(0, MAX_BATCH_DIRS + 1)], Some(Path::new("/d0")));
        assert_eq!(over, Some(Apply::All));
    }

    #[test]
    fn any_rescan_is_everything() {
        assert_eq!(
            merge_batches(vec![WatchBatch::Rescan], None),
            Some(Apply::All)
        );
        assert_eq!(
            merge_batches(
                vec![dirs(&["/a"]), WatchBatch::Rescan, dirs(&["/b"])],
                Some(Path::new("/a"))
            ),
            Some(Apply::All)
        );
    }

    // The host's live sizes and the watcher, end to end: a real folder in the temporary folder,
    // a real watcher, the real `Listing` and `Sizes`. Real threads deliver the events, so the
    // tests wait (with a deadline) while advancing the test clock, which runs the timers of
    // `SIZE_REPAINT` and `LIST_REFRESH`; the watcher's own 500 ms flush runs in real time.

    mod live {
        use std::fs;

        use gpui::{AppContext, TestAppContext};

        use super::*;
        use crate::state::listing::{Click, LoadState};
        use crate::state::navigation::{Location, Navigation};
        use crate::state::settings::SettingValue;

        const DEADLINE: Duration = Duration::from_secs(10);

        struct Env {
            dir: PathBuf,
            listing: Entity<Listing>,
            sizes: Entity<Sizes>,
            source: SourceId,
            _sources: Entity<Sources>,
            _search: Entity<Search>,
            _navigation: Entity<Navigation>,
        }

        /// A folder of `files` (name, bytes) and `folders` (each holding one file `inner.bin` of
        /// the bytes given), shown in a listing under the given settings.
        fn open(
            cx: &mut TestAppContext,
            label: &str,
            files: &[(&str, usize)],
            folders: &[(&str, usize)],
            watch: bool,
            mode: &'static str,
        ) -> Env {
            let dir = std::env::temp_dir().join(format!(
                "disk-atlas-r14-{label}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            fs::create_dir_all(&dir).unwrap();
            for (name, bytes) in files {
                fs::write(dir.join(name), vec![b'x'; *bytes]).unwrap();
            }
            for (name, bytes) in folders {
                fs::create_dir_all(dir.join(name)).unwrap();
                fs::write(dir.join(name).join("inner.bin"), vec![b'y'; *bytes]).unwrap();
            }
            cx.executor().allow_parking();
            cx.update(|cx| {
                cx.set_global(Settings::defaults());
                Settings::set(cx, "watch_changes", SettingValue::Bool(watch));
                Settings::set(cx, "folder_sizes", SettingValue::Choice(mode));
            });
            let sources = cx.new(Sources::new);
            let navigation = cx.new(|_| Navigation::new());
            let listing = cx.new(|cx| Listing::new(navigation.clone(), sources.clone(), cx));
            let search =
                cx.new(|cx| Search::new(sources.clone(), navigation.clone(), listing.clone(), cx));
            let sizes =
                cx.new(|cx| Sizes::new(sources.clone(), search.clone(), listing.clone(), cx));
            let source = cx.update(|cx| {
                let source = sources
                    .update(cx, |sources, cx| sources.add_host_for_test(dir.clone(), cx))
                    .expect("the temporary folder is a host source");
                let item = sources.read(cx).source(source).cloned().unwrap();
                navigation.update(cx, |navigation, cx| {
                    navigation.navigate(Location::root(&item), cx)
                });
                source
            });
            Env {
                dir,
                listing,
                sizes,
                source,
                _sources: sources,
                _search: search,
                _navigation: navigation,
            }
        }

        /// Runs the app until `done` holds; fails after [`DEADLINE`]. Returns how long it took.
        fn wait_for(
            cx: &mut TestAppContext,
            what: &str,
            mut done: impl FnMut(&mut TestAppContext) -> bool,
        ) -> Duration {
            let started = Instant::now();
            loop {
                cx.run_until_parked();
                if done(cx) {
                    return started.elapsed();
                }
                assert!(started.elapsed() < DEADLINE, "timed out waiting for {what}");
                cx.executor().advance_clock(Duration::from_millis(100));
                std::thread::sleep(Duration::from_millis(15));
            }
        }

        /// Runs the app for `span` of real time.
        fn settle(cx: &mut TestAppContext, span: Duration) {
            let started = Instant::now();
            while started.elapsed() < span {
                cx.run_until_parked();
                cx.executor().advance_clock(Duration::from_millis(100));
                std::thread::sleep(Duration::from_millis(15));
            }
            cx.run_until_parked();
        }

        impl Env {
            fn names(&self, cx: &mut TestAppContext) -> Vec<String> {
                cx.update(|cx| {
                    let listing = self.listing.read(cx);
                    (0..listing.len())
                        .filter_map(|p| listing.row(p).map(|row| row.entry.name.to_owned()))
                        .collect()
                })
            }

            fn ready(&self, cx: &mut TestAppContext, count: usize) -> bool {
                cx.update(|cx| {
                    let listing = self.listing.read(cx);
                    *listing.state() == LoadState::Ready && listing.len() == count
                })
            }

            /// The Size column of the row `name`.
            fn size(&self, cx: &mut TestAppContext, name: &str) -> Option<SizeState> {
                cx.update(|cx| {
                    let listing = self.listing.read(cx);
                    (0..listing.len())
                        .filter_map(|p| listing.row(p))
                        .find(|row| row.entry.name == name)
                        .map(|row| row.size)
                })
            }

            /// Selects the rows `names`, the last one clicked is the cursor.
            fn select(&self, cx: &mut TestAppContext, names: &[&str]) {
                let order = self.names(cx);
                cx.update(|cx| {
                    self.listing.update(cx, |listing, cx| {
                        for (n, name) in names.iter().enumerate() {
                            let position = order.iter().position(|o| o == name).unwrap();
                            let click = if n == 0 {
                                Click::Replace
                            } else {
                                Click::Toggle
                            };
                            listing.click(position, click, cx);
                        }
                    })
                });
            }

            /// The names of the selected rows, in display order, and the cursor's name.
            fn selection(&self, cx: &mut TestAppContext) -> (Vec<String>, Option<String>) {
                cx.update(|cx| {
                    let listing = self.listing.read(cx);
                    let selected = listing
                        .selected_positions(1000)
                        .into_iter()
                        .filter_map(|p| listing.row(p))
                        .map(|row| row.entry.name.to_owned())
                        .collect();
                    let cursor = listing
                        .cursor()
                        .and_then(|p| listing.row(p))
                        .map(|row| row.entry.name.to_owned());
                    (selected, cursor)
                })
            }

            fn watching(&self, cx: &mut TestAppContext) -> bool {
                cx.update(|cx| {
                    matches!(
                        self.sizes.read(cx).hosts.get(&self.source),
                        Some(HostSlot::Ready(HostTable {
                            watcher: WatcherSlot::On(_),
                            ..
                        }))
                    )
                })
            }

            fn tree(&self, cx: &mut TestAppContext) -> Option<Arc<HostSizes>> {
                cx.update(|cx| match self.sizes.read(cx).hosts.get(&self.source) {
                    Some(HostSlot::Ready(table)) => Some(Arc::clone(&table.sizes)),
                    _ => None,
                })
            }

            /// Stops the watcher (so Windows lets the folder go) and removes the folder.
            fn finish(self, cx: &mut TestAppContext) {
                cx.update(|cx| Settings::set(cx, "watch_changes", SettingValue::Bool(false)));
                settle(cx, Duration::from_millis(200));
                if let Some(tree) = self.tree(cx) {
                    tree.shutdown();
                }
                fs::remove_dir_all(&self.dir).ok();
            }
        }

        #[gpui::test]
        #[allow(clippy::print_stdout)]
        fn a_file_created_outside_appears_and_the_selection_stays(cx: &mut TestAppContext) {
            let env = open(
                cx,
                "create",
                &[("a.txt", 1), ("b.txt", 2), ("c.txt", 3)],
                &[("sub", 10)],
                true,
                "open_folders",
            );
            wait_for(cx, "the folder to load", |cx| env.ready(cx, 4));
            wait_for(cx, "the watcher to start", |cx| env.watching(cx));
            env.select(cx, &["b.txt"]);
            let before = env.selection(cx);
            assert_eq!(before, (vec!["b.txt".to_owned()], Some("b.txt".to_owned())));

            fs::write(env.dir.join("aa.txt"), b"new").unwrap();
            let took = wait_for(cx, "the new file in the listing", |cx| {
                env.names(cx).iter().any(|n| n == "aa.txt")
            });
            println!("a created file reached the listing in {took:?}");
            assert!(took < Duration::from_secs(5), "took {took:?}");
            // It sorted in by name, and the selection and cursor are still on "b.txt".
            let names = env.names(cx);
            assert_eq!(names.len(), 5);
            let (a, aa) = (
                names.iter().position(|n| n == "a.txt").unwrap(),
                names.iter().position(|n| n == "aa.txt").unwrap(),
            );
            assert_eq!(aa, a + 1);
            assert_eq!(env.selection(cx), before);
            assert!(env.ready(cx, 5));
            env.finish(cx);
        }

        #[gpui::test]
        fn a_selected_file_deleted_outside_leaves_the_selection(cx: &mut TestAppContext) {
            let env = open(
                cx,
                "delete",
                &[("a.txt", 1), ("b.txt", 2), ("c.txt", 3)],
                &[],
                true,
                "open_folders",
            );
            wait_for(cx, "the folder to load", |cx| env.ready(cx, 3));
            wait_for(cx, "the watcher to start", |cx| env.watching(cx));
            env.select(cx, &["a.txt", "b.txt"]);
            assert_eq!(env.selection(cx).0, ["a.txt", "b.txt"]);

            fs::remove_file(env.dir.join("b.txt")).unwrap();
            wait_for(cx, "b.txt to leave the listing", |cx| {
                !env.names(cx).iter().any(|n| n == "b.txt")
            });
            // "a.txt" stays selected; "b.txt" was the cursor, which falls to a neighbour.
            let (selected, cursor) = env.selection(cx);
            assert_eq!(selected, ["a.txt"]);
            assert_eq!(cursor.as_deref(), Some("c.txt"));
            assert!(env.ready(cx, 2));
            env.finish(cx);
        }

        #[gpui::test]
        fn with_watching_off_nothing_changes_until_f5(cx: &mut TestAppContext) {
            let env = open(
                cx,
                "off",
                &[("a.txt", 1), ("b.txt", 2)],
                &[],
                false,
                "open_folders",
            );
            wait_for(cx, "the folder to load", |cx| env.ready(cx, 2));
            env.select(cx, &["b.txt"]);
            fs::write(env.dir.join("new.txt"), b"1").unwrap();
            fs::remove_file(env.dir.join("a.txt")).unwrap();
            settle(cx, Duration::from_millis(2000));
            assert!(!env.watching(cx));
            assert_eq!(env.names(cx), ["a.txt", "b.txt"]);
            // F5 reads it again.
            cx.update(|cx| env.listing.update(cx, |listing, cx| listing.refresh(cx)));
            wait_for(cx, "F5 to show the change", |cx| env.ready(cx, 2));
            assert_eq!(env.names(cx), ["b.txt", "new.txt"]);
            env.finish(cx);
        }

        #[gpui::test]
        fn the_setting_starts_and_stops_the_watcher_at_runtime(cx: &mut TestAppContext) {
            let env = open(cx, "toggle", &[("a.txt", 1)], &[], false, "open_folders");
            wait_for(cx, "the folder to load", |cx| env.ready(cx, 1));
            assert!(!env.watching(cx));
            cx.update(|cx| Settings::set(cx, "watch_changes", SettingValue::Bool(true)));
            wait_for(cx, "the watcher to start", |cx| env.watching(cx));
            cx.update(|cx| Settings::set(cx, "watch_changes", SettingValue::Bool(false)));
            wait_for(cx, "the watcher to stop", |cx| !env.watching(cx));
            env.finish(cx);
        }

        #[gpui::test]
        fn a_folder_with_files_shows_its_exact_size_after_the_walk(cx: &mut TestAppContext) {
            let env = open(
                cx,
                "exact",
                &[("note.txt", 5)],
                &[("sub", 1000), ("other", 24)],
                true,
                "open_folders",
            );
            wait_for(cx, "the folder to load", |cx| env.ready(cx, 3));
            wait_for(cx, "the exact sizes", |cx| {
                env.size(cx, "sub") == Some(SizeState::Exact(1000))
                    && env.size(cx, "other") == Some(SizeState::Exact(24))
            });
            // A file shows its own size, not the folder column's.
            assert_eq!(env.size(cx, "note.txt"), Some(SizeState::Unknown));
            env.finish(cx);
        }

        #[gpui::test]
        #[allow(clippy::print_stdout)]
        fn a_size_follows_a_change_inside_a_listed_folders_child(cx: &mut TestAppContext) {
            let env = open(cx, "grow", &[], &[("sub", 1000)], true, "open_folders");
            wait_for(cx, "the folder to load", |cx| env.ready(cx, 1));
            wait_for(cx, "the exact size", |cx| {
                env.size(cx, "sub") == Some(SizeState::Exact(1000))
            });
            wait_for(cx, "the watcher to start", |cx| env.watching(cx));
            fs::write(env.dir.join("sub").join("more.bin"), vec![b'z'; 2000]).unwrap();
            let took = wait_for(cx, "the size to grow", |cx| {
                env.size(cx, "sub") == Some(SizeState::Exact(3000))
            });
            println!("a child folder's size caught up with a change in {took:?}");
            assert!(took < Duration::from_secs(5), "took {took:?}");
            env.finish(cx);
        }

        #[gpui::test]
        fn with_folder_sizes_off_nothing_is_measured(cx: &mut TestAppContext) {
            let env = open(cx, "sizes-off", &[], &[("sub", 1000)], true, "off");
            wait_for(cx, "the folder to load", |cx| env.ready(cx, 1));
            wait_for(cx, "the tree", |cx| env.tree(cx).is_some());
            settle(cx, Duration::from_millis(1000));
            assert_eq!(env.size(cx, "sub"), Some(SizeState::Unknown));
            let tree = env.tree(cx).unwrap();
            assert_eq!(tree.state(&env.dir.join("sub")), HostState::Unknown);
            assert_eq!(tree.state(&env.dir), HostState::Unknown);
            env.finish(cx);
        }

        #[gpui::test]
        fn visible_rows_are_queued_when_scrolling_settles(cx: &mut TestAppContext) {
            // Sizes are on, but the folder itself is not queued by `whole_drives`' root: only the
            // settle timer's `Visible` request measures the row.
            let env = open(cx, "visible", &[], &[("sub", 77)], false, "open_folders");
            wait_for(cx, "the folder to load", |cx| env.ready(cx, 1));
            cx.update(|cx| {
                env.listing
                    .update(cx, |listing, cx| listing.note_visible(0..1, cx))
            });
            wait_for(cx, "the visible row's size", |cx| {
                env.size(cx, "sub") == Some(SizeState::Exact(77))
            });
            env.finish(cx);
        }
    }
}
