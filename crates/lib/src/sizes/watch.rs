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

//! Watching a folder tree on this computer for changes (design
//! `r10-everyday.md` §3.3).
//!
//! [`Watcher`] turns the operating system's change events under one root
//! into batches of "the direct entries of these folders changed", which the
//! app passes to [`HostSizes::invalidate`](super::host::HostSizes::invalidate)
//! and uses to refresh the folder it lists. When events were lost the batch
//! is [`WatchBatch::Rescan`], for
//! [`HostSizes::invalidate_all`](super::host::HostSizes::invalidate_all).
//!
//! # How it works
//!
//! - **One `notify` watcher per root.** Windows and macOS watch the whole
//!   tree with one recursive handle. Linux (inotify) needs one watch per
//!   folder, so there the root is watched on its own and
//!   [`Watcher::focus`] adds the folders nearest the open one.
//! - **Reduction, on `notify`'s thread.** Each path in an event becomes the
//!   folder that holds it; a path outside the root, and the root itself,
//!   are skipped. Access events (opening, reading, closing, a new access
//!   time) are ignored. `wanted` is asked once per folder per flush (a
//!   folder already pending is not asked again), and a folder it refuses
//!   is dropped at once, so background noise in folders nobody measured
//!   never reaches the set. Our lock is held only to look up and insert;
//!   `wanted` runs without it.
//! - **Flush thread** ("disk-atlas-watch"). It sleeps until something is
//!   pending, waits [`WATCH_FLUSH`] after the first pending event, takes
//!   the set under the lock and calls `sink` without it. A panic in `sink`
//!   or `wanted` is caught; a panicking `wanted` counts as a refusal.
//! - **Lost events.** `notify`'s rescan flag, an error delivered as an
//!   event, and a set that would pass [`MAX_BATCH_DIRS`] all become one
//!   [`WatchBatch::Rescan`].
//! - **Paths** are compared as [`Path`]s, with no case folding. Where the
//!   backend reports the canonical spelling of the root (macOS FSEvents:
//!   `/private/var` for `/var`), paths are moved back under the root the
//!   caller gave, so `wanted` and `sink` see the caller's spelling.
//!
//! # What is bounded
//!
//! The pending set holds at most [`MAX_BATCH_DIRS`] folders; past that it is
//! emptied and the next flush is a rescan, and events until then cost only
//! a lock. At most one batch per [`WATCH_FLUSH`]. On Linux at most
//! [`MAX_LINUX_WATCHES`] folders besides the root are watched. Work per
//! event is one pass over the event's paths. An idle watcher's thread
//! sleeps with no timeout. Dropping the [`Watcher`] stops `notify` first,
//! then the flush thread, and waits for it; no `wanted` call starts after
//! the drop returns.
//!
//! # What `notify` 8.2 does not report
//!
//! On Windows, `notify` 8.2.2 never sets the rescan flag: when the
//! 16 KiB change buffer overflows, the events in it are lost without a
//! notice. Nothing here can detect that; the app's [`REVALIDATE_AFTER`]
//! re-measure on coming back to a folder is the backstop.

use std::borrow::Cow;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use notify::event::{MetadataKind, ModifyKind};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::error::{Error, IoContext, Result};

/// Changes are collected for this long after the first one, then sent as
/// one batch.
pub const WATCH_FLUSH: Duration = Duration::from_millis(500);

/// Most folders in one [`WatchBatch::Dirs`]. A flush that would hold more
/// is sent as [`WatchBatch::Rescan`].
pub const MAX_BATCH_DIRS: usize = 4096;

/// Linux only: most folders [`Watcher::focus`] watches besides the root.
pub const MAX_LINUX_WATCHES: usize = 4096;

/// For the app: a folder that was not watched (past [`MAX_LINUX_WATCHES`],
/// or on a drive whose watcher could not start) is measured again when the
/// user comes back to it, if its numbers are older than this.
pub const REVALIDATE_AFTER: Duration = Duration::from_secs(60);

/// What changed, as one flush reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchBatch {
    /// The direct entries of these folders changed. De-duplicated,
    /// at most `MAX_BATCH_DIRS`.
    Dirs(Vec<PathBuf>),
    /// Events were lost: treat everything as changed.
    Rescan,
}

type Wanted = Box<dyn Fn(&Path) -> bool + Send + Sync>;
type Sink = Box<dyn Fn(WatchBatch) + Send + Sync>;

/// How the root itself is watched: recursively where the backend watches a
/// tree with one handle, on its own where it needs a watch per folder.
#[cfg(target_os = "linux")]
const ROOT_MODE: RecursiveMode = RecursiveMode::NonRecursive;
#[cfg(not(target_os = "linux"))]
const ROOT_MODE: RecursiveMode = RecursiveMode::Recursive;

/// The limits, lowered by tests.
#[derive(Debug, Clone, Copy)]
struct Config {
    flush: Duration,
    max_dirs: usize,
    #[cfg(target_os = "linux")]
    max_watches: usize,
}

impl Config {
    fn standard() -> Self {
        Self {
            flush: WATCH_FLUSH,
            max_dirs: MAX_BATCH_DIRS,
            #[cfg(target_os = "linux")]
            max_watches: MAX_LINUX_WATCHES,
        }
    }
}

/// What the next flush sends.
#[derive(Default)]
struct Pending {
    /// Folders whose direct entries changed; at most `max_dirs`.
    dirs: HashSet<PathBuf>,
    /// Events were lost: the next flush is a rescan, and `dirs` stays empty.
    rescan: bool,
    /// When the first event of this flush arrived; `None` when nothing is
    /// pending.
    since: Option<Instant>,
    /// The watcher is being dropped.
    stop: bool,
}

/// What `notify`'s event handler and the flush thread share.
struct Shared {
    root: PathBuf,
    /// The root as the backend may spell it, when that differs from `root`.
    alt_root: Option<PathBuf>,
    wanted: Wanted,
    /// `false` once the watcher is dropped. Held for reading while an event
    /// is handled, so the drop waits for a `wanted` call in progress.
    live: RwLock<bool>,
    pending: Mutex<Pending>,
    wake: Condvar,
    config: Config,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Does an event of this kind change a folder's entries? Access events
/// (open, read, close, a new access time) do not.
fn counts(kind: &EventKind) -> bool {
    !matches!(
        kind,
        EventKind::Access(_) | EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime))
    )
}

impl Shared {
    fn new(root: PathBuf, alt_root: Option<PathBuf>, wanted: Wanted, config: Config) -> Self {
        Self {
            root,
            alt_root,
            wanted,
            live: RwLock::new(true),
            pending: Mutex::new(Pending::default()),
            wake: Condvar::new(),
            config,
        }
    }

    /// `notify`'s event handler.
    fn handle(&self, event: notify::Result<Event>) {
        let live = self.live.read().unwrap_or_else(PoisonError::into_inner);
        if !*live {
            return;
        }
        match event {
            // An error from the backend (inotify's watch limit, a read that
            // failed): events may have been missed.
            Err(_) => self.lost(),
            Ok(event) if event.need_rescan() => self.lost(),
            Ok(event) => {
                if !counts(&event.kind) {
                    return;
                }
                for path in &event.paths {
                    if let Some(folder) = self.holder(path) {
                        self.offer(&folder);
                    }
                }
            }
        }
        drop(live);
    }

    /// The folder under the root that holds `path`, in the caller's
    /// spelling of the root. `None` for the root itself and for anything
    /// outside it.
    fn holder<'a>(&'a self, path: &'a Path) -> Option<Cow<'a, Path>> {
        let parent = path.parent()?;
        if parent.starts_with(&self.root) {
            return Some(Cow::Borrowed(parent));
        }
        let rel = parent.strip_prefix(self.alt_root.as_deref()?).ok()?;
        if rel.as_os_str().is_empty() {
            Some(Cow::Borrowed(self.root.as_path()))
        } else {
            Some(Cow::Owned(self.root.join(rel)))
        }
    }

    /// Store `folder` for the next flush, if it is not there yet and
    /// `wanted` takes it.
    fn offer(&self, folder: &Path) {
        {
            let p = lock(&self.pending);
            if p.stop || p.rescan || p.dirs.contains(folder) {
                return;
            }
        }
        let wanted = catch_unwind(AssertUnwindSafe(|| (self.wanted)(folder))).unwrap_or(false);
        if !wanted {
            return;
        }
        let mut p = lock(&self.pending);
        if p.stop || p.rescan || p.dirs.contains(folder) {
            return;
        }
        if p.dirs.len() >= self.config.max_dirs {
            p.dirs = HashSet::new();
            p.rescan = true;
        } else {
            p.dirs.insert(folder.to_path_buf());
        }
        self.arm(&mut p);
    }

    /// Events were lost: the next flush is a rescan.
    fn lost(&self) {
        let mut p = lock(&self.pending);
        if p.stop {
            return;
        }
        p.dirs = HashSet::new();
        p.rescan = true;
        self.arm(&mut p);
    }

    /// Start the flush clock, and wake the flush thread, on the first
    /// pending event.
    fn arm(&self, p: &mut Pending) {
        if p.since.is_none() {
            p.since = Some(Instant::now());
            self.wake.notify_one();
        }
    }

    /// Waits for the next batch. `None` once the watcher is dropped.
    fn next_batch(&self) -> Option<WatchBatch> {
        let mut p = lock(&self.pending);
        loop {
            if p.stop {
                return None;
            }
            let Some(since) = p.since else {
                p = self.wake.wait(p).unwrap_or_else(PoisonError::into_inner);
                continue;
            };
            let now = Instant::now();
            let due = since.checked_add(self.config.flush).unwrap_or(now);
            if now >= due {
                if let Some(batch) = take_batch(&mut p) {
                    return Some(batch);
                }
                continue;
            }
            p = self
                .wake
                .wait_timeout(p, due.saturating_duration_since(now))
                .map_or_else(|e| e.into_inner().0, |(guard, _)| guard);
        }
    }
}

/// Empties `p` into a batch: a rescan if events were lost, else the folders
/// (sorted), else nothing. Never an empty `Dirs`.
fn take_batch(p: &mut Pending) -> Option<WatchBatch> {
    p.since = None;
    if std::mem::take(&mut p.rescan) {
        p.dirs = HashSet::new();
        return Some(WatchBatch::Rescan);
    }
    if p.dirs.is_empty() {
        return None;
    }
    let mut dirs: Vec<PathBuf> = std::mem::take(&mut p.dirs).into_iter().collect();
    dirs.sort_unstable();
    Some(WatchBatch::Dirs(dirs))
}

/// The flush thread: one batch per flush to `sink`, until the watcher is
/// dropped.
fn flush_loop(shared: &Shared, sink: &Sink) {
    while let Some(batch) = shared.next_batch() {
        let _ = catch_unwind(AssertUnwindSafe(|| sink(batch)));
    }
}

/// Maps a `notify` error onto the engine's, under "watching `root`".
fn notify_error(root: &Path, err: notify::Error) -> Error {
    let message = err.to_string();
    let context = IoContext::Path {
        op: "watching",
        path: root.to_path_buf(),
    };
    match err.kind {
        notify::ErrorKind::Io(source) => Error::io(context, source),
        notify::ErrorKind::PathNotFound => Error::Io {
            context,
            source: io::Error::new(io::ErrorKind::NotFound, message),
        },
        _ => Error::Io {
            context,
            source: io::Error::other(message),
        },
    }
}

/// The `notify` watcher, and on Linux the folders watched besides the root.
struct Watches {
    notify: Option<RecommendedWatcher>,
    #[cfg(target_os = "linux")]
    extra: HashSet<PathBuf>,
}

/// Watches one folder tree on this computer and reports which folders'
/// direct entries changed.
///
/// `Send + Sync`; every method takes `&self`. Dropping it stops the
/// watcher and its flush thread and waits for the thread, unless it is
/// dropped from inside `sink` (on that thread), which then ends as soon as
/// `sink` returns. It must not be dropped, and [`focus`](Self::focus) must
/// not be called, from inside `wanted`, which runs on `notify`'s thread.
pub struct Watcher {
    watches: Mutex<Watches>,
    shared: Arc<Shared>,
    flusher: Option<JoinHandle<()>>,
}

impl Watcher {
    /// Watch `root` and everything inside it. `wanted` is asked for the
    /// folder of each event, on the watcher's thread, and must be cheap;
    /// a folder it refuses is dropped at once. `sink` receives batches.
    pub fn new(
        root: &Path,
        wanted: Box<dyn Fn(&Path) -> bool + Send + Sync>,
        sink: Box<dyn Fn(WatchBatch) + Send + Sync>,
    ) -> Result<Self> {
        Self::with_config(root, wanted, sink, Config::standard())
    }

    fn with_config(root: &Path, wanted: Wanted, sink: Sink, config: Config) -> Result<Self> {
        let context = || IoContext::Path {
            op: "watching",
            path: root.to_path_buf(),
        };
        let meta = fs::metadata(root).map_err(|e| Error::io(context(), e))?;
        if !meta.is_dir() {
            return Err(Error::Io {
                context: context(),
                source: io::Error::other("not a folder"),
            });
        }
        let alt_root = fs::canonicalize(root).ok().filter(|c| c != root);
        let shared = Arc::new(Shared::new(root.to_path_buf(), alt_root, wanted, config));

        let handler = Arc::clone(&shared);
        let mut notify = RecommendedWatcher::new(
            move |event| handler.handle(event),
            notify::Config::default().with_follow_symlinks(false),
        )
        .map_err(|e| notify_error(root, e))?;
        notify
            .watch(root, ROOT_MODE)
            .map_err(|e| notify_error(root, e))?;

        let flushing = Arc::clone(&shared);
        let flusher = thread::Builder::new()
            .name("disk-atlas-watch".into())
            .spawn(move || flush_loop(&flushing, &sink))
            .map_err(|e| Error::io(context(), e))?;

        Ok(Self {
            watches: Mutex::new(Watches {
                notify: Some(notify),
                #[cfg(target_os = "linux")]
                extra: HashSet::new(),
            }),
            shared,
            flusher: Some(flusher),
        })
    }

    /// Linux: watch these folders besides the root, in place of those the
    /// last call gave. Pass the folders nearest the open folder first: only
    /// the first [`MAX_LINUX_WATCHES`] are watched. Folders outside the
    /// root, the root itself and repeats are skipped, as is a folder that
    /// cannot be watched (gone, no permission, the system's watch limit).
    ///
    /// Windows and macOS: does nothing; the whole tree is watched already.
    ///
    /// Blocks while `notify` adds and removes the watches; call it off the
    /// UI thread.
    pub fn focus(&self, dirs: &[PathBuf]) {
        #[cfg(target_os = "linux")]
        self.focus_linux(dirs);
        #[cfg(not(target_os = "linux"))]
        let _ = dirs;
    }

    #[cfg(target_os = "linux")]
    fn focus_linux(&self, dirs: &[PathBuf]) {
        let root = self.shared.root.as_path();
        let limit = self.shared.config.max_watches;
        let mut keep: HashSet<&Path> = HashSet::new();
        let mut order: Vec<&Path> = Vec::new();
        for dir in dirs {
            if order.len() >= limit {
                break;
            }
            let dir = dir.as_path();
            if dir != root && dir.starts_with(root) && keep.insert(dir) {
                order.push(dir);
            }
        }
        let mut watches = lock(&self.watches);
        let Watches { notify, extra } = &mut *watches;
        let Some(notify) = notify.as_mut() else {
            return;
        };
        // Remove first, so the joining folders do not meet the system's limit
        // because of folders that are leaving.
        extra.retain(|dir| {
            if keep.contains(dir.as_path()) {
                true
            } else {
                let _ = notify.unwatch(dir);
                false
            }
        });
        for dir in order {
            if !extra.contains(dir) && notify.watch(dir, RecursiveMode::NonRecursive).is_ok() {
                extra.insert(dir.to_path_buf());
            }
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // No more events: stop `notify` first.
        let notify = lock(&self.watches).notify.take();
        drop(notify);
        // Wait out an event being handled (`notify`'s thread may still be
        // finishing one), and refuse the rest.
        *self
            .shared
            .live
            .write()
            .unwrap_or_else(PoisonError::into_inner) = false;
        {
            let mut p = lock(&self.shared.pending);
            p.stop = true;
            p.dirs = HashSet::new();
            p.rescan = false;
            p.since = None;
        }
        self.shared.wake.notify_all();
        if let Some(handle) = self.flusher.take() {
            // Dropped from inside `sink`: that thread ends when `sink` returns.
            if handle.thread().id() != thread::current().id() {
                let _ = handle.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sizes::host::{HostSizes, Priority, SizeState};
    use notify::event::{AccessKind, AccessMode, CreateKind, Flag, RenameMode};
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Receiver};

    // --- helpers -----------------------------------------------------------

    /// A uniquely named temp folder, removed (best effort) on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("disk-atlas-watch-{}-{n}", std::process::id()));
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

    /// Longer than one flush: no batch for this long means the watcher is quiet.
    const QUIET: Duration = Duration::from_millis(800);
    /// How long a change may take to arrive as a batch.
    const ARRIVES: Duration = Duration::from_secs(2);

    fn all() -> Wanted {
        Box::new(|_| true)
    }

    fn collector() -> (Sink, Receiver<WatchBatch>) {
        let (tx, rx) = mpsc::channel();
        (
            Box::new(move |batch| {
                let _ = tx.send(batch);
            }),
            rx,
        )
    }

    fn names(batch: &WatchBatch, dir: &Path) -> bool {
        matches!(batch, WatchBatch::Dirs(dirs) if dirs.iter().any(|d| d == dir))
    }

    /// Drops batches until none arrives for `QUIET`.
    fn settle(rx: &Receiver<WatchBatch>) {
        for _ in 0..50 {
            if rx.recv_timeout(QUIET).is_err() {
                return;
            }
        }
        panic!("the watcher never went quiet");
    }

    /// Receives until a batch names `dir`, within `ARRIVES`; returns every
    /// batch seen.
    fn expect_named(rx: &Receiver<WatchBatch>, dir: &Path, what: &str) -> Vec<WatchBatch> {
        let deadline = Instant::now() + ARRIVES;
        let mut seen = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(batch) => {
                    let hit = names(&batch, dir);
                    seen.push(batch);
                    if hit {
                        return seen;
                    }
                }
                Err(_) => panic!(
                    "{what}: no batch named {} within {ARRIVES:?}; saw {seen:?}",
                    dir.display()
                ),
            }
        }
    }

    /// Every batch until none arrives for `quiet` (at most 200).
    fn collect_until_quiet(rx: &Receiver<WatchBatch>, quiet: Duration) -> Vec<WatchBatch> {
        let mut seen = Vec::new();
        for _ in 0..200 {
            match rx.recv_timeout(quiet) {
                Ok(batch) => seen.push(batch),
                Err(_) => break,
            }
        }
        seen
    }

    fn shared(root: &Path, wanted: Wanted, config: Config) -> Shared {
        Shared::new(root.to_path_buf(), None, wanted, config)
    }

    fn event(kind: EventKind, paths: &[PathBuf]) -> notify::Result<Event> {
        let mut e = Event::new(kind);
        e.paths = paths.to_vec();
        Ok(e)
    }

    fn create(path: PathBuf) -> notify::Result<Event> {
        event(EventKind::Create(CreateKind::File), &[path])
    }

    fn take(s: &Shared) -> Option<WatchBatch> {
        take_batch(&mut lock(&s.pending))
    }

    // --- the type -----------------------------------------------------------

    #[test]
    fn watcher_is_send_sync() {
        fn assert_bounds<T: Send + Sync>() {}
        assert_bounds::<Watcher>();
        assert_bounds::<WatchBatch>();
    }

    #[test]
    fn missing_root_is_an_error() {
        let tmp = TempDir::new();
        let missing = tmp.path().join("missing");
        let (sink, _rx) = collector();
        match Watcher::new(&missing, all(), sink) {
            Err(Error::Io { context, source }) => {
                assert_eq!(source.kind(), io::ErrorKind::NotFound);
                assert_eq!(
                    context,
                    IoContext::Path {
                        op: "watching",
                        path: missing.clone()
                    }
                );
            }
            Err(other) => panic!("expected Error::Io, got {other:?}"),
            Ok(_) => panic!("watching a missing folder succeeded"),
        }
        let file = tmp.path().join("file.txt");
        fs::write(&file, b"x").unwrap();
        let (sink, _rx) = collector();
        assert!(matches!(
            Watcher::new(&file, all(), sink),
            Err(Error::Io { .. })
        ));
    }

    #[test]
    fn notify_errors_map_onto_engine_errors() {
        let root = Path::new("root");
        let io_err = notify_error(root, notify::Error::io(io::Error::other("boom")));
        assert!(matches!(io_err, Error::Io { .. }));
        assert_eq!(io_err.to_string(), "watching root: boom");
        let denied = notify_error(
            root,
            notify::Error::io(io::Error::from(io::ErrorKind::PermissionDenied)),
        );
        assert!(matches!(denied, Error::AccessDenied { .. }));
        match notify_error(root, notify::Error::path_not_found()) {
            Error::Io { source, .. } => assert_eq!(source.kind(), io::ErrorKind::NotFound),
            other => panic!("{other:?}"),
        }
        match notify_error(root, notify::Error::new(notify::ErrorKind::MaxFilesWatch)) {
            Error::Io { source, .. } => {
                assert_eq!(source.kind(), io::ErrorKind::Other);
                assert_eq!(source.to_string(), "OS file watch limit reached.");
            }
            other => panic!("{other:?}"),
        }
    }

    // --- reduction, fed synthetic events ------------------------------------

    #[test]
    fn events_reduce_to_the_holding_folder() {
        let root = PathBuf::from("root");
        let s = shared(&root, all(), Config::standard());
        s.handle(create(root.join("a").join("f.txt")));
        s.handle(create(root.join("a").join("g.txt")));
        s.handle(create(root.join("top.txt")));
        assert_eq!(
            take(&s),
            Some(WatchBatch::Dirs(vec![root.clone(), root.join("a")]))
        );
        assert_eq!(take(&s), None, "a flush is never an empty Dirs");
    }

    #[test]
    fn root_itself_and_paths_outside_are_skipped() {
        let root = PathBuf::from("root");
        let s = shared(&root, all(), Config::standard());
        s.handle(create(root.clone()));
        s.handle(create(PathBuf::from("elsewhere").join("f.txt")));
        s.handle(create(PathBuf::from("rootless").join("f.txt")));
        s.handle(create(PathBuf::from("f.txt")));
        s.handle(event(EventKind::Any, &[]));
        assert_eq!(take(&s), None);
    }

    #[test]
    fn a_rename_names_both_parents() {
        let root = PathBuf::from("root");
        let s = shared(&root, all(), Config::standard());
        s.handle(event(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            &[root.join("a").join("x"), root.join("b").join("y")],
        ));
        assert_eq!(
            take(&s),
            Some(WatchBatch::Dirs(vec![root.join("a"), root.join("b")]))
        );
    }

    #[test]
    fn access_events_are_ignored() {
        let root = PathBuf::from("root");
        let s = shared(&root, all(), Config::standard());
        let f = root.join("a").join("f.txt");
        s.handle(event(
            EventKind::Access(AccessKind::Open(AccessMode::Any)),
            std::slice::from_ref(&f),
        ));
        s.handle(event(
            EventKind::Access(AccessKind::Close(AccessMode::Write)),
            std::slice::from_ref(&f),
        ));
        s.handle(event(
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::AccessTime)),
            std::slice::from_ref(&f),
        ));
        assert_eq!(take(&s), None);
        for kind in [
            EventKind::Any,
            EventKind::Other,
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::WriteTime)),
            EventKind::Modify(ModifyKind::Any),
            EventKind::Remove(notify::event::RemoveKind::Any),
        ] {
            s.handle(event(kind, std::slice::from_ref(&f)));
            assert_eq!(
                take(&s),
                Some(WatchBatch::Dirs(vec![root.join("a")])),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn need_rescan_and_errors_become_rescan() {
        let root = PathBuf::from("root");
        let s = shared(&root, all(), Config::standard());
        s.handle(create(root.join("a").join("f")));
        s.handle(Ok(Event::new(EventKind::Other).set_flag(Flag::Rescan)));
        assert_eq!(take(&s), Some(WatchBatch::Rescan));
        assert_eq!(take(&s), None);

        s.handle(Err(notify::Error::generic("backend failed")));
        s.handle(create(root.join("b").join("f")));
        assert_eq!(take(&s), Some(WatchBatch::Rescan));
        assert_eq!(take(&s), None);
    }

    #[test]
    fn more_than_max_batch_dirs_becomes_rescan_and_stays_bounded() {
        let root = PathBuf::from("root");
        let asked = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&asked);
        let s = shared(
            &root,
            Box::new(move |_| {
                counter.fetch_add(1, Ordering::Relaxed);
                true
            }),
            Config::standard(),
        );
        for i in 0..MAX_BATCH_DIRS {
            s.handle(create(root.join(format!("d{i}")).join("f")));
        }
        assert_eq!(lock(&s.pending).dirs.len(), MAX_BATCH_DIRS);
        match take(&s) {
            Some(WatchBatch::Dirs(dirs)) => assert_eq!(dirs.len(), MAX_BATCH_DIRS),
            other => panic!("{other:?}"),
        }

        for i in 0..=MAX_BATCH_DIRS {
            s.handle(create(root.join(format!("d{i}")).join("f")));
        }
        {
            let p = lock(&s.pending);
            assert!(p.rescan);
            assert!(
                p.dirs.is_empty(),
                "the set is emptied once it would overflow"
            );
        }
        // Later events are dropped without asking `wanted`.
        let before = asked.load(Ordering::Relaxed);
        for i in 0..10_000 {
            s.handle(create(root.join(format!("e{i}")).join("f")));
        }
        assert_eq!(asked.load(Ordering::Relaxed), before);
        assert!(lock(&s.pending).dirs.is_empty());
        assert_eq!(take(&s), Some(WatchBatch::Rescan));
        assert_eq!(take(&s), None);
    }

    #[test]
    fn wanted_is_asked_once_per_folder_per_flush() {
        let root = PathBuf::from("root");
        let asked = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&asked);
        let s = shared(
            &root,
            Box::new(move |_| {
                counter.fetch_add(1, Ordering::Relaxed);
                true
            }),
            Config::standard(),
        );
        for i in 0..10_000 {
            s.handle(create(root.join("hot").join(format!("f{i}"))));
        }
        assert_eq!(asked.load(Ordering::Relaxed), 1);
        assert_eq!(take(&s), Some(WatchBatch::Dirs(vec![root.join("hot")])));
    }

    #[test]
    fn refused_and_panicking_wanted_drop_the_folder() {
        let root = PathBuf::from("root");
        let noise = root.join("noise");
        let refused = noise.clone();
        let s = shared(
            &root,
            Box::new(move |dir| {
                if dir == refused {
                    return false;
                }
                if dir.ends_with("boom") {
                    panic!("wanted panics");
                }
                true
            }),
            Config::standard(),
        );
        for i in 0..1_000 {
            s.handle(create(noise.join(format!("f{i}"))));
        }
        s.handle(create(root.join("boom").join("f")));
        assert_eq!(take(&s), None);
        s.handle(create(root.join("fine").join("f")));
        assert_eq!(take(&s), Some(WatchBatch::Dirs(vec![root.join("fine")])));
    }

    #[test]
    fn the_canonical_root_is_moved_back_under_the_given_root() {
        let root = PathBuf::from("var").join("t");
        let canonical = PathBuf::from("private").join("var").join("t");
        let s = Shared::new(
            root.clone(),
            Some(canonical.clone()),
            all(),
            Config::standard(),
        );
        s.handle(create(canonical.join("a").join("f")));
        s.handle(create(canonical.join("top")));
        s.handle(create(canonical.clone()));
        assert_eq!(
            take(&s),
            Some(WatchBatch::Dirs(vec![root.clone(), root.join("a")]))
        );
    }

    #[test]
    fn nothing_is_stored_after_stop() {
        let root = PathBuf::from("root");
        let s = shared(&root, all(), Config::standard());
        *s.live.write().unwrap() = false;
        s.handle(create(root.join("a").join("f")));
        s.handle(Err(notify::Error::generic("late")));
        assert_eq!(take(&s), None);
    }

    // --- the real watcher ---------------------------------------------------

    #[test]
    fn create_write_rename_delete_name_the_holding_folder() {
        let tmp = TempDir::new();
        let root = tmp.path();
        let sub = root.join("sub");
        let inner = sub.join("inner");
        fs::create_dir_all(&inner).unwrap();
        fs::write(inner.join("old.txt"), b"x").unwrap();
        let (sink, rx) = collector();
        let w = Watcher::new(root, all(), sink).unwrap();
        w.focus(&[inner.clone(), sub.clone()]);

        fs::write(root.join("a.txt"), b"1").unwrap();
        expect_named(&rx, root, "create");
        settle(&rx);

        fs::write(root.join("a.txt"), b"22").unwrap();
        expect_named(&rx, root, "write");
        settle(&rx);

        fs::rename(root.join("a.txt"), root.join("b.txt")).unwrap();
        expect_named(&rx, root, "rename");
        settle(&rx);

        fs::remove_file(root.join("b.txt")).unwrap();
        expect_named(&rx, root, "delete");
        settle(&rx);

        // A change inside a sub-folder names the sub-folder, not the root.
        // Windows also reports the sub-folder's own change (its time), in
        // the folder above it, so `sub` may be named too, but never `root`.
        fs::write(inner.join("old.txt"), b"changed").unwrap();
        let mut seen = expect_named(&rx, &inner, "write in a sub-folder");
        seen.extend(collect_until_quiet(&rx, QUIET));
        assert!(
            !seen.iter().any(|b| names(b, root)),
            "the root was named for a change inside {}: {seen:?}",
            inner.display()
        );
        #[cfg(not(windows))]
        assert!(!seen.iter().any(|b| names(b, &sub)), "{seen:?}");

        // A file created in the sub-folder names the sub-folder too.
        settle(&rx);
        fs::write(inner.join("new.txt"), b"n").unwrap();
        expect_named(&rx, &inner, "create in a sub-folder");
        drop(w);
    }

    #[test]
    fn ten_thousand_writes_in_one_folder_become_few_batches_naming_it() {
        let tmp = TempDir::new();
        let root = tmp.path();
        let hot = root.join("hot");
        fs::create_dir(&hot).unwrap();
        for i in 0..100 {
            fs::write(hot.join(format!("f{i:02}.txt")), b"0").unwrap();
        }
        let (sink, rx) = collector();
        let w = Watcher::new(root, all(), sink).unwrap();
        w.focus(std::slice::from_ref(&hot));

        let started = Instant::now();
        for i in 0..10_000u32 {
            fs::write(hot.join(format!("f{:02}.txt", i % 100)), i.to_le_bytes()).unwrap();
        }
        let writing = started.elapsed();
        let seen = collect_until_quiet(&rx, ARRIVES);
        drop(w);

        assert!(!seen.is_empty(), "10,000 writes gave no batch");
        // Windows also reports `hot`'s own change (its time) in the root.
        let allowed: &[&Path] = if cfg!(windows) {
            &[&hot, root]
        } else {
            &[&hot]
        };
        for batch in &seen {
            match batch {
                WatchBatch::Rescan => {}
                WatchBatch::Dirs(dirs) => {
                    assert!(dirs.contains(&hot) || cfg!(windows), "{seen:?}");
                    assert!(
                        dirs.iter().all(|d| allowed.contains(&d.as_path())),
                        "{seen:?}"
                    );
                }
            }
        }
        assert!(seen.iter().any(|b| names(b, &hot)), "{seen:?}");
        // One batch per flush at most, while writing and once after.
        let flushes = writing.as_millis() / WATCH_FLUSH.as_millis() + 2;
        assert!(
            seen.len() as u128 <= flushes,
            "{} batches for {writing:?} of writing",
            seen.len()
        );
    }

    #[test]
    fn changes_in_more_folders_than_a_batch_holds_become_rescan() {
        let tmp = TempDir::new();
        let root = tmp.path();
        let dirs: Vec<PathBuf> = (0..60).map(|i| root.join(format!("d{i:02}"))).collect();
        for dir in &dirs {
            fs::create_dir(dir).unwrap();
            fs::write(dir.join("f.txt"), b"0").unwrap();
        }
        let (sink, rx) = collector();
        let config = Config {
            max_dirs: 20,
            ..Config::standard()
        };
        let w = Watcher::with_config(root, all(), sink, config).unwrap();
        w.focus(&dirs);
        for dir in &dirs {
            fs::write(dir.join("f.txt"), b"1").unwrap();
        }
        let deadline = Instant::now() + ARRIVES + WATCH_FLUSH;
        let mut seen = Vec::new();
        while !seen.contains(&WatchBatch::Rescan) {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(batch) => seen.push(batch),
                Err(_) => panic!("no Rescan for 60 folders past a limit of 20: {seen:?}"),
            }
        }
        for batch in &seen {
            if let WatchBatch::Dirs(d) = batch {
                assert!(d.len() <= 20, "{} folders in one batch", d.len());
            }
        }
        drop(w);
    }

    #[test]
    fn ten_thousand_changes_in_refused_folders_send_nothing() {
        let tmp = TempDir::new();
        let root = tmp.path();
        let noise = root.join("noise");
        let quiet = root.join("quiet");
        fs::create_dir(&noise).unwrap();
        fs::create_dir(&quiet).unwrap();
        let asked = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&asked);
        let only = quiet.clone();
        let (sink, rx) = collector();
        let w = Watcher::new(
            root,
            Box::new(move |dir| {
                counter.fetch_add(1, Ordering::Relaxed);
                dir == only
            }),
            sink,
        )
        .unwrap();
        w.focus(&[noise.clone(), quiet.clone()]);
        for i in 0..10_000u32 {
            fs::write(noise.join(format!("f{:03}.txt", i % 500)), i.to_le_bytes()).unwrap();
        }
        // No batch within 2 s after the last change.
        let got = rx.recv_timeout(ARRIVES);
        assert!(got.is_err(), "a refused folder produced {got:?}");
        assert!(
            asked.load(Ordering::Relaxed) > 0,
            "no event reached `wanted`, so the test proved nothing"
        );
        // The watcher still reports a folder it wants.
        fs::write(quiet.join("f.txt"), b"q").unwrap();
        expect_named(&rx, &quiet, "a wanted folder after the noise");
        drop(w);
    }

    #[test]
    fn dropping_the_watcher_stops_its_thread() {
        let tmp = TempDir::new();
        let token = Arc::new(());
        let held = Arc::clone(&token);
        let w = Watcher::new(
            tmp.path(),
            all(),
            Box::new(move |_| {
                let _keep = &held;
            }),
        )
        .unwrap();
        assert_eq!(Arc::strong_count(&token), 2);
        fs::write(tmp.path().join("f.txt"), b"x").unwrap();
        drop(w);
        // The sink lives on the flush thread: it is gone once the thread is.
        assert_eq!(Arc::strong_count(&token), 1);
    }

    #[test]
    fn dropping_the_watcher_from_inside_sink_does_not_deadlock() {
        let tmp = TempDir::new();
        let slot: Arc<Mutex<Option<Watcher>>> = Arc::new(Mutex::new(None));
        let in_sink = Arc::clone(&slot);
        let token = Arc::new(());
        let held = Arc::clone(&token);
        let (done_tx, done_rx) = mpsc::channel();
        let w = Watcher::new(
            tmp.path(),
            all(),
            Box::new(move |_| {
                let _keep = &held;
                let w = lock(&in_sink).take();
                drop(w);
                let _ = done_tx.send(());
            }),
        )
        .unwrap();
        *lock(&slot) = Some(w);
        fs::write(tmp.path().join("f.txt"), b"x").unwrap();
        done_rx
            .recv_timeout(ARRIVES + Duration::from_secs(1))
            .expect("the sink never ran, or the drop inside it hung");
        assert!(lock(&slot).is_none());
        let deadline = Instant::now() + ARRIVES;
        while Arc::strong_count(&token) > 1 {
            assert!(
                Instant::now() < deadline,
                "the flush thread outlived the watcher"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn panicking_sink_and_wanted_do_not_stop_the_watcher() {
        let tmp = TempDir::new();
        let root = tmp.path();
        let wanted_calls = Arc::new(AtomicUsize::new(0));
        let sink_calls = Arc::new(AtomicUsize::new(0));
        let wc = Arc::clone(&wanted_calls);
        let sc = Arc::clone(&sink_calls);
        let (tx, rx) = mpsc::channel();
        let w = Watcher::new(
            root,
            Box::new(move |_| {
                if wc.fetch_add(1, Ordering::SeqCst) == 0 {
                    panic!("the first `wanted` call panics");
                }
                true
            }),
            Box::new(move |batch| {
                if sc.fetch_add(1, Ordering::SeqCst) == 0 {
                    panic!("the first `sink` call panics");
                }
                let _ = tx.send(batch);
            }),
        )
        .unwrap();
        let mut got = None;
        for i in 0..8 {
            fs::write(root.join(format!("f{i}.txt")), b"x").unwrap();
            if let Ok(batch) = rx.recv_timeout(Duration::from_millis(1500)) {
                got = Some(batch);
                break;
            }
        }
        drop(w);
        assert!(
            matches!(&got, Some(b) if names(b, root)),
            "no batch after the panics: {got:?}"
        );
        assert!(wanted_calls.load(Ordering::SeqCst) >= 2);
        assert!(sink_calls.load(Ordering::SeqCst) >= 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn focus_keeps_at_most_the_limit_and_swaps_watches() {
        let tmp = TempDir::new();
        let root = tmp.path();
        let dirs: Vec<PathBuf> = (0..10).map(|i| root.join(format!("d{i}"))).collect();
        for dir in &dirs {
            fs::create_dir(dir).unwrap();
        }
        let (sink, rx) = collector();
        let config = Config {
            max_watches: 3,
            ..Config::standard()
        };
        assert_eq!(Config::standard().max_watches, MAX_LINUX_WATCHES);
        let w = Watcher::with_config(root, all(), sink, config).unwrap();
        let watched = |w: &Watcher| {
            let mut v: Vec<PathBuf> = lock(&w.watches).extra.iter().cloned().collect();
            v.sort();
            v
        };

        w.focus(&dirs);
        assert_eq!(watched(&w), dirs[..3].to_vec());

        // Repeats, the root, folders outside it and missing folders are
        // skipped; the first three that can be watched are kept.
        let outside = std::env::temp_dir();
        let mut asked = vec![
            dirs[5].clone(),
            dirs[5].clone(),
            root.to_path_buf(),
            outside,
            root.join("missing"),
        ];
        asked.extend(dirs[6..].iter().cloned());
        w.focus(&asked);
        assert_eq!(
            watched(&w),
            vec![dirs[5].clone(), dirs[6].clone()],
            "the limit counts the folders asked for, a missing one included"
        );

        w.focus(&dirs[5..]);
        assert_eq!(watched(&w), dirs[5..8].to_vec());

        // A watched folder reports; one that left does not.
        fs::write(dirs[0].join("f.txt"), b"x").unwrap();
        fs::write(dirs[5].join("f.txt"), b"x").unwrap();
        let seen = expect_named(&rx, &dirs[5], "a focused folder");
        assert!(!seen.iter().any(|b| names(b, &dirs[0])), "{seen:?}");

        w.focus(&[]);
        assert!(watched(&w).is_empty());
        drop(w);
    }

    // --- timing -------------------------------------------------------------

    #[test]
    #[ignore = "timing; run in release with --ignored --nocapture"]
    #[allow(clippy::print_stdout)]
    fn one_flush_of_1000_folders_into_invalidate() {
        const RUNS: usize = 3;
        fn median(mut runs: Vec<Duration>) -> Duration {
            runs.sort_unstable();
            runs[runs.len() / 2]
        }
        let mut invalidate = Vec::with_capacity(RUNS);
        let mut baseline = Vec::with_capacity(RUNS);
        let mut folders = 0;
        for _ in 0..RUNS {
            let tmp = TempDir::new();
            let root = tmp.path();
            let dirs: Vec<PathBuf> = (0..1_000)
                .map(|i| {
                    root.join(format!("g{:02}", i / 100))
                        .join(format!("d{i:03}"))
                })
                .collect();
            for dir in &dirs {
                fs::create_dir_all(dir).unwrap();
                for f in 0..10 {
                    fs::write(dir.join(format!("f{f}.txt")), [b'x'; 16]).unwrap();
                }
            }
            let sizes = HostSizes::new(root.to_path_buf(), Box::new(|| {}));
            sizes.request(root, Priority::Visible);
            let start = Instant::now();
            while !matches!(sizes.state(root), SizeState::Exact(_)) {
                assert!(start.elapsed() < Duration::from_secs(60), "never Exact");
                thread::sleep(Duration::from_millis(2));
            }
            for dir in &dirs {
                fs::write(dir.join("new.txt"), [b'y'; 32]).unwrap();
            }
            let batch = WatchBatch::Dirs(dirs.clone());
            let started = Instant::now();
            if let WatchBatch::Dirs(changed) = &batch {
                for dir in changed {
                    sizes.invalidate(dir);
                }
            }
            invalidate.push(started.elapsed());

            // Baseline: the bare directory reads the same folders need.
            let started = Instant::now();
            let mut bytes = 0u64;
            for dir in &dirs {
                for entry in fs::read_dir(dir).unwrap() {
                    bytes += entry.unwrap().metadata().unwrap().len();
                }
            }
            baseline.push(started.elapsed());
            std::hint::black_box(bytes);
            sizes.shutdown();
            folders = dirs.len();
        }
        let invalidate = median(invalidate);
        let baseline = median(baseline);
        let budget = Duration::from_millis(200);
        println!(
            "one_flush_of_1000_folders_into_invalidate: median {:.1} ms of {RUNS} runs for {folders} folders ({:.1} us per folder); budget 200 ms: {}",
            invalidate.as_secs_f64() * 1e3,
            invalidate.as_secs_f64() * 1e6 / folders as f64,
            if invalidate <= budget {
                "met"
            } else {
                "MISSED"
            },
        );
        println!(
            "baseline read_dir + DirEntry::metadata over the same {folders} folders: median {:.1} ms of {RUNS} runs",
            baseline.as_secs_f64() * 1e3,
        );
    }
}
