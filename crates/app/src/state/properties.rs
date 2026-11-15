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

//! The state of the Properties window (`r10-everyday.md` section 7, R18.2b): which item, volume
//! or disk a window is about, how each of its sections stands, and the loaders that fill them.
//!
//! Every read of a filesystem, a size table or the host runs on the background executor; each
//! section is its own task and lands when it is done. A result carries the generation of the load
//! that started it and is dropped when the window has moved on ([`Sheet::apply`]). The section
//! builders are the pure functions of `state/facts.rs`.
//!
//! [`PropertiesWindows`] keeps the open windows so that asking again for the same target brings
//! its window forward, and refuses a ninth window.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use disk_atlas_lib::error::Error;
use disk_atlas_lib::fs::{
    EntryFlags, FileSystem, Metadata, NodeId, NodeKind, Stream, VolumeIdentity, VolumeInfo,
};
use disk_atlas_lib::mac::xattr::read_attributes;
use disk_atlas_lib::search::Index;
use disk_atlas_lib::search::kind::classify_entry;
use disk_atlas_lib::sizes::{ClassBytes, IndexSizes, Totals};
use gpui::{AnyWindowHandle, App, Context, Entity, Global, SharedString, Subscription, Task};

use crate::format;
use crate::state::checksum::{self, Outcome, PROGRESS_INTERVAL, Phase, Progress};
use crate::state::facts::{
    DiskDescription, FactLink, HostFlags, ItemFacts, LISTED_LARGEST, LargestItem, PartitionFacts,
    PartitionRow, PieceStrip, SectionId, SectionState, copy_everything, disk_sections,
    general_item, inside_section, mac_item, on_disk_item, permissions_item, scheme_name,
    several_items, subtitle, volume_sections, xattrs_with_mac,
};
use crate::state::listing::{Listing, Reach};
use crate::state::navigation::{Crumb, Location, Navigation};
use crate::state::search::Search;
use crate::state::settings::{SettingValue, Settings};
use crate::state::sizes::Sizes;
use crate::state::sources::{
    Availability, Disk, DiskId, Source, SourceGroup, SourceId, SourceKind, Sources,
};
use crate::theme::Glyph;
use crate::views::aux_window;

/// The most Properties windows open at once (`r10-everyday.md` section 7).
pub const MAX_PROPERTIES_WINDOWS: usize = 8;

/// The settings key the collapsed sections are remembered under: a comma list of section keys.
const COLLAPSED_KEY: &str = "properties.collapsed";

/// What a Properties window is about.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PropertiesTarget {
    /// One file or folder.
    Item {
        source: SourceId,
        /// The folder it is in.
        parent: NodeId,
        node: NodeId,
        name: SharedString,
    },
    /// Several items of one folder, one window for all of them.
    Items {
        source: SourceId,
        parent: NodeId,
        nodes: Arc<[NodeId]>,
    },
    /// A volume: the top of a source.
    Volume { source: SourceId },
    /// A physical disk or an opened image. The design says `SourceId`; `Sources` names disks with
    /// a [`DiskId`].
    Disk { disk: DiskId },
}

impl PropertiesTarget {
    /// The source the target lies in; `None` for a disk.
    pub fn source(&self) -> Option<SourceId> {
        match self {
            Self::Item { source, .. } | Self::Items { source, .. } | Self::Volume { source } => {
                Some(*source)
            }
            Self::Disk { .. } => None,
        }
    }
}

/// The sections a target gets, in the order the window draws them. `host` is true for a place on
/// this computer. `folder` says whether an item is a folder; the window knows it from the row it
/// was opened on and confirms it when the metadata arrives. It is ignored for other targets.
///
/// A section that turns out to have nothing to show ends up `Absent` and is not drawn.
pub fn sections_for(target: &PropertiesTarget, host: bool, folder: bool) -> Vec<SectionId> {
    use SectionId::{Checksum, General, Inside, Mac, OnDisk, Permissions, Volume, Xattrs};
    match target {
        // The host's folder sizes are R14: "What is inside" waits there, listed but empty.
        PropertiesTarget::Item { .. } if host && folder => vec![General, Inside, Permissions],
        // A file also gets a Checksum; the metadata drops it for a link, a device or a socket.
        PropertiesTarget::Item { .. } if host => vec![General, Permissions, Checksum],
        PropertiesTarget::Item { .. } if folder => {
            vec![General, Inside, Mac, Permissions, Xattrs]
        }
        PropertiesTarget::Item { .. } => {
            vec![General, Mac, OnDisk, Permissions, Xattrs, Checksum]
        }
        PropertiesTarget::Items { .. } => vec![General, Inside],
        PropertiesTarget::Volume { .. } => {
            let mut sections = vec![General, Volume];
            if !host {
                sections.push(OnDisk);
            }
            sections.push(Inside);
            sections
        }
        PropertiesTarget::Disk { .. } => vec![SectionId::Disk],
    }
}

/// The window's title: "<name> Properties", or "Properties" when there is no single name.
pub fn title_text(name: Option<&str>) -> SharedString {
    match name {
        Some(name) if !name.is_empty() => format!("{name} Properties").into(),
        _ => "Properties".into(),
    }
}

// ---------------------------------------------------------------------------------------------
// The sheet: what the window shows, as plain data.

/// How each section of one window stands, and the generation of the load they belong to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sheet {
    /// Bumped by every load; a result of an older one is dropped.
    pub generation: u64,
    pub sections: Vec<(SectionId, SectionState)>,
    /// The file's pieces on its volume, for the On the disk section.
    pub strip: Option<PieceStrip>,
}

impl Sheet {
    /// Starts a load: a new generation, with every one of `ids` loading and the strip gone.
    /// Returns the generation.
    pub fn begin(&mut self, ids: &[SectionId]) -> u64 {
        self.generation += 1;
        self.sections = ids.iter().map(|&id| (id, SectionState::Loading)).collect();
        self.strip = None;
        self.generation
    }

    /// The sections become `ids`: one that stays keeps its state, a new one starts loading.
    pub fn relist(&mut self, ids: &[SectionId]) {
        let old = std::mem::take(&mut self.sections);
        self.sections = ids
            .iter()
            .map(|&id| {
                let state = old
                    .iter()
                    .find(|(held, _)| *held == id)
                    .map_or(SectionState::Loading, |(_, state)| state.clone());
                (id, state)
            })
            .collect();
    }

    /// Puts `state` into section `id`. False, and nothing changes, when `generation` is not the
    /// current one or the window has no such section.
    pub fn apply(&mut self, generation: u64, id: SectionId, state: SectionState) -> bool {
        if generation != self.generation {
            return false;
        }
        match self.sections.iter_mut().find(|(held, _)| *held == id) {
            Some((_, slot)) => {
                *slot = state;
                true
            }
            None => false,
        }
    }

    /// Applies `results`, then makes every section of `covers` that they did not mention `Absent`:
    /// a loader that builds several sections at once has nothing for the rest.
    pub fn settle(
        &mut self,
        generation: u64,
        results: Vec<(SectionId, SectionState)>,
        covers: &[SectionId],
    ) -> bool {
        if generation != self.generation {
            return false;
        }
        for &id in covers {
            if !results.iter().any(|(held, _)| *held == id) {
                self.apply(generation, id, SectionState::Absent);
            }
        }
        for (id, state) in results {
            self.apply(generation, id, state);
        }
        true
    }

    /// Moves the Checksum section to `phase`, keeping what it says about the fork. False, and
    /// nothing changes, when `generation` is not the current one or the window has no Checksum
    /// section that is showing one.
    pub fn set_checksum(&mut self, generation: u64, phase: Phase) -> bool {
        if generation != self.generation {
            return false;
        }
        match self
            .sections
            .iter_mut()
            .find(|(held, _)| *held == SectionId::Checksum)
        {
            Some((_, SectionState::Checksum(checksum))) => {
                checksum.phase = phase;
                true
            }
            _ => false,
        }
    }

    /// Where the Checksum section stands, when the window has one that is showing.
    pub fn checksum(&self) -> Option<&checksum::Checksum> {
        self.sections.iter().find_map(|(id, state)| match state {
            SectionState::Checksum(checksum) if *id == SectionId::Checksum => Some(checksum),
            _ => None,
        })
    }

    /// The state of section `id`.
    #[cfg(test)]
    pub fn state_of(&self, id: SectionId) -> Option<&SectionState> {
        self.sections
            .iter()
            .find(|(held, _)| *held == id)
            .map(|(_, state)| state)
    }

    /// "Copy everything": the window as text.
    pub fn as_text(&self, title: &str) -> String {
        copy_everything(title, &self.sections)
    }
}

/// Every section, in the order the window draws them.
const ORDER: [SectionId; 9] = [
    SectionId::General,
    SectionId::Inside,
    SectionId::Mac,
    SectionId::OnDisk,
    SectionId::Permissions,
    SectionId::Xattrs,
    SectionId::Checksum,
    SectionId::Volume,
    SectionId::Disk,
];

/// The word a section is stored under in the `properties.collapsed` setting.
fn section_key(id: SectionId) -> &'static str {
    match id {
        SectionId::General => "general",
        SectionId::Inside => "inside",
        SectionId::Mac => "mac",
        SectionId::OnDisk => "on-disk",
        SectionId::Permissions => "permissions",
        SectionId::Xattrs => "xattrs",
        SectionId::Checksum => "checksum",
        SectionId::Volume => "volume",
        SectionId::Disk => "disk",
    }
}

/// The sections a setting's text names; words it does not know are ignored.
pub fn parse_collapsed(text: &str) -> HashSet<SectionId> {
    text.split(',')
        .map(str::trim)
        .filter_map(|word| ORDER.into_iter().find(|&id| section_key(id) == word))
        .collect()
}

/// The text of `collapsed` for the setting: section keys in window order, comma-separated.
pub fn collapsed_text(collapsed: &HashSet<SectionId>) -> String {
    ORDER
        .into_iter()
        .filter(|id| collapsed.contains(id))
        .map(section_key)
        .collect::<Vec<_>>()
        .join(",")
}

// ---------------------------------------------------------------------------------------------
// The window registry, generic over the handle so its rules are tested without a window.

/// What asking for a window comes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opening<H> {
    /// A window for the same target is open: bring it forward.
    Focus(H),
    /// `MAX_PROPERTIES_WINDOWS` are open: open nothing and bring the newest forward.
    Full(H),
    /// Open a new window.
    Open,
}

/// The open windows, oldest first.
#[derive(Debug)]
pub struct Registry<H> {
    open: Vec<(PropertiesTarget, H)>,
}

impl<H> Default for Registry<H> {
    fn default() -> Self {
        Self { open: Vec::new() }
    }
}

impl<H: Copy + PartialEq> Registry<H> {
    /// Forgets the windows `live` does not vouch for (the ones the user closed).
    pub fn prune(&mut self, live: impl Fn(&H) -> bool) {
        self.open.retain(|(_, handle)| live(handle));
    }

    /// What to do for `target`: call [`Registry::prune`] first.
    pub fn decide(&self, target: &PropertiesTarget, max: usize) -> Opening<H> {
        if let Some((_, handle)) = self.open.iter().find(|(held, _)| held == target) {
            return Opening::Focus(*handle);
        }
        match self.open.last() {
            Some((_, newest)) if self.open.len() >= max => Opening::Full(*newest),
            _ => Opening::Open,
        }
    }

    /// Records a window that was opened for `target`.
    pub fn add(&mut self, target: PropertiesTarget, handle: H) {
        self.open.push((target, handle));
    }

    /// How many windows are recorded.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.open.len()
    }
}

/// What the requester knew when it asked, which the window cannot find out for itself.
#[derive(Clone, Default)]
pub struct Request {
    /// The folder the item is in, as a place the main window can go to. `None` when it is not
    /// known (a search hit on this computer): the window then has no "Where" row.
    pub from: Option<Location>,
    /// The item's path on this computer, for the Windows attributes.
    pub host_path: Option<PathBuf>,
    /// The item is a folder, as the row said; the metadata confirms it.
    pub folder: bool,
    /// The bytes of the selected rows, for several items.
    pub selection_total: Option<u64>,
}

/// The entities the loaders read; cloned into every window.
#[derive(Clone)]
pub struct PropertiesEnv {
    pub sources: Entity<Sources>,
    pub sizes: Entity<Sizes>,
    pub search: Entity<Search>,
    /// Where a link in a window takes the main window.
    pub navigation: Entity<Navigation>,
    pub request: Request,
}

impl PropertiesEnv {
    /// The environment of the main window's entities, with nothing known of any request yet.
    pub fn new(
        sources: Entity<Sources>,
        sizes: Entity<Sizes>,
        search: Entity<Search>,
        navigation: Entity<Navigation>,
    ) -> Self {
        Self {
            sources,
            sizes,
            search,
            navigation,
            request: Request::default(),
        }
    }
}

/// All open Properties windows, so a second request focuses the first.
#[derive(Default)]
pub struct PropertiesWindows {
    registry: Registry<AnyWindowHandle>,
    /// The main window's entities, kept for the menus that open a window.
    env: Option<PropertiesEnv>,
}

impl Global for PropertiesWindows {}

impl PropertiesWindows {
    /// Keeps the main window's entities, so [`PropertiesWindows::open_here`] can open a window
    /// from anywhere in the app. Call once when the workspace is made.
    pub fn install(env: PropertiesEnv, cx: &mut App) {
        cx.default_global::<Self>().env = Some(env);
    }

    /// Opens the window for `target` with what the requester knew, using the installed
    /// entities.
    pub fn open_here(target: PropertiesTarget, request: Request, cx: &mut App) {
        let Some(mut env) = cx
            .try_global::<Self>()
            .and_then(|windows| windows.env.clone())
        else {
            tracing::warn!("Properties was asked for before the workspace was made");
            return;
        };
        env.request = request;
        Self::open(target, env, cx);
    }

    /// Focuses the window for `target` or opens one. Refuses past [`MAX_PROPERTIES_WINDOWS`],
    /// bringing the newest forward.
    pub fn open(target: PropertiesTarget, env: PropertiesEnv, cx: &mut App) {
        let live = cx.windows();
        let decision = {
            let windows = cx.default_global::<Self>();
            windows.registry.prune(|handle| live.contains(handle));
            windows.registry.decide(&target, MAX_PROPERTIES_WINDOWS)
        };
        match decision {
            Opening::Focus(handle) => focus(handle, cx),
            Opening::Full(newest) => {
                // The app has no notice surface yet; the log stands in for "Close a Properties
                // window first."
                tracing::info!("Close a Properties window first.");
                focus(newest, cx);
            }
            Opening::Open => match crate::views::properties::open(target.clone(), env, cx) {
                Ok(handle) => cx.default_global::<Self>().registry.add(target, handle),
                Err(error) => tracing::error!(%error, "could not open the Properties window"),
            },
        }
    }
}

/// Brings a window forward.
fn focus(handle: AnyWindowHandle, cx: &mut App) {
    handle
        .update(cx, |_, window, _| window.activate_window())
        .ok();
}

// ---------------------------------------------------------------------------------------------
// What a request from the file list comes to.

/// What the main window's selection asks for.
pub enum Ask {
    /// Everything is known: open the window.
    Open(PropertiesTarget, Request),
    /// A hit on this computer: its real node is found by walking its names on the background
    /// executor ([`Walk::resolve`]), then the window opens.
    Walk(Walk),
}

/// A search hit on the host whose real node is not known yet.
pub struct Walk {
    pub source: SourceId,
    pub name: SharedString,
    pub reach: Reach,
    pub fs: Arc<dyn FileSystem>,
    pub request: Request,
}

impl Walk {
    /// Finds the node. Blocking: background executor only.
    pub fn resolve(&self) -> Result<Option<NodeId>, Error> {
        self.reach.resolve(self.fs.as_ref())
    }

    /// The target once the node is known. The folder the hit lies in is not known without
    /// walking its names again, so the parent is the node itself and the window has no "Where"
    /// row: the folder-less variant.
    pub fn target(&self, node: NodeId) -> PropertiesTarget {
        PropertiesTarget::Item {
            source: self.source,
            parent: node,
            node,
            name: self.name.clone(),
        }
    }
}

/// The path of an index entry on this computer: the host root, then the names below the index's
/// root (whose own name is the volume's).
fn host_path_of_crumbs(root: &Path, crumbs: &[(NodeId, String)]) -> PathBuf {
    let mut path = root.to_path_buf();
    for (_, name) in crumbs.iter().skip(1) {
        path.push(name);
    }
    path
}

/// The crumbs of the folder a hit lies in, as `Crumb`s with the root named as the source is
/// (the index holds the volume's own name). `None` for a hit with no folder above it.
fn parent_crumbs(path: &[(NodeId, String)], root_name: &SharedString) -> Option<Vec<Crumb>> {
    let (_, above) = path.split_last()?;
    if above.is_empty() {
        return None;
    }
    Some(
        above
            .iter()
            .enumerate()
            .map(|(depth, (node, name))| Crumb {
                node: *node,
                name: if depth == 0 {
                    root_name.clone()
                } else {
                    name.clone().into()
                },
            })
            .collect(),
    )
}

/// What Alt+Enter and Ctrl+I ask for: the selection of the file list, or the folder shown when
/// nothing is selected. `None` while there is no folder. Only reads memory.
pub fn ask_for_selection(
    listing: &Listing,
    search: &Search,
    sources: &Sources,
    cx: &App,
) -> Option<Ask> {
    let location = listing.location()?;
    let source = location.source();
    let selected = listing.selected_count();
    if selected == 0 {
        // The folder shown: an item of its parent, or the volume at the top.
        let from = location.parent();
        let request = Request {
            from: from.clone(),
            host_path: listing.host_folder_path(cx),
            folder: true,
            selection_total: None,
        };
        let target = match &from {
            Some(parent) => PropertiesTarget::Item {
                source,
                parent: parent.node(),
                node: location.node(),
                name: location.name().clone(),
            },
            None => PropertiesTarget::Volume { source },
        };
        return Some(Ask::Open(target, request));
    }
    if selected > 1 {
        let results = listing.in_results();
        let request = Request {
            from: (!results).then(|| location.clone()),
            host_path: None,
            folder: false,
            selection_total: listing.selection_total(),
        };
        let target = PropertiesTarget::Items {
            source,
            parent: location.node(),
            nodes: listing.selected_nodes(usize::MAX).into(),
        };
        return Some(Ask::Open(target, request));
    }
    let entry = listing.primary()?;
    let (name, node, folder) = (
        SharedString::from(entry.name.to_owned()),
        entry.node,
        entry.is_browsable(),
    );
    if !listing.in_results() {
        let host_path = listing
            .selected_positions(1)
            .first()
            .and_then(|&position| listing.host_path_of(position, cx));
        let request = Request {
            from: Some(location.clone()),
            host_path,
            folder,
            selection_total: None,
        };
        let target = PropertiesTarget::Item {
            source,
            parent: location.node(),
            node,
            name,
        };
        return Some(Ask::Open(target, request));
    }
    // A search hit: its folder comes from the index's path.
    let root_name = location.crumbs().first()?.name.clone();
    let reach = listing.primary_reach()?;
    match &reach {
        Reach::Node(node) => {
            let node = *node;
            let index = search.index(source)?;
            let path = index.crumbs(index.find_node(node)?);
            let from =
                parent_crumbs(&path, &root_name).and_then(|crumbs| location.with_crumbs(crumbs))?;
            let request = Request {
                from: Some(from.clone()),
                host_path: None,
                folder,
                selection_total: None,
            };
            let target = PropertiesTarget::Item {
                source,
                parent: from.node(),
                node,
                name,
            };
            Some(Ask::Open(target, request))
        }
        Reach::Hit { index, entry, .. } => {
            let host_path = sources
                .host_root(source)
                .map(|root| host_path_of_crumbs(root, &index.crumbs(*entry)));
            Some(Ask::Walk(Walk {
                source,
                name,
                reach: reach.clone(),
                fs: Arc::clone(location.fs()),
                request: Request {
                    from: None,
                    host_path,
                    folder,
                    selection_total: None,
                },
            }))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Loaders. Plain functions over what the background executor was handed.

/// The sentence for a part that could not be read.
fn read_error(what: &str, error: &Error) -> SharedString {
    match error {
        Error::Unsupported(_) => "This volume does not say.".into(),
        other => format!("The {what} could not be read ({other}).").into(),
    }
}

/// The Windows attributes read-only, hidden and system.
#[cfg(any(windows, test))]
fn host_flags_of(attributes: u32) -> HostFlags {
    HostFlags {
        read_only: attributes & 0x1 != 0,
        hidden: attributes & 0x2 != 0,
        system: attributes & 0x4 != 0,
    }
}

/// What the host says about `path` that a BSD mode does not. Windows only; reads the disk, so
/// the background executor only.
#[cfg(windows)]
fn host_flags(path: &Path) -> Option<HostFlags> {
    use std::os::windows::fs::MetadataExt;
    let metadata = std::fs::metadata(path).ok()?;
    Some(host_flags_of(metadata.file_attributes()))
}

#[cfg(not(windows))]
fn host_flags(_path: &Path) -> Option<HostFlags> {
    None
}

/// The `limit` biggest of `items` by their bytes, biggest first; ties keep their order. O(n) in
/// the items and O(limit) in memory: a folder may hold millions of names.
fn largest_of<T>(items: impl Iterator<Item = (u64, T)>, limit: usize) -> Vec<(u64, T)> {
    let mut top: Vec<(u64, T)> = Vec::with_capacity(limit.saturating_add(1));
    for item in items {
        if item.0 == 0 {
            continue;
        }
        if top.len() >= limit && top.last().is_some_and(|last| last.0 >= item.0) {
            continue;
        }
        let at = top.partition_point(|held| held.0 >= item.0);
        top.insert(at, item);
        top.truncate(limit);
    }
    top
}

/// One of the biggest items of a folder, with what a click on it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Largest {
    node: NodeId,
    name: SharedString,
    bytes: u64,
    folder: bool,
}

/// What the size table knows of one folder.
struct FolderFacts {
    totals: Totals,
    /// `None` when the caller has the classes from elsewhere.
    classes: Option<ClassBytes>,
    largest: Vec<Largest>,
    lower_bound: bool,
}

/// The totals, the kinds and the largest children of the folder `node`. `None` when the table
/// does not know the folder. O(subtree) with `with_classes`, else O(children): background
/// executor only.
fn folder_facts(
    table: &IndexSizes,
    index: &Index,
    node: NodeId,
    with_classes: bool,
) -> Option<FolderFacts> {
    let entry = table.folder_entry(node)?;
    let totals = table.totals(entry)?;
    let classes = with_classes.then(|| table.classes_of(index, entry));
    let children = table.children(entry).filter_map(|child| {
        let row = index.entry(child)?;
        let bytes = match table.totals(child) {
            Some(totals) => totals.bytes,
            None => row.size.unwrap_or(0),
        };
        Some((bytes, (child, row.node, row.name.to_owned())))
    });
    let largest = largest_of(children, LISTED_LARGEST)
        .into_iter()
        .map(|(bytes, (child, node, name))| Largest {
            node,
            name: name.into(),
            bytes,
            folder: table.totals(child).is_some(),
        })
        .collect();
    Some(FolderFacts {
        totals,
        classes,
        largest,
        lower_bound: table.is_lower_bound(),
    })
}

/// The "What is inside" section for `facts`, with the places its rows lead to: each folder opens
/// as a child of `base`, each file shows `base` with the file's row selected.
fn inside_of(
    source: SourceId,
    facts: &FolderFacts,
    classes: ClassBytes,
    base: Option<&Location>,
) -> (SectionState, Vec<(NodeId, Place)>) {
    let mut places = Vec::new();
    let largest: Vec<LargestItem> = facts
        .largest
        .iter()
        .map(|item| {
            let link = base.map(|base| {
                let place = if item.folder {
                    (base.child(item.node, item.name.clone()), None)
                } else {
                    (base.clone(), Some(item.name.clone()))
                };
                places.push((item.node, place));
                FactLink::Folder {
                    source,
                    node: item.node,
                }
            });
            LargestItem {
                name: item.name.clone(),
                bytes: item.bytes,
                link,
            }
        })
        .collect();
    (inside_section(classes, &largest, facts.lower_bound), places)
}

/// Where a link in the window takes the main window: a folder, and the row to put the cursor on.
type Place = (Location, Option<SharedString>);

/// The heading of the window: the glyph tile, the name and the line under it.
#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub name: SharedString,
    pub glyph: Glyph,
    /// Kind and size, empty until they are known.
    pub subtitle: SharedString,
}

/// The line under a volume's name.
fn volume_subtitle(format: Option<&str>, total: Option<u64>) -> SharedString {
    let kind = match format {
        Some(format) => format!("{format} volume"),
        None => "Volume".to_owned(),
    };
    match total {
        Some(bytes) => format!("{kind}, {}", format::size(bytes)).into(),
        None => kind.into(),
    }
}

/// What the Disk target describes, from what `Sources` holds. `None` when the disk is gone.
fn describe_disk(sources: &Sources, id: DiskId) -> Option<DiskDescription> {
    let disk = sources.disk(id)?;
    let partitions: Vec<PartitionRow> = disk
        .volumes
        .iter()
        .filter_map(|&volume| sources.source(volume))
        .filter_map(|source| {
            let SourceKind::Volume { start, len, .. } = source.kind else {
                return None;
            };
            let readable = match &source.availability {
                Availability::Browsable => Ok(()),
                Availability::Unsupported { format } => {
                    Err(SharedString::from(format!("no reader for {format} yet")))
                }
                Availability::NeedsElevation => {
                    Err(SharedString::from("needs administrator rights"))
                }
                Availability::Failed(text) => Err(text.clone()),
            };
            Some((
                source.label.clone(),
                source.format.clone(),
                start,
                len,
                readable,
            ))
        })
        .collect();
    Some(DiskDescription {
        model: disk.label.clone(),
        connection: disk.removable.then(|| SharedString::from("Removable")),
        size: disk.size,
        sector_size: None,
        scheme: disk.scheme.map(scheme_name),
        device_path: disk.path.display().to_string().into(),
        partitions,
    })
}

/// The glyph of a source's tile.
fn source_glyph(source: &Source) -> Glyph {
    match source.group {
        SourceGroup::ThisComputer => Glyph::Folder,
        SourceGroup::DiskImages => Glyph::DiskImage,
        SourceGroup::Disks => Glyph::Other,
    }
}

/// The glyph of a disk's tile.
fn disk_glyph(disk: &Disk) -> Glyph {
    if disk.group == SourceGroup::DiskImages {
        Glyph::DiskImage
    } else {
        Glyph::Other
    }
}

// ---------------------------------------------------------------------------------------------
// The entity.

/// One Properties window's state.
pub struct Properties {
    target: PropertiesTarget,
    title: SharedString,
    header: Header,
    sheet: Sheet,
    collapsed: HashSet<SectionId>,
    /// Used and total bytes of a volume, for its bar.
    usage: Option<(u64, u64)>,
    /// Where the links of the window lead.
    places: HashMap<(SourceId, NodeId), Place>,
    /// True when a size table would change what this window shows and the source has none yet:
    /// the window loads again when one arrives.
    waiting_for_table: bool,
    tasks: Vec<Task<()>>,
    /// The SHA-256 being calculated, if one is.
    checksum: Option<ChecksumRun>,
    env: PropertiesEnv,
    _subscriptions: Vec<Subscription>,
}

impl Properties {
    /// Starts every loader of `target`; each section fills in as its task finishes.
    pub fn new(target: PropertiesTarget, env: PropertiesEnv, cx: &mut Context<Self>) -> Self {
        let title = Self::title_of(&target, env.sources.read(cx));
        let collapsed = cx
            .try_global::<Settings>()
            .and_then(|settings| settings.text_of(COLLAPSED_KEY))
            .map(parse_collapsed)
            .unwrap_or_default();
        let subscriptions = vec![cx.observe(&env.sizes, |this, _, cx| this.sizes_changed(cx))];
        let mut this = Self {
            header: Header {
                name: title.clone(),
                glyph: Glyph::Other,
                subtitle: SharedString::default(),
            },
            target,
            title,
            sheet: Sheet::default(),
            collapsed,
            usage: None,
            places: HashMap::new(),
            waiting_for_table: false,
            tasks: Vec::new(),
            checksum: None,
            env,
            _subscriptions: subscriptions,
        };
        this.load(cx);
        this
    }

    /// The window's title for `target`, from what `sources` holds.
    pub fn title_of(target: &PropertiesTarget, sources: &Sources) -> SharedString {
        match target {
            PropertiesTarget::Item { name, .. } => title_text(Some(name)),
            PropertiesTarget::Items { .. } => title_text(None),
            PropertiesTarget::Volume { source } => {
                title_text(sources.source(*source).map(|s| s.label.as_ref()))
            }
            PropertiesTarget::Disk { disk } => {
                title_text(sources.disk(*disk).map(|d| d.label.as_ref()))
            }
        }
    }

    /// The glyph tile, name and subtitle.
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// The sections in drawing order, and how each stands.
    pub fn sections(&self) -> &[(SectionId, SectionState)] {
        &self.sheet.sections
    }

    /// The pieces of the file on its volume, once known.
    pub fn strip(&self) -> Option<&PieceStrip> {
        self.sheet.strip.as_ref()
    }

    /// A volume's used and total bytes, once known.
    pub fn usage(&self) -> Option<(u64, u64)> {
        self.usage
    }

    /// "Copy everything": the window as text.
    pub fn as_text(&self) -> String {
        self.sheet.as_text(&self.title)
    }

    /// True when the section is folded away.
    pub fn is_collapsed(&self, id: SectionId) -> bool {
        self.collapsed.contains(&id)
    }

    /// Folds a section away or opens it again, and remembers that in the settings.
    pub fn toggle(&mut self, id: SectionId, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&id) {
            self.collapsed.insert(id);
        }
        if cx.try_global::<Settings>().is_some() {
            Settings::set(
                cx,
                COLLAPSED_KEY,
                SettingValue::Text(collapsed_text(&self.collapsed)),
            );
        }
        cx.notify();
    }

    /// "Calculate SHA-256": starts hashing the file's data on the background executor. Does
    /// nothing while one is running, or when the section is not showing the button.
    pub fn start_checksum(&mut self, cx: &mut Context<Self>) {
        if self.checksum.is_some() {
            return;
        }
        let offered = self
            .sheet
            .checksum()
            .is_some_and(|section| matches!(section.phase, Phase::Idle | Phase::Failed(_)));
        let PropertiesTarget::Item { source, node, .. } = &self.target else {
            return;
        };
        let (source, node) = (*source, *node);
        if !offered {
            return;
        }
        let Some(src) = self.source(source, cx) else {
            return;
        };
        let generation = self.sheet.generation;
        self.sheet
            .set_checksum(generation, Phase::Running { done: 0, total: 0 });
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Progress::default());
        let fs = Arc::clone(&src.fs);
        let work = {
            let (cancel, progress) = (Arc::clone(&cancel), Arc::clone(&progress));
            cx.spawn(async move |this, cx| {
                let result =
                    cx.background_executor()
                        .spawn(async move {
                            checksum::run(fs.open(node, Stream::Data), &cancel, &progress)
                        })
                        .await;
                this.update(cx, |this, cx| {
                    this.checksum_finished(generation, result, cx)
                })
                .ok();
            })
        };
        let ticker = {
            let progress = Arc::clone(&progress);
            cx.spawn(async move |this, cx| {
                let mut shown = (0, 0);
                loop {
                    cx.background_executor().timer(PROGRESS_INTERVAL).await;
                    let alive = this.update(cx, |this, cx| {
                        this.checksum_progress(generation, &progress, &mut shown, cx)
                    });
                    if !matches!(alive, Ok(true)) {
                        break;
                    }
                }
            })
        };
        self.checksum = Some(ChecksumRun {
            cancel,
            _work: work,
            _ticker: ticker,
        });
        cx.notify();
    }

    /// "Cancel": stops the calculation and goes back to the button.
    pub fn cancel_checksum(&mut self, cx: &mut Context<Self>) {
        // Dropping the run sets its flag and drops its tasks.
        if self.checksum.take().is_none() {
            return;
        }
        let generation = self.sheet.generation;
        self.sheet.set_checksum(generation, Phase::Idle);
        cx.notify();
    }

    /// The ticker's look at the counter. False once the calculation it belongs to is over.
    fn checksum_progress(
        &mut self,
        generation: u64,
        progress: &Progress,
        shown: &mut (u64, u64),
        cx: &mut Context<Self>,
    ) -> bool {
        if generation != self.sheet.generation || self.checksum.is_none() {
            return false;
        }
        if let Some((done, total)) = progress.changed_since(shown) {
            self.sheet
                .set_checksum(generation, Phase::Running { done, total });
            cx.notify();
        }
        true
    }

    /// The calculation ended. A result for a window that has loaded again is dropped.
    fn checksum_finished(
        &mut self,
        generation: u64,
        result: Result<Outcome, SharedString>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.sheet.generation {
            return;
        }
        self.checksum = None;
        self.sheet.set_checksum(generation, phase_of(result));
        cx.notify();
    }

    /// A click on a fact that leads somewhere: shows the place in the main window and brings it
    /// forward.
    pub fn go(&self, link: &FactLink, cx: &mut Context<Self>) {
        let place: Option<Place> = match link {
            FactLink::Folder { source, node } => self.places.get(&(*source, *node)).cloned(),
            FactLink::Disk(id) => {
                let sources = self.env.sources.read(cx);
                sources
                    .disk(*id)
                    .and_then(Disk::first_source)
                    .and_then(|first| sources.source(first))
                    .map(|source| (Location::root(source), None))
            }
        };
        let Some((location, focus)) = place else {
            return;
        };
        self.env.navigation.update(cx, |navigation, cx| {
            navigation.navigate_focus(location, focus, cx);
        });
        if let Some(main) = aux_window::main_window(cx) {
            main.update(cx, |_, window, _| window.activate_window())
                .ok();
        }
    }

    /// The size tables changed: a window that waited for one loads again.
    fn sizes_changed(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.target.source() else {
            return;
        };
        if self.waiting_for_table && self.env.sizes.read(cx).table(source).is_some() {
            self.load(cx);
        }
    }

    /// Runs `work` on the background executor and hands its result to `then`, unless the window
    /// has loaded again meanwhile.
    fn spawn<T: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        work: impl FnOnce() -> T + Send + 'static,
        then: impl FnOnce(&mut Self, T, u64, &mut Context<Self>) + 'static,
    ) {
        let generation = self.sheet.generation;
        let task = cx.spawn(async move |this, cx| {
            let value = cx.background_executor().spawn(async move { work() }).await;
            this.update(cx, |this, cx| {
                if this.sheet.generation == generation {
                    then(this, value, generation, cx);
                    cx.notify();
                }
            })
            .ok();
        });
        self.tasks.push(task);
    }

    /// A section of its own: `work` builds it off the UI thread.
    fn spawn_section(
        &mut self,
        cx: &mut Context<Self>,
        id: SectionId,
        work: impl FnOnce() -> SectionState + Send + 'static,
    ) {
        self.spawn(cx, work, move |this, state, generation, _| {
            this.sheet.apply(generation, id, state);
        });
    }

    /// Reads `source` and its kind of place, from memory.
    fn source(&self, id: SourceId, cx: &Context<Self>) -> Option<Source> {
        self.env.sources.read(cx).source(id).cloned()
    }

    /// Starts every loader again under a new generation: the tasks of the last load are dropped.
    fn load(&mut self, cx: &mut Context<Self>) {
        self.tasks.clear();
        // A hash of the old target stops with it.
        self.checksum = None;
        self.usage = None;
        self.places.clear();
        self.waiting_for_table = false;
        let target = self.target.clone();
        let host = target
            .source()
            .and_then(|id| self.source(id, cx))
            .is_some_and(|source| matches!(source.kind, SourceKind::Host));
        let ids = sections_for(&target, host, self.env.request.folder);
        self.sheet.begin(&ids);
        match target {
            PropertiesTarget::Item {
                source,
                parent,
                node,
                name,
            } => self.load_item(source, parent, node, name, host, cx),
            PropertiesTarget::Items { .. } => self.load_items(cx),
            PropertiesTarget::Volume { source } => self.load_volume(source, host, cx),
            PropertiesTarget::Disk { disk } => self.load_disk(disk, cx),
        }
        cx.notify();
    }

    fn failed(&mut self, text: SharedString) {
        let generation = self.sheet.generation;
        let ids: Vec<SectionId> = self.sheet.sections.iter().map(|(id, _)| *id).collect();
        self.sheet.settle(
            generation,
            vec![(SectionId::General, SectionState::Failed(text))],
            &ids,
        );
    }

    // ---- several items ----

    fn load_items(&mut self, cx: &mut Context<Self>) {
        let PropertiesTarget::Items { nodes, .. } = &self.target else {
            return;
        };
        let count = nodes.len();
        let request = &self.env.request;
        let shared = request.from.as_ref().map(|from| from.name().clone());
        let general = several_items(count, request.selection_total, shared);
        // The classes of several items are not summed yet: "What is inside" stays empty.
        let generation = self.sheet.generation;
        self.sheet.settle(
            generation,
            vec![(SectionId::General, general)],
            &[SectionId::General, SectionId::Inside],
        );
        self.header = Header {
            name: format::items(count).into(),
            glyph: Glyph::Other,
            subtitle: request
                .selection_total
                .map(|bytes| SharedString::from(format::size(bytes)))
                .unwrap_or_default(),
        };
        cx.notify();
    }

    // ---- a disk ----

    fn load_disk(&mut self, id: DiskId, cx: &mut Context<Self>) {
        let generation = self.sheet.generation;
        let sources = self.env.sources.read(cx);
        let (description, glyph) = (describe_disk(sources, id), sources.disk(id).map(disk_glyph));
        match description {
            Some(description) => {
                self.header = Header {
                    name: description.model.clone(),
                    glyph: glyph.unwrap_or(Glyph::Other),
                    subtitle: format!(
                        "{}, {}",
                        if glyph == Some(Glyph::DiskImage) {
                            "Disk image"
                        } else {
                            "Disk"
                        },
                        format::size(description.size)
                    )
                    .into(),
                };
                self.sheet
                    .settle(generation, disk_sections(&description), &[SectionId::Disk]);
            }
            None => self.failed("This disk is no longer there.".into()),
        }
    }

    // ---- a volume ----

    fn load_volume(&mut self, source: SourceId, host: bool, cx: &mut Context<Self>) {
        let Some(src) = self.source(source, cx) else {
            self.failed("This volume is no longer there.".into());
            return;
        };
        self.header = Header {
            name: src.label.clone(),
            glyph: source_glyph(&src),
            subtitle: volume_subtitle(src.format.as_ref().map(|f| f.as_ref()), None),
        };
        let sizes = self.env.sizes.read(cx);
        let (table, classes, lower) = (
            sizes.table(source),
            sizes.classes(source).map(|(classes, _)| classes),
            sizes.is_lower_bound(source),
        );
        let index = self.env.search.read(cx).index(source);
        self.waiting_for_table = !host && table.is_none();
        let partition = match src.kind {
            SourceKind::Volume { disk, start, len } => Some(PartitionFacts {
                number: None,
                start,
                len,
                disk: self
                    .env
                    .sources
                    .read(cx)
                    .disk(disk)
                    .map(|disk| (disk.label.clone(), disk.id)),
            }),
            _ => None,
        };
        let base = Location::root(&src);
        let fs = Arc::clone(&src.fs);
        let format = src.format.clone();
        self.spawn(
            cx,
            move || {
                let info = fs.info();
                let identity = fs.identity();
                let inside = table
                    .zip(index)
                    .and_then(|(table, index)| folder_facts(&table, &index, fs.root(), false));
                (info, identity, inside)
            },
            move |this, (info, identity, inside), generation, _| {
                this.volume_arrived(
                    source,
                    &base,
                    VolumeRead {
                        info,
                        identity,
                        inside,
                        classes,
                        lower_bound: lower,
                        partition,
                        format,
                    },
                    generation,
                );
            },
        );
    }

    fn volume_arrived(
        &mut self,
        source: SourceId,
        base: &Location,
        read: VolumeRead,
        generation: u64,
    ) {
        let VolumeRead {
            info,
            identity,
            mut inside,
            classes,
            lower_bound,
            partition,
            format,
        } = read;
        let format_name: Option<&str> = info.format;
        self.header.subtitle = volume_subtitle(
            format_name.or(format.as_ref().map(|f| f.as_ref())),
            info.total_bytes,
        );
        self.usage = info.total_bytes.map(|total| {
            let free = info.free_bytes.unwrap_or(0);
            (total.saturating_sub(free).min(total), total)
        });
        if let Some(facts) = inside.as_mut() {
            facts.lower_bound |= lower_bound;
        }
        let totals = inside.as_ref().map(|facts| facts.totals);
        let mut results = volume_sections(&info, identity.as_ref(), totals, classes, partition);
        let inside_state = match (classes, inside.as_ref()) {
            (Some(classes), Some(facts)) => {
                let (state, places) = inside_of(source, facts, classes, Some(base));
                for (node, place) in places {
                    self.places.insert((source, node), place);
                }
                state
            }
            _ => SectionState::Absent,
        };
        results.push((SectionId::Inside, inside_state));
        self.sheet.settle(
            generation,
            results,
            &[
                SectionId::General,
                SectionId::Volume,
                SectionId::OnDisk,
                SectionId::Inside,
            ],
        );
    }

    // ---- one item ----

    fn load_item(
        &mut self,
        source: SourceId,
        parent: NodeId,
        node: NodeId,
        name: SharedString,
        host: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(src) = self.source(source, cx) else {
            self.failed("This item is no longer there.".into());
            return;
        };
        self.header.name = name.clone();
        let fs = Arc::clone(&src.fs);
        self.spawn(
            cx,
            move || {
                let metadata = fs
                    .metadata(node)
                    .map_err(|error| read_error("item", &error))?;
                Ok::<_, SharedString>((metadata, fs.info()))
            },
            move |this, read, generation, cx| match read {
                Ok((metadata, info)) => this.item_arrived(
                    ItemRead {
                        source,
                        parent,
                        node,
                        name,
                        host,
                        src,
                        metadata,
                        info,
                    },
                    generation,
                    cx,
                ),
                Err(text) => this.failed(text),
            },
        );
    }

    /// The metadata is in: the header is known, and every other section can start.
    fn item_arrived(&mut self, read: ItemRead, generation: u64, cx: &mut Context<Self>) {
        let ItemRead {
            source,
            parent,
            node,
            name,
            host,
            src,
            metadata,
            info,
        } = read;
        let browsable = metadata.kind == NodeKind::Directory
            || (metadata.kind == NodeKind::Symlink
                && metadata.flags.contains(EntryFlags::LINKS_TO_DIRECTORY));
        let compressed = metadata.flags.contains(EntryFlags::COMPRESSED);
        self.header = Header {
            glyph: Glyph::from_class(classify_entry(&name, browsable, None)),
            subtitle: subtitle(
                &name,
                metadata.kind,
                metadata.finder_codes,
                metadata.size,
                browsable,
                compressed,
            )
            .into(),
            name: name.clone(),
        };
        // The row said whether this is a folder; the metadata has the last word.
        let ids = sections_for(&self.target, host, browsable);
        self.sheet.relist(&ids);
        // The hash is offered for a regular file and for nothing else; it never starts alone.
        let checksum = if metadata.kind == NodeKind::File {
            SectionState::Checksum(checksum::Checksum::idle(!host))
        } else {
            SectionState::Absent
        };
        self.sheet.apply(generation, SectionId::Checksum, checksum);

        let item = ItemFacts {
            name: name.clone(),
            kind: metadata.kind,
            size: metadata.size,
            modified: metadata.modified,
            browsable,
        };
        let request = self.env.request.clone();
        let from = request.from.clone();
        let where_ = from.as_ref().map(|from| {
            self.places
                .insert((source, parent), (from.clone(), Some(name.clone())));
            (
                from.name().clone(),
                FactLink::Folder {
                    source,
                    node: parent,
                },
            )
        });
        let fs = Arc::clone(&src.fs);
        let volume_len = match src.kind {
            SourceKind::Volume { len, .. } => Some(len),
            SourceKind::Host | SourceKind::WholeDisk { .. } => None,
        };
        let sizes = self.env.sizes.read(cx);
        let table = sizes.table(source);
        let index = self.env.search.read(cx).index(source);
        let sized = table.is_some() && index.is_some();
        self.waiting_for_table = browsable && !host && !sized;
        let metadata = Arc::new(metadata);

        // General: the pieces' sum for a file, the folder's totals.
        {
            let (fs, metadata, table) = (Arc::clone(&fs), Arc::clone(&metadata), table.clone());
            self.spawn_section(cx, SectionId::General, move || {
                let on_disk = (item.kind == NodeKind::File && !host)
                    .then(|| fs.extents(node, Stream::Data))
                    .map(|runs| runs.map_err(|error| read_error("pieces", &error)));
                let totals = if browsable && !host {
                    table.and_then(|table| table.folder_entry(node).and_then(|e| table.totals(e)))
                } else {
                    None
                };
                general_item(
                    &item,
                    &metadata,
                    on_disk.as_ref(),
                    &info,
                    totals,
                    where_,
                    host,
                )
            });
        }
        if !host {
            self.sheet
                .apply(generation, SectionId::Mac, mac_item(&metadata, None));
        }
        // Permissions: the host's attributes are read from the disk.
        if host {
            let metadata = Arc::clone(&metadata);
            let path = request.host_path.clone();
            self.spawn_section(cx, SectionId::Permissions, move || {
                let flags = path.as_deref().and_then(host_flags);
                permissions_item(&metadata, flags)
            });
        } else {
            self.sheet.apply(
                generation,
                SectionId::Permissions,
                permissions_item(&metadata, None),
            );
        }
        if !host && !browsable {
            let (fs, metadata) = (Arc::clone(&fs), Arc::clone(&metadata));
            self.spawn(
                cx,
                move || {
                    let extents = fs
                        .extents(node, Stream::Data)
                        .map_err(|error| read_error("pieces", &error));
                    on_disk_item(&extents, &metadata, volume_len)
                },
                |this, (state, strip), generation, _| {
                    this.sheet.apply(generation, SectionId::OnDisk, state);
                    this.sheet.strip = strip;
                },
            );
        }
        if !host {
            let fs = Arc::clone(&fs);
            let (dir, item_name) = (parent, name.to_string());
            self.spawn_section(cx, SectionId::Xattrs, move || {
                let mut mac = read_attributes(fs.as_ref(), node);
                // The Finder keeps comments in the folder's `.DS_Store` as well.
                if mac.comment.is_none() {
                    mac.comment = disk_atlas_lib::mac::ds_store::read_store(fs.as_ref(), dir)
                        .and_then(|store| store.comment(&item_name).map(str::to_owned));
                }
                xattrs_with_mac(
                    &mac,
                    &fs.xattrs(node)
                        .map_err(|error| read_error("extended attributes", &error)),
                )
            });
        }
        // What is inside a folder, from the size table (a disk Disk Atlas reads).
        if browsable && !host {
            match (table, index) {
                (Some(table), Some(index)) => {
                    let base = from.as_ref().map(|from| from.child(node, name.clone()));
                    self.spawn(
                        cx,
                        move || folder_facts(&table, &index, node, true),
                        move |this, facts, generation, _| {
                            let state = match facts {
                                Some(facts) => {
                                    let classes = facts.classes.unwrap_or_default();
                                    let (state, places) =
                                        inside_of(source, &facts, classes, base.as_ref());
                                    for (child, place) in places {
                                        this.places.insert((source, child), place);
                                    }
                                    state
                                }
                                None => SectionState::Absent,
                            };
                            this.sheet.apply(generation, SectionId::Inside, state);
                        },
                    );
                }
                // No table yet: the section shows up when one arrives.
                _ => {
                    self.sheet
                        .apply(generation, SectionId::Inside, SectionState::Absent);
                }
            }
        } else {
            self.sheet
                .apply(generation, SectionId::Inside, SectionState::Absent);
        }
    }
}

/// A SHA-256 being calculated. Dropping it stops it: the flag is set, which the step loop sees
/// before its next step, and the tasks are dropped.
struct ChecksumRun {
    cancel: Arc<AtomicBool>,
    _work: Task<()>,
    _ticker: Task<()>,
}

impl Drop for ChecksumRun {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Where the section goes when the calculation ends with `result`; a cancel goes back to the
/// button.
fn phase_of(result: Result<Outcome, SharedString>) -> Phase {
    match result {
        Ok(Outcome::Done(digest)) => Phase::Done(digest.into()),
        Ok(Outcome::Cancelled) => Phase::Idle,
        Err(reason) => Phase::Failed(reason),
    }
}

/// What the metadata step of an item brings.
struct ItemRead {
    source: SourceId,
    parent: NodeId,
    node: NodeId,
    name: SharedString,
    host: bool,
    src: Source,
    metadata: Metadata,
    info: VolumeInfo,
}

/// What the loader of a volume brings.
struct VolumeRead {
    info: VolumeInfo,
    identity: Option<VolumeIdentity>,
    inside: Option<FolderFacts>,
    classes: Option<ClassBytes>,
    lower_bound: bool,
    partition: Option<PartitionFacts>,
    format: Option<SharedString>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::facts::Fact;

    fn item(n: u64) -> PropertiesTarget {
        PropertiesTarget::Item {
            source: SourceId::for_test(1),
            parent: NodeId(2),
            node: NodeId(n),
            name: format!("file {n}").into(),
        }
    }

    #[test]
    fn a_file_on_a_disk_gets_the_sections_of_a_file() {
        use SectionId::*;
        assert_eq!(
            sections_for(&item(5), false, false),
            [General, Mac, OnDisk, Permissions, Xattrs, Checksum]
        );
    }

    #[test]
    fn a_folder_on_a_disk_gets_what_is_inside_and_no_pieces() {
        use SectionId::*;
        assert_eq!(
            sections_for(&item(5), false, true),
            [General, Inside, Mac, Permissions, Xattrs]
        );
    }

    #[test]
    fn a_host_item_gets_general_and_permissions_and_a_folder_waits_for_its_sizes() {
        use SectionId::*;
        assert_eq!(
            sections_for(&item(5), true, false),
            [General, Permissions, Checksum]
        );
        assert_eq!(
            sections_for(&item(5), true, true),
            [General, Inside, Permissions]
        );
    }

    #[test]
    fn several_items_get_general_and_inside() {
        use SectionId::*;
        let target = PropertiesTarget::Items {
            source: SourceId::for_test(1),
            parent: NodeId(2),
            nodes: Arc::from([NodeId(3), NodeId(4)]),
        };
        assert_eq!(sections_for(&target, false, false), [General, Inside]);
        assert_eq!(sections_for(&target, true, false), [General, Inside]);
    }

    #[test]
    fn a_volume_has_a_partition_section_only_off_the_host() {
        use SectionId::*;
        let target = PropertiesTarget::Volume {
            source: SourceId::for_test(1),
        };
        assert_eq!(
            sections_for(&target, false, false),
            [General, Volume, OnDisk, Inside]
        );
        assert_eq!(
            sections_for(&target, true, false),
            [General, Volume, Inside]
        );
    }

    #[test]
    fn a_disk_gets_the_disk_section_alone() {
        let target = PropertiesTarget::Disk {
            disk: DiskId::for_test(1),
        };
        assert_eq!(sections_for(&target, false, false), [SectionId::Disk]);
    }

    #[test]
    fn only_a_single_file_gets_a_checksum() {
        let has = |target: &PropertiesTarget, host, folder| {
            sections_for(target, host, folder).contains(&SectionId::Checksum)
        };
        // A file, on this computer and on a disk.
        assert!(has(&item(5), true, false));
        assert!(has(&item(5), false, false));
        // A folder, several items, a volume, a disk.
        assert!(!has(&item(5), true, true));
        assert!(!has(&item(5), false, true));
        let several = PropertiesTarget::Items {
            source: SourceId::for_test(1),
            parent: NodeId(2),
            nodes: Arc::from([NodeId(3), NodeId(4)]),
        };
        assert!(!has(&several, true, false));
        assert!(!has(&several, false, false));
        let volume = PropertiesTarget::Volume {
            source: SourceId::for_test(1),
        };
        assert!(!has(&volume, true, false));
        assert!(!has(&volume, false, false));
        let disk = PropertiesTarget::Disk {
            disk: DiskId::for_test(1),
        };
        assert!(!has(&disk, false, false));
    }

    #[test]
    fn titles_say_the_name_or_nothing() {
        assert_eq!(title_text(Some("notes.txt")), "notes.txt Properties");
        assert_eq!(title_text(None), "Properties");
        assert_eq!(title_text(Some("")), "Properties");
    }

    #[test]
    fn targets_are_equal_when_what_they_name_is() {
        assert_eq!(item(5), item(5));
        assert_ne!(item(5), item(6));
        let several = |nodes: &[u64]| PropertiesTarget::Items {
            source: SourceId::for_test(1),
            parent: NodeId(2),
            nodes: nodes.iter().map(|&n| NodeId(n)).collect(),
        };
        assert_eq!(several(&[1, 2]), several(&[1, 2]));
        assert_ne!(several(&[1, 2]), several(&[1, 3]));
    }

    // The registry --------------------------------------------------------------------------

    fn open_all(registry: &mut Registry<u32>, count: u64) {
        for n in 0..count {
            registry.add(item(n), n as u32);
        }
    }

    #[test]
    fn asking_again_for_the_same_target_focuses_its_window() {
        let mut registry = Registry::<u32>::default();
        assert_eq!(registry.decide(&item(1), 8), Opening::Open);
        registry.add(item(1), 10);
        assert_eq!(registry.decide(&item(1), 8), Opening::Focus(10));
    }

    #[test]
    fn a_different_target_opens_a_second_window() {
        let mut registry = Registry::<u32>::default();
        registry.add(item(1), 10);
        assert_eq!(registry.decide(&item(2), 8), Opening::Open);
        registry.add(item(2), 11);
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.decide(&item(1), 8), Opening::Focus(10));
        assert_eq!(registry.decide(&item(2), 8), Opening::Focus(11));
    }

    #[test]
    fn the_ninth_window_is_refused_and_the_newest_is_focused() {
        let mut registry = Registry::<u32>::default();
        open_all(&mut registry, MAX_PROPERTIES_WINDOWS as u64);
        assert_eq!(
            registry.decide(&item(100), MAX_PROPERTIES_WINDOWS),
            Opening::Full(7)
        );
        // Asking for one that is open still just focuses it.
        assert_eq!(
            registry.decide(&item(3), MAX_PROPERTIES_WINDOWS),
            Opening::Focus(3)
        );
    }

    #[test]
    fn closed_windows_are_pruned_and_free_their_slot() {
        let mut registry = Registry::<u32>::default();
        open_all(&mut registry, MAX_PROPERTIES_WINDOWS as u64);
        registry.prune(|handle| *handle != 2 && *handle != 5);
        assert_eq!(registry.len(), MAX_PROPERTIES_WINDOWS - 2);
        assert_eq!(
            registry.decide(&item(100), MAX_PROPERTIES_WINDOWS),
            Opening::Open
        );
        // A closed window's target opens afresh instead of focusing a ghost.
        assert_eq!(
            registry.decide(&item(2), MAX_PROPERTIES_WINDOWS),
            Opening::Open
        );
    }

    // The sheet -----------------------------------------------------------------------------

    fn ready(label: &str, value: &str) -> SectionState {
        SectionState::Ready(vec![Fact::new(label.to_string(), value.to_string())])
    }

    #[test]
    fn a_result_for_an_older_generation_is_dropped() {
        let mut sheet = Sheet::default();
        let first = sheet.begin(&[SectionId::General, SectionId::Mac]);
        let second = sheet.begin(&[SectionId::General, SectionId::Mac]);
        assert_ne!(first, second);
        assert!(!sheet.apply(first, SectionId::General, ready("Kind", "Old")));
        assert_eq!(
            sheet.state_of(SectionId::General),
            Some(&SectionState::Loading)
        );
        assert!(sheet.apply(second, SectionId::General, ready("Kind", "New")));
        assert_eq!(
            sheet.state_of(SectionId::General),
            Some(&ready("Kind", "New"))
        );
    }

    #[test]
    fn a_result_for_a_section_the_window_does_not_have_is_dropped() {
        let mut sheet = Sheet::default();
        let generation = sheet.begin(&[SectionId::General]);
        assert!(!sheet.apply(generation, SectionId::Xattrs, SectionState::Absent));
        assert_eq!(sheet.sections.len(), 1);
    }

    #[test]
    fn settling_makes_the_sections_nobody_built_absent() {
        let mut sheet = Sheet::default();
        let ids = [SectionId::General, SectionId::OnDisk, SectionId::Inside];
        let generation = sheet.begin(&ids);
        assert!(sheet.settle(
            generation,
            vec![(SectionId::General, ready("Format", "HFS+"))],
            &ids
        ));
        assert_eq!(
            sheet.state_of(SectionId::OnDisk),
            Some(&SectionState::Absent)
        );
        assert_eq!(
            sheet.state_of(SectionId::Inside),
            Some(&SectionState::Absent)
        );
        assert!(!sheet.settle(generation + 1, Vec::new(), &ids));
    }

    #[test]
    fn relisting_keeps_what_arrived_and_starts_the_new_sections() {
        let mut sheet = Sheet::default();
        let generation = sheet.begin(&[SectionId::General, SectionId::Mac]);
        sheet.apply(generation, SectionId::General, ready("Kind", "PNG image"));
        sheet.relist(&[SectionId::General, SectionId::Inside]);
        assert_eq!(
            sheet.state_of(SectionId::General),
            Some(&ready("Kind", "PNG image"))
        );
        assert_eq!(
            sheet.state_of(SectionId::Inside),
            Some(&SectionState::Loading)
        );
        assert_eq!(sheet.state_of(SectionId::Mac), None);
    }

    #[test]
    fn the_window_as_text_has_each_section_with_its_rows() {
        let mut sheet = Sheet::default();
        let generation = sheet.begin(&[
            SectionId::General,
            SectionId::Permissions,
            SectionId::Xattrs,
        ]);
        sheet.apply(
            generation,
            SectionId::General,
            SectionState::Ready(vec![
                Fact::new("Kind", "PNG image"),
                Fact::new("Size", "1.8 MB").copying("1887437"),
            ]),
        );
        sheet.apply(
            generation,
            SectionId::Permissions,
            ready("Mode", "rw-r--r-- (644)"),
        );
        sheet.apply(generation, SectionId::Xattrs, SectionState::Absent);
        assert_eq!(
            sheet.as_text("a.png Properties"),
            "a.png Properties\n\nGeneral\nKind\tPNG image\nSize\t1887437\n\nPermissions\nMode\trw-r--r-- (644)"
        );
    }

    // The checksum -------------------------------------------------------------------------

    fn sheet_with_checksum(data_fork: bool) -> (Sheet, u64) {
        let mut sheet = Sheet::default();
        let generation = sheet.begin(&[SectionId::General, SectionId::Checksum]);
        sheet.apply(
            generation,
            SectionId::Checksum,
            SectionState::Checksum(checksum::Checksum::idle(data_fork)),
        );
        (sheet, generation)
    }

    #[test]
    fn a_late_checksum_result_for_an_old_generation_is_dropped() {
        let (mut sheet, old) = sheet_with_checksum(false);
        assert!(sheet.set_checksum(old, Phase::Running { done: 1, total: 2 }));
        // The window loads again: the old calculation's results no longer count.
        let new = sheet.begin(&[SectionId::General, SectionId::Checksum]);
        sheet.apply(
            new,
            SectionId::Checksum,
            SectionState::Checksum(checksum::Checksum::idle(false)),
        );
        assert!(!sheet.set_checksum(old, Phase::Done("ab".into())));
        assert_eq!(sheet.checksum().map(|c| &c.phase), Some(&Phase::Idle));
        assert!(sheet.set_checksum(new, Phase::Done("cd".into())));
        assert_eq!(
            sheet
                .checksum()
                .and_then(|c| c.digest())
                .map(|d| d.as_ref()),
            Some("cd")
        );
    }

    #[test]
    fn a_checksum_result_for_a_window_without_the_section_is_dropped() {
        let mut sheet = Sheet::default();
        let generation = sheet.begin(&[SectionId::General]);
        assert!(!sheet.set_checksum(generation, Phase::Idle));
        // Still loading (the metadata has not said it is a file): nothing to move.
        let generation = sheet.begin(&[SectionId::General, SectionId::Checksum]);
        assert!(!sheet.set_checksum(generation, Phase::Idle));
        assert_eq!(sheet.checksum(), None);
    }

    #[test]
    fn the_phases_follow_how_the_calculation_ended() {
        use checksum::Outcome;
        assert_eq!(
            phase_of(Ok(Outcome::Done("ab".to_owned()))),
            Phase::Done("ab".into())
        );
        assert_eq!(phase_of(Ok(Outcome::Cancelled)), Phase::Idle);
        assert_eq!(
            phase_of(Err("It broke.".into())),
            Phase::Failed("It broke.".into())
        );
    }

    #[test]
    fn the_window_as_text_has_the_digest_only_once_it_is_calculated() {
        let (mut sheet, generation) = sheet_with_checksum(true);
        let general = |sheet: &mut Sheet| {
            sheet.apply(generation, SectionId::General, ready("Kind", "Document"));
        };
        general(&mut sheet);
        // Idle, running, failed: no line, and not even the section.
        let plain = "a.txt Properties

General
Kind	Document";
        assert_eq!(sheet.as_text("a.txt Properties"), plain);
        sheet.set_checksum(generation, Phase::Running { done: 1, total: 2 });
        assert_eq!(sheet.as_text("a.txt Properties"), plain);
        sheet.set_checksum(generation, Phase::Failed("It broke.".into()));
        assert_eq!(sheet.as_text("a.txt Properties"), plain);
        // Done: the digest, once, under the row's label.
        let digest = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        sheet.set_checksum(generation, Phase::Done(digest.into()));
        assert_eq!(
            sheet.as_text("a.txt Properties"),
            format!(
                "{plain}

Checksum
SHA-256 of the data fork	{digest}"
            )
        );
        let text = sheet.as_text("a.txt Properties");
        assert_eq!(text.matches(digest).count(), 1);
    }

    #[test]
    fn the_checksum_section_is_remembered_collapsed_under_its_own_word() {
        let set: HashSet<SectionId> = [SectionId::Checksum].into_iter().collect();
        assert_eq!(collapsed_text(&set), "checksum");
        assert_eq!(parse_collapsed("checksum"), set);
    }

    // The collapsed setting -----------------------------------------------------------------

    #[test]
    fn collapsed_sections_round_trip_through_their_text() {
        let set: HashSet<SectionId> = [SectionId::Xattrs, SectionId::General, SectionId::OnDisk]
            .into_iter()
            .collect();
        let text = collapsed_text(&set);
        assert_eq!(text, "general,on-disk,xattrs");
        assert_eq!(parse_collapsed(&text), set);
        assert!(parse_collapsed("").is_empty());
        assert_eq!(
            parse_collapsed(" mac , nonsense,,mac"),
            [SectionId::Mac].into_iter().collect()
        );
    }

    // Loaders -------------------------------------------------------------------------------

    #[test]
    fn the_largest_items_are_the_biggest_in_order() {
        let items = [
            (5u64, 'a'),
            (0, 'z'),
            (90, 'b'),
            (7, 'c'),
            (90, 'd'),
            (1, 'e'),
            (8, 'f'),
        ];
        let top = largest_of(items.into_iter(), 3);
        assert_eq!(top, [(90, 'b'), (90, 'd'), (8, 'f')]);
        assert!(largest_of(std::iter::empty::<(u64, ())>(), 3).is_empty());
        // Nothing of zero bytes is "largest".
        assert!(largest_of([(0u64, 1)].into_iter(), 3).is_empty());
    }

    #[test]
    fn the_windows_attributes_are_read_one_bit_each() {
        assert_eq!(
            host_flags_of(0x1 | 0x20),
            HostFlags {
                read_only: true,
                hidden: false,
                system: false
            }
        );
        assert_eq!(
            host_flags_of(0x2 | 0x4),
            HostFlags {
                read_only: false,
                hidden: true,
                system: true
            }
        );
    }

    #[test]
    fn a_hits_folder_is_its_path_without_itself_and_the_root_is_renamed() {
        let path = vec![
            (NodeId(2), "Vol".to_owned()),
            (NodeId(10), "Docs".to_owned()),
            (NodeId(11), "a.txt".to_owned()),
        ];
        let crumbs = parent_crumbs(&path, &"Macintosh HD".into()).expect("a folder above");
        let names: Vec<&str> = crumbs.iter().map(|c| c.name.as_ref()).collect();
        assert_eq!(names, ["Macintosh HD", "Docs"]);
        assert_eq!(crumbs[1].node, NodeId(10));
        // An entry at the root has no folder above it.
        assert!(parent_crumbs(&path[..1], &"x".into()).is_none());
        assert!(parent_crumbs(&[], &"x".into()).is_none());
    }

    #[test]
    fn a_hit_on_the_host_has_the_path_of_its_names() {
        let path = vec![
            (NodeId(2), "C".to_owned()),
            (NodeId(10), "Docs".to_owned()),
            (NodeId(11), "a.txt".to_owned()),
        ];
        assert_eq!(
            host_path_of_crumbs(Path::new("/root"), &path),
            Path::new("/root").join("Docs").join("a.txt")
        );
    }

    #[test]
    fn the_subtitle_of_a_volume_names_its_format_and_size() {
        assert_eq!(
            volume_subtitle(Some("HFS+"), Some(1024)),
            "HFS+ volume, 1 KB"
        );
        assert_eq!(volume_subtitle(None, None), "Volume");
    }
}
