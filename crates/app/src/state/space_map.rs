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

//! The data of the space map (`r1-redesign.md` 3.5): the tree of sizes of the folder shown, two
//! levels deep, read off the UI thread from the size table of the volume (`IndexSizes`) or the
//! measured tree of the host (`HostSizes`).
//!
//! It works only while the map is on screen. A tree is made again when the folder changes, when
//! the size table of the volume is replaced, and, while a host folder is still being measured,
//! at most twice a second. A result for a folder the user has left is dropped.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use disk_atlas_lib::fs::{FileSystem, NodeId};
use disk_atlas_lib::search::index::Index;
use disk_atlas_lib::sizes::{HostSizes, IndexSizes, MapNode, MapTarget, host_map, index_map};
use gpui::{Context, Entity, SharedString, Subscription, Task};

use super::listing::Listing;
use super::navigation::{Crumb, Location, Navigation};
use super::search::{IndexState, Search};
use super::sizes::Sizes;
use super::sources::{SourceId, SourceKind, Sources};
use super::view::ViewState;

/// Levels of the map below the folder shown.
pub const DEPTH: u8 = 2;

/// While a host folder is being measured, its tree is read again no more often than this.
const REBUILD_EVERY: Duration = Duration::from_millis(500);

/// A tree for one folder.
#[derive(Debug)]
pub struct Loaded {
    /// The source and folder it is for.
    pub source: SourceId,
    pub node: NodeId,
    /// The folder, with two levels below it.
    pub root: MapNode,
}

/// What the map has to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapState {
    /// The map is not on screen.
    Idle,
    /// The sizes are being read or measured.
    Loading,
    /// A tree is there (and may be replaced by a better one).
    Ready,
    /// The volume's file list has not been read: the map offers to read it.
    NeedsIndex(SourceId),
    /// There is nothing to draw, and this says why.
    Unavailable(SharedString),
}

/// What a tree was made from: the size table it read, or nothing for a host folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Key {
    source: SourceId,
    node: NodeId,
    /// The address of the `IndexSizes` it read; 0 for the host.
    table: usize,
}

/// The tree of sizes of the folder shown.
pub struct SpaceMap {
    sources: Entity<Sources>,
    navigation: Entity<Navigation>,
    search: Entity<Search>,
    sizes: Entity<Sizes>,
    view: Entity<ViewState>,
    loaded: Option<Arc<Loaded>>,
    state: MapState,
    key: Option<Key>,
    /// The tree for `key` has no unmeasured folder in it: nothing more to read.
    settled: bool,
    cancel: Arc<AtomicBool>,
    task: Option<Task<()>>,
    last_build: Option<Instant>,
    retry: Option<Task<()>>,
    /// The name of the one selected row of the list, for the region outlined in `pin`.
    selected: Option<SharedString>,
    resolving: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl SpaceMap {
    /// Follows the view, the folder shown, the size tables and the selection.
    pub fn new(
        sources: Entity<Sources>,
        navigation: Entity<Navigation>,
        search: Entity<Search>,
        sizes: Entity<Sizes>,
        listing: Entity<Listing>,
        view: Entity<ViewState>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.observe(&view, |this, _, cx| this.refresh(false, cx)),
            cx.observe(&navigation, |this, _, cx| this.refresh(false, cx)),
            cx.observe(&sizes, |this, _, cx| this.refresh(false, cx)),
            cx.observe(&sources, |this, _, cx| this.refresh(false, cx)),
            cx.observe(&listing, |this, listing, cx| {
                let name = {
                    let listing = listing.read(cx);
                    (listing.selected_count() == 1)
                        .then(|| listing.primary().map(|entry| entry.name.to_owned()))
                        .flatten()
                };
                let name = name.map(SharedString::from);
                if name != this.selected {
                    this.selected = name;
                    cx.notify();
                }
            }),
        ];
        let mut map = Self {
            sources,
            navigation,
            search,
            sizes,
            view,
            loaded: None,
            state: MapState::Idle,
            key: None,
            settled: false,
            cancel: Arc::new(AtomicBool::new(false)),
            task: None,
            last_build: None,
            retry: None,
            selected: None,
            resolving: None,
            _subscriptions: subscriptions,
        };
        map.refresh(false, cx);
        map
    }

    /// The tree last made.
    pub fn loaded(&self) -> Option<&Arc<Loaded>> {
        self.loaded.as_ref()
    }

    /// What there is to say.
    pub fn state(&self) -> &MapState {
        &self.state
    }

    /// The name of the selected row, when one is selected.
    pub fn selected_name(&self) -> Option<&SharedString> {
        self.selected.as_ref()
    }

    /// Asks the index of the source shown to be built.
    pub fn read_file_list(&mut self, source: SourceId, cx: &mut Context<Self>) {
        self.search
            .update(cx, |search, cx| search.calculate_sizes(source, cx));
        self.state = MapState::Loading;
        cx.notify();
    }

    fn stop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.task = None;
        self.retry = None;
    }

    /// Looks at what is shown and starts a read when the tree is out of date. `force` reads
    /// again even when nothing is known to have changed.
    fn refresh(&mut self, force: bool, cx: &mut Context<Self>) {
        if !self.view.read(cx).shows_map() {
            if self.state != MapState::Idle {
                self.stop();
                self.state = MapState::Idle;
                self.key = None;
                cx.notify();
            }
            return;
        }
        let Some(location) = self.navigation.read(cx).current().cloned() else {
            return;
        };
        let source_id = location.source();
        let Some(source) = self.sources.read(cx).source(source_id) else {
            return;
        };
        let kind = source.kind;
        let browsable = source.availability.is_browsable();
        let fs = Arc::clone(&source.fs);
        // Free space is the volume's: shown at the top of a volume or a drive, not of a folder
        // such as Home.
        let is_root = location.crumbs().len() == 1
            && self
                .sources
                .read(cx)
                .host_root(source_id)
                .is_none_or(crate::components::storage_bar::is_drive_root);

        let input = match kind {
            SourceKind::WholeDisk { .. } => {
                self.unavailable("This disk cannot be read, so there is no map of it.", cx);
                return;
            }
            SourceKind::Volume { .. } if !browsable => {
                self.unavailable("This volume cannot be read, so there is no map of it.", cx);
                return;
            }
            SourceKind::Volume { .. } => {
                let table = self.sizes.read(cx).table(source_id);
                let index = self.search.read(cx).index(source_id);
                match (index, table) {
                    (Some(index), Some(table)) => Input::Index(index, table),
                    _ => {
                        let state = match self.search.read(cx).index_state(source_id) {
                            Some(IndexState::Failed(why)) => MapState::Unavailable(why.clone()),
                            Some(
                                IndexState::Absent
                                | IndexState::Building { .. }
                                | IndexState::Ready(_),
                            ) => MapState::Loading,
                            None => MapState::NeedsIndex(source_id),
                        };
                        self.waiting(state, cx);
                        return;
                    }
                }
            }
            SourceKind::Host => {
                let Some(root) = self.sources.read(cx).host_root(source_id) else {
                    self.unavailable("This folder is not on a drive that can be mapped.", cx);
                    return;
                };
                let mut path = root.to_path_buf();
                for crumb in location.crumbs().iter().skip(1) {
                    path.push(crumb.name.as_ref());
                }
                match self.sizes.read(cx).host(source_id) {
                    Some(sizes) => Input::Host(sizes, path),
                    None => {
                        self.waiting(MapState::Loading, cx);
                        return;
                    }
                }
            }
        };

        let key = Key {
            source: source_id,
            node: location.node(),
            table: match &input {
                Input::Index(_, table) => Arc::as_ptr(table) as usize,
                Input::Host(..) => 0,
            },
        };
        let same = self.key == Some(key);
        if same && !force && (self.task.is_some() || self.settled || self.retry.is_some()) {
            return;
        }
        if same && !force {
            // Measuring goes on: read again, but not more often than `REBUILD_EVERY`.
            if let Some(at) = self.last_build
                && at.elapsed() < REBUILD_EVERY
            {
                let wait = REBUILD_EVERY - at.elapsed();
                self.retry = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(wait).await;
                    this.update(cx, |this, cx| {
                        this.retry = None;
                        this.refresh(true, cx);
                    })
                    .ok();
                }));
                return;
            }
        }
        if !same {
            self.settled = false;
            if self
                .loaded
                .as_ref()
                .is_none_or(|l| (l.source, l.node) != (key.source, key.node))
            {
                self.state = MapState::Loading;
            }
        }
        self.key = Some(key);
        self.start(key, input, fs, is_root, cx);
    }

    fn unavailable(&mut self, why: &'static str, cx: &mut Context<Self>) {
        self.stop();
        self.key = None;
        self.loaded = None;
        self.state = MapState::Unavailable(why.into());
        cx.notify();
    }

    fn waiting(&mut self, state: MapState, cx: &mut Context<Self>) {
        self.stop();
        self.key = None;
        if self.state != state {
            self.state = state;
            cx.notify();
        }
    }

    fn start(
        &mut self,
        key: Key,
        input: Input,
        fs: Arc<dyn FileSystem>,
        is_root: bool,
        cx: &mut Context<Self>,
    ) {
        self.cancel.store(true, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Arc::clone(&cancel);
        self.last_build = Some(Instant::now());
        self.task = Some(cx.spawn(async move |this, cx| {
            let free = if is_root { fs.info().free_bytes } else { None };
            let node = key.node;
            let made = cx
                .background_executor()
                .spawn(async move { build(&input, node, free, &cancel) })
                .await;
            this.update(cx, |this, cx| this.arrived(key, made, cx)).ok();
        }));
    }

    /// A tree was made for `key`; it is dropped when the user has moved on.
    fn arrived(&mut self, key: Key, made: Option<MapNode>, cx: &mut Context<Self>) {
        if self.key != Some(key) {
            return;
        }
        self.task = None;
        let Some(root) = made else {
            return;
        };
        self.settled = !root.item.lower_bound;
        let changed = match &self.loaded {
            Some(old) => (old.source, old.node) != (key.source, key.node) || old.root != root,
            None => true,
        };
        if changed {
            self.loaded = Some(Arc::new(Loaded {
                source: key.source,
                node: key.node,
                root,
            }));
        }
        self.state = MapState::Ready;
        cx.notify();
        if !self.settled {
            self.retry = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(REBUILD_EVERY).await;
                this.update(cx, |this, cx| {
                    this.retry = None;
                    this.refresh(true, cx);
                })
                .ok();
            }));
        }
    }

    /// The region at `path` of the tree shown: the folder to go into when it is clicked, or the
    /// holder of a file inside a folder.
    fn goal(&self, path: (u32, Option<u32>)) -> Option<MapTarget> {
        let loaded = self.loaded.as_ref()?;
        let first = loaded.root.children.get(path.0 as usize)?;
        match (path.1, first.item.is_folder) {
            (None, true) => Some(first.item.target.clone()),
            (None, false) => None,
            (Some(second), _) => {
                let second = first.children.get(second as usize)?;
                if second.item.is_folder || matches!(second.item.target, MapTarget::HostFiles(_)) {
                    Some(second.item.target.clone())
                } else {
                    Some(first.item.target.clone())
                }
            }
        }
    }

    /// Whether clicking the region at `path` goes anywhere.
    pub fn can_enter(&self, path: (u32, Option<u32>)) -> bool {
        self.goal(path)
            .is_some_and(|t| !matches!(t, MapTarget::Free | MapTarget::Merged))
    }

    /// Goes into the region at `path`: the folder becomes the one shown, `then` runs after.
    pub fn enter(&mut self, path: (u32, Option<u32>), open_list: bool, cx: &mut Context<Self>) {
        let Some(target) = self.goal(path) else {
            if open_list {
                self.view.update(cx, |view, cx| {
                    view.set_mode(super::view::ViewMode::List, cx)
                });
            }
            return;
        };
        let Some(location) = self.navigation.read(cx).current().cloned() else {
            return;
        };
        let root = self
            .sources
            .read(cx)
            .host_root(location.source())
            .map(PathBuf::from);
        let index = self.search.read(cx).index(location.source());
        let base: Vec<Crumb> = location.crumbs().to_vec();
        let work = cx.background_executor().spawn({
            let fs = Arc::clone(location.fs());
            async move {
                crumbs_for(
                    &target,
                    &base,
                    fs.as_ref(),
                    root.as_deref(),
                    index.as_deref(),
                )
            }
        });
        self.resolving = Some(cx.spawn(async move |this, cx| {
            let crumbs = work.await;
            this.update(cx, |this, cx| {
                let Some(to) = crumbs.and_then(|crumbs| location.with_crumbs(crumbs)) else {
                    return;
                };
                let view = this.view.clone();
                view.update(cx, |view, cx| {
                    if open_list {
                        view.set_mode(super::view::ViewMode::List, cx);
                        this.navigation
                            .update(cx, |navigation, cx| navigation.navigate(to, cx));
                    } else {
                        view.navigate_from_map(to, cx);
                    }
                });
            })
            .ok();
        }));
    }

    /// Goes to the enclosing folder.
    pub fn leave(&mut self, cx: &mut Context<Self>) {
        let parent = self
            .navigation
            .read(cx)
            .current()
            .and_then(Location::parent);
        if let Some(parent) = parent {
            self.view
                .update(cx, |view, cx| view.navigate_from_map(parent, cx));
        }
    }
}

/// What a tree is read from.
enum Input {
    Index(Arc<Index>, Arc<IndexSizes>),
    Host(Arc<HostSizes>, PathBuf),
}

/// The tree of `node`, with the volume's free space at the top level when `free` is known.
/// Blocking: the background executor.
fn build(input: &Input, node: NodeId, free: Option<u64>, cancel: &AtomicBool) -> Option<MapNode> {
    let tree = match input {
        Input::Index(index, table) => {
            let entry = table.folder_entry(node)?;
            index_map(index, table, entry, DEPTH, cancel)?
        }
        Input::Host(sizes, path) => host_map(sizes, path, DEPTH, cancel)?,
    };
    Some(match free {
        Some(free) => tree.with_free(free),
        None => tree,
    })
}

/// The path of crumbs to the folder `target` stands for, below the folder `base` ends at (which
/// is always an ancestor of it: the map shows two levels below it). Blocking.
fn crumbs_for(
    target: &MapTarget,
    base: &[Crumb],
    fs: &dyn FileSystem,
    host_root: Option<&std::path::Path>,
    index: Option<&Index>,
) -> Option<Vec<Crumb>> {
    match target {
        MapTarget::Index(entry) => {
            let chain = index?.crumbs(*entry);
            if chain.is_empty() {
                return None;
            }
            Some(
                chain
                    .into_iter()
                    .enumerate()
                    .map(|(depth, (node, name))| Crumb {
                        node,
                        // The first crumb is the volume's own name, as `Location::root` has it.
                        name: if depth == 0 {
                            base.first()
                                .map_or_else(|| name.clone().into(), |c| c.name.clone())
                        } else {
                            name.into()
                        },
                    })
                    .collect(),
            )
        }
        MapTarget::Host(path) | MapTarget::HostFiles(path) => {
            let relative = path.strip_prefix(host_root?).ok()?;
            let mut crumbs = vec![base.first()?.clone()];
            let mut dir = crumbs[0].node;
            for part in relative.components() {
                let name = part.as_os_str().to_str()?;
                let node = fs.lookup(dir, name).ok()??;
                crumbs.push(Crumb {
                    node,
                    name: name.to_owned().into(),
                });
                dir = node;
            }
            Some(crumbs)
        }
        MapTarget::Free | MapTarget::Merged => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_atlas_lib::fs::native::NativeFs;

    fn base_for(fs: &NativeFs) -> Vec<Crumb> {
        vec![Crumb {
            node: fs.root(),
            name: "Home".into(),
        }]
    }

    #[test]
    fn a_host_folder_resolves_to_crumbs_below_the_root() {
        let root = std::env::temp_dir().join(format!("disk-atlas-spacemap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a").join("b")).unwrap();
        let fs = NativeFs::new(&root, "Home").unwrap();
        let base = base_for(&fs);
        let target = MapTarget::Host(root.join("a").join("b"));
        let crumbs = crumbs_for(&target, &base, &fs, Some(&root), None).unwrap();
        let names: Vec<&str> = crumbs.iter().map(|c| c.name.as_ref()).collect();
        assert_eq!(names, ["Home", "a", "b"]);
        // A folder that is not there, and one outside the root, give nothing.
        let gone = MapTarget::Host(root.join("a").join("zzz"));
        assert!(crumbs_for(&gone, &base, &fs, Some(&root), None).is_none());
        let outside = MapTarget::Host(std::env::temp_dir());
        assert!(crumbs_for(&outside, &base, &fs, Some(&root), None).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn free_space_and_merged_tiles_go_nowhere() {
        let root =
            std::env::temp_dir().join(format!("disk-atlas-spacemap2-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let fs = NativeFs::new(&root, "Home").unwrap();
        let base = base_for(&fs);
        assert!(crumbs_for(&MapTarget::Free, &base, &fs, Some(&root), None).is_none());
        assert!(crumbs_for(&MapTarget::Merged, &base, &fs, Some(&root), None).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
