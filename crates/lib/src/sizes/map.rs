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

//! The space map: what a folder holds, as a tree of sizes, and its
//! squarified layout (design `r1-redesign.md` §3.5).
//!
//! [`index_map`] and [`host_map`] read a size table ([`IndexSizes`] or
//! [`HostSizes`]) into a [`MapNode`] tree a few levels deep; [`layout`] turns
//! that tree into [`Tile`]s inside a rectangle. The readers block and walk
//! subtrees, so they run off the UI thread; `layout` is pure arithmetic.
//!
//! Sizes come from disk (or from a table built from disk), so every sum
//! saturates and every ratio is computed in `f64` from values that cannot
//! be NaN or infinite, then clamped into the bounds.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{self, AtomicBool};

use crate::fs::NodeId;
use crate::search::index::{Index, NO_RSIZE, NO_SIZE};
use crate::search::kind::KindClass;
use crate::sizes::host::{HostSizes, SizeState};
use crate::sizes::index::{ClassBytes, IndexSizes};

/// Tiles smaller than this in either direction merge.
pub const MIN_TILE_PX: f32 = 6.0;
/// Most tiles [`layout`] returns.
pub const MAX_TILES: usize = 2000;
/// Narrowest tile that gets a label.
pub const LABEL_MIN_W: f32 = 60.0;
/// Shortest tile that gets a label.
pub const LABEL_MIN_H: f32 = 28.0;

/// `cancel` is checked when the count of entries visited crosses a multiple
/// of this.
const CANCEL_EVERY: u64 = 1 << 16;

/// How many times [`layout`] raises its merge threshold before it cuts the
/// list instead. Each step multiplies the threshold by [`GROWTH`].
const MAX_ROUNDS: u32 = 64;
const GROWTH: f32 = 1.25;

/// What a tile stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapTarget {
    /// A folder or file of a volume Disk Atlas reads: its index entry. For a
    /// folder this is the entry [`IndexSizes::folder_entry`] gives for its
    /// node (the one that holds the children), so a directory link names its
    /// target.
    Index(u32),
    /// A folder on the host.
    Host(PathBuf),
    /// The files directly in a host folder, which have no entries of their own.
    HostFiles(PathBuf),
    /// Free space of the volume (added by [`MapNode::with_free`]).
    Free,
    /// "N smaller items": made by [`layout`], never by the readers.
    Merged,
}

/// One item of the map.
#[derive(Debug, Clone, PartialEq)]
pub struct MapItem {
    /// Display name. For [`MapTarget::HostFiles`] the folder's name only
    /// ("Files in <folder>" is the app's job); for the root of a volume, the
    /// volume's name.
    pub name: String,
    pub target: MapTarget,
    pub is_folder: bool,
    /// A folder's total; a file's size (data plus resource fork, as
    /// [`IndexSizes`] counts it).
    pub bytes: u64,
    /// A file's kind; a folder's dominant kind (the class with the most
    /// bytes inside, housekeeping ignored; `Other` when it holds no bytes).
    pub class: KindClass,
    /// The bytes are a lower bound: a host folder not measured completely,
    /// or a size table flagged lower bound.
    pub lower_bound: bool,
}

/// An item and, for a folder, what it holds.
#[derive(Debug, Clone, PartialEq)]
pub struct MapNode {
    pub item: MapItem,
    /// Largest first (ties by name, case-insensitive, then entry or path, so
    /// the order is total and stable). Only folders have children, and only
    /// down to the requested depth.
    pub children: Vec<MapNode>,
}

impl MapNode {
    /// Adds free space as the last child of the root, only when `free > 0`.
    /// For the volume's top level. The root's own `bytes` stay its total.
    #[must_use]
    pub fn with_free(mut self, free: u64) -> Self {
        if free > 0 {
            self.children.push(MapNode {
                item: MapItem {
                    name: "Free space".to_owned(),
                    target: MapTarget::Free,
                    is_folder: false,
                    bytes: free,
                    class: KindClass::Other,
                    lower_bound: false,
                },
                children: Vec::new(),
            });
        }
        self
    }
}

/// A rectangle in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    /// Finite, with a positive width and height.
    fn is_usable(self) -> bool {
        [self.x, self.y, self.w, self.h]
            .iter()
            .all(|v| v.is_finite())
            && (self.x + self.w).is_finite()
            && (self.y + self.h).is_finite()
            && self.w > 0.0
            && self.h > 0.0
    }
}

/// One drawn region.
#[derive(Debug, Clone, PartialEq)]
pub struct Tile {
    pub rect: Rect,
    /// 0 = a tile of the shown folder's children, 1 = inside one of those.
    pub depth: u8,
    /// Index into the root's `children` (depth 0), and for depth 1 into that
    /// child's `children`. The second index is `None` for depth 0 and for a
    /// `Merged` tile at depth 1. A `Merged` tile at depth 0 has `u32::MAX`
    /// as its first index (it stands for no single child).
    pub path: (u32, Option<u32>),
    /// Large enough for a label: at least [`LABEL_MIN_W`] x [`LABEL_MIN_H`]
    /// (the tile's own rect, which for a depth-1 tile already lies below its
    /// parent's header band).
    pub labelled: bool,
    /// For a `Merged` tile: how many items it stands for, and their bytes.
    pub merged: Option<(u32, u64)>,
}

/// How [`layout`] nests a folder's children inside its tile.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutOptions {
    /// Border between a folder tile and its children; the app passes 2.0.
    pub inset: f32,
    /// Band at the top of a folder tile kept free for its label when the
    /// tile is tall enough; the app passes 16.0.
    pub header: f32,
}

// ---------------------------------------------------------------------------
// Readers
// ---------------------------------------------------------------------------

/// The class with the most bytes in `classes`, housekeeping (`system`)
/// ignored. Ties go to the class first in [`KindClass::ALL`]; `Other` when
/// every class is 0.
fn dominant(classes: &ClassBytes) -> KindClass {
    let mut best = KindClass::Other;
    let mut best_bytes = 0u64;
    for class in KindClass::ALL {
        let bytes = classes.of(class);
        if bytes > best_bytes {
            best = class;
            best_bytes = bytes;
        }
    }
    best
}

/// Case-insensitive order of two names, without allocating.
fn cmp_names(a: &str, b: &str) -> Ordering {
    a.chars()
        .flat_map(char::to_lowercase)
        .cmp(b.chars().flat_map(char::to_lowercase))
}

fn target_rank(t: &MapTarget) -> u8 {
    match t {
        MapTarget::Index(_) => 0,
        MapTarget::Host(_) => 1,
        MapTarget::HostFiles(_) => 2,
        MapTarget::Free => 3,
        MapTarget::Merged => 4,
    }
}

fn cmp_targets(a: &MapTarget, b: &MapTarget) -> Ordering {
    match (a, b) {
        (MapTarget::Index(x), MapTarget::Index(y)) => x.cmp(y),
        (MapTarget::Host(x), MapTarget::Host(y))
        | (MapTarget::HostFiles(x), MapTarget::HostFiles(y)) => x.cmp(y),
        _ => target_rank(a).cmp(&target_rank(b)),
    }
}

/// Largest first, then by name (case-insensitive), then by target.
fn sort_children(children: &mut [MapNode]) {
    children.sort_by(|a, b| {
        b.item
            .bytes
            .cmp(&a.item.bytes)
            .then_with(|| cmp_names(&a.item.name, &b.item.name))
            .then_with(|| a.item.name.cmp(&b.item.name))
            .then_with(|| cmp_targets(&a.item.target, &b.item.target))
    });
}

/// The tree for `folder` of an indexed volume, `depth` levels below it (the
/// app asks for 2): depth 0 is the folder alone.
///
/// `folder` is an entry number; a directory link is resolved to the entry
/// that holds the children. `None` when `folder` is not a folder of `index`,
/// or when `cancel` was set (checked every 65,536 entries visited).
///
/// Cost: listing the children is O(entries listed). Each folder's dominant
/// kind takes one [`IndexSizes::classes_of`], O(its subtree). The folders of
/// one level have disjoint subtrees inside `folder`'s, so a level costs at
/// most one pass over `folder`'s subtree, and depth 2 costs three (the
/// root, level 0, level 1). The one exception is a directory link: its
/// target may lie outside `folder`, so its pass is extra; the results are
/// kept per target for the call, so a target linked many times (a Time
/// Machine backup) is read once. Off the UI thread.
pub fn index_map(
    index: &Index,
    sizes: &IndexSizes,
    folder: u32,
    depth: u8,
    cancel: &AtomicBool,
) -> Option<MapNode> {
    let mut walk = IndexWalk {
        index,
        sizes,
        cancel,
        visited: 0,
        next_check: CANCEL_EVERY,
        // In a well-formed index every entry appears at most once in the
        // tree; directory links can repeat subtrees. Past this many nodes no
        // more folders are opened (they stay leaves with their totals).
        node_budget: u64::from(index.len())
            .saturating_mul(2)
            .saturating_add(CANCEL_EVERY),
        classes: HashMap::new(),
        lower_bound: sizes.is_lower_bound(),
    };
    if walk.cancelled() || !index.is_folder(folder) {
        return None;
    }
    let target = walk.resolve(folder);
    let totals = sizes.totals(target).or_else(|| sizes.totals(folder))?;
    let item = MapItem {
        name: index.name(folder).to_owned(),
        target: MapTarget::Index(target),
        is_folder: true,
        bytes: totals.bytes,
        class: walk.class_of(target),
        lower_bound: walk.lower_bound,
    };
    let children = walk.children(target, depth)?;
    Some(MapNode { item, children })
}

struct IndexWalk<'a> {
    index: &'a Index,
    sizes: &'a IndexSizes,
    cancel: &'a AtomicBool,
    visited: u64,
    next_check: u64,
    node_budget: u64,
    /// Dominant class per resolved folder entry, for this call.
    classes: HashMap<u32, KindClass>,
    lower_bound: bool,
}

impl IndexWalk<'_> {
    fn cancelled(&self) -> bool {
        self.cancel.load(atomic::Ordering::Relaxed)
    }

    /// Counts one entry visited; `None` when cancelled.
    fn tick(&mut self) -> Option<()> {
        self.visited = self.visited.saturating_add(1);
        if self.visited >= self.next_check {
            self.next_check = self.visited.saturating_add(CANCEL_EVERY);
            if self.cancelled() {
                return None;
            }
        }
        Some(())
    }

    /// The entry that holds a folder's children: its node's
    /// [`IndexSizes::folder_entry`], else the entry itself.
    fn resolve(&self, entry: u32) -> u32 {
        self.index
            .nodes()
            .get(entry as usize)
            .and_then(|&node| self.sizes.folder_entry(NodeId(node)))
            .unwrap_or(entry)
    }

    fn class_of(&mut self, folder: u32) -> KindClass {
        if let Some(&class) = self.classes.get(&folder) {
            return class;
        }
        let class = dominant(&self.sizes.classes_of(self.index, folder));
        self.classes.insert(folder, class);
        class
    }

    /// Data plus resource fork of a file entry, saturating.
    fn file_bytes(&self, entry: u32) -> u64 {
        let size = match self.index.sizes().get(entry as usize) {
            Some(&s) if s != NO_SIZE => s,
            _ => 0,
        };
        let rsize = match self.index.rsizes().get(entry as usize) {
            Some(&r) if r != NO_RSIZE => u64::from(r),
            _ => 0,
        };
        size.saturating_add(rsize)
    }

    /// The children of the resolved folder `folder`, `levels` deep. Nested
    /// at most `levels` (a `u8`) calls deep.
    fn children(&mut self, folder: u32, levels: u8) -> Option<Vec<MapNode>> {
        if levels == 0 || self.visited >= self.node_budget {
            return Some(Vec::new());
        }
        let mut out = Vec::new();
        let sizes = self.sizes;
        for child in sizes.children(folder) {
            self.tick()?;
            let Some(flags) = self.index.flags(child) else {
                continue;
            };
            let name = self.index.name(child).to_owned();
            if flags.is_folder() {
                let target = self.resolve(child);
                let bytes = sizes
                    .totals(target)
                    .or_else(|| sizes.totals(child))
                    .map_or(0, |t| t.bytes);
                let class = self.class_of(target);
                let children = self.children(target, levels.saturating_sub(1))?;
                out.push(MapNode {
                    item: MapItem {
                        name,
                        target: MapTarget::Index(target),
                        is_folder: true,
                        bytes,
                        class,
                        lower_bound: self.lower_bound,
                    },
                    children,
                });
            } else {
                out.push(MapNode {
                    item: MapItem {
                        name,
                        target: MapTarget::Index(child),
                        is_folder: false,
                        bytes: self.file_bytes(child),
                        class: flags.kind_class(),
                        lower_bound: self.lower_bound,
                    },
                    children: Vec::new(),
                });
            }
        }
        sort_children(&mut out);
        Some(out)
    }
}

/// Bytes and the lower-bound flag of a host folder's state. `Unknown` is 0.
fn host_bytes(state: SizeState) -> (u64, bool) {
    match state {
        SizeState::Unknown => (0, true),
        SizeState::Running(t) | SizeState::AtLeast(t) => (t.bytes, true),
        SizeState::Exact(t) => (t.bytes, false),
    }
}

/// The last component of `dir`, or the whole path for a drive root.
fn display_name(dir: &Path) -> String {
    dir.file_name()
        .unwrap_or(dir.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// The tree for a host folder, `depth` levels below it. Sub-folders the
/// tree has not measured appear with 0 bytes and `lower_bound`. The files
/// directly in a folder come as one [`MapTarget::HostFiles`] child (none
/// when it has no files), classed by the folder's bytes per kind minus its
/// sub-folders' (saturating).
///
/// `None` when `dir` is not in `sizes`' tree, or when `cancel` was set
/// (checked at every folder). Each folder costs one
/// [`HostSizes::classes`], O(its subtree): a level is one pass over `dir`'s
/// subtree. Off the UI thread.
pub fn host_map(sizes: &HostSizes, dir: &Path, depth: u8, cancel: &AtomicBool) -> Option<MapNode> {
    if cancel.load(atomic::Ordering::Relaxed) || !sizes.knows(dir) {
        return None;
    }
    let (bytes, lower_bound) = host_bytes(sizes.state(dir));
    let classes = sizes.classes(dir);
    let item = MapItem {
        name: display_name(dir),
        target: MapTarget::Host(dir.to_path_buf()),
        is_folder: true,
        bytes,
        class: dominant(&classes),
        lower_bound,
    };
    let children = host_children(sizes, dir, &classes, depth, cancel)?;
    Some(MapNode { item, children })
}

fn host_children(
    sizes: &HostSizes,
    dir: &Path,
    dir_classes: &ClassBytes,
    levels: u8,
    cancel: &AtomicBool,
) -> Option<Vec<MapNode>> {
    if levels == 0 {
        return Some(Vec::new());
    }
    if cancel.load(atomic::Ordering::Relaxed) {
        return None;
    }
    let mut files = *dir_classes;
    let mut out = Vec::new();
    for (name, state) in sizes.children(dir) {
        let path = dir.join(&name);
        let classes = sizes.classes(&path);
        for (slot, sub) in files.by_class.iter_mut().zip(classes.by_class) {
            *slot = slot.saturating_sub(sub);
        }
        let (bytes, lower_bound) = host_bytes(state);
        let children = host_children(sizes, &path, &classes, levels.saturating_sub(1), cancel)?;
        out.push(MapNode {
            item: MapItem {
                name: name.to_string_lossy().into_owned(),
                target: MapTarget::Host(path),
                is_folder: true,
                bytes,
                class: dominant(&classes),
                lower_bound,
            },
            children,
        });
    }
    let own = sizes.own(dir);
    if own.files > 0 {
        out.push(MapNode {
            item: MapItem {
                name: display_name(dir),
                target: MapTarget::HostFiles(dir.to_path_buf()),
                is_folder: false,
                bytes: own.bytes,
                class: dominant(&files),
                lower_bound: false,
            },
            children: Vec::new(),
        });
    }
    sort_children(&mut out);
    Some(out)
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// Edges of a rectangle in `f64`: x0, y0, x1, y1.
type Edges = [f64; 4];

/// The pure squarify step: `weights` sorted descending; one rect per weight
/// (zero-sized for zero weight), tiling `bounds`'s area in proportion.
/// Unusable bounds (not finite, or not positive in both directions) give
/// zero-sized rects.
pub fn squarify(weights: &[u64], bounds: Rect) -> Vec<Rect> {
    let w: Vec<f64> = weights.iter().map(|&w| w as f64).collect();
    let mut edges = Vec::new();
    let mut order = Vec::new();
    if bounds.is_usable() {
        squarify_edges(&w, outer_edges(bounds), &mut edges, &mut order);
        edges.iter().map(|&e| to_rect(e)).collect()
    } else {
        let (x, y) = if bounds.x.is_finite() && bounds.y.is_finite() {
            (bounds.x, bounds.y)
        } else {
            (0.0, 0.0)
        };
        let zero = Rect {
            x,
            y,
            w: 0.0,
            h: 0.0,
        };
        vec![zero; weights.len()]
    }
}

fn outer_edges(r: Rect) -> Edges {
    // The far edges are computed in f32 like the caller would, so a rect
    // converted back never reaches past `r.x + r.w`.
    [
        f64::from(r.x),
        f64::from(r.y),
        f64::from(r.x + r.w),
        f64::from(r.y + r.h),
    ]
}

/// Edges to a rect. Shared edges are the same `f64`, so neighbours meet
/// exactly after the conversion and never overlap.
#[allow(clippy::cast_possible_truncation)]
fn to_rect(e: Edges) -> Rect {
    let x0 = e[0] as f32;
    let y0 = e[1] as f32;
    let x1 = e[2] as f32;
    let y1 = e[3] as f32;
    Rect {
        x: x0,
        y: y0,
        w: (x1 - x0).max(0.0),
        h: (y1 - y0).max(0.0),
    }
}

/// Squarified layout (Bruls, Huizing, van Wijk) into `out`, one entry per
/// weight. `order` is scratch. Non-positive or non-finite weights get a
/// zero-sized rect at the top-left corner.
fn squarify_edges(weights: &[f64], b: Edges, out: &mut Vec<Edges>, order: &mut Vec<usize>) {
    out.clear();
    out.resize(weights.len(), [b[0], b[1], b[0], b[1]]);
    order.clear();
    order.extend(
        weights
            .iter()
            .enumerate()
            .filter(|&(_, &w)| w.is_finite() && w > 0.0)
            .map(|(i, _)| i),
    );
    let total: f64 = order.iter().filter_map(|&i| weights.get(i)).sum();
    let (mut x0, mut y0, x1, y1) = (b[0], b[1], b[2], b[3]);
    let area = (x1 - x0) * (y1 - y0);
    if !(total > 0.0 && total.is_finite() && area > 0.0 && area.is_finite()) {
        return;
    }
    let scale = area / total;
    let area_of = |i: usize| weights.get(i).copied().unwrap_or(0.0) * scale;

    let m = order.len();
    let mut start = 0;
    // Bounded: `start` grows by at least one every turn.
    while start < m {
        let rw = x1 - x0;
        let rh = y1 - y0;
        if rw <= 0.0 || rh <= 0.0 {
            // Rounding used the space up: the rest stay zero-sized.
            for &i in order.get(start..).unwrap_or(&[]) {
                if let Some(slot) = out.get_mut(i) {
                    *slot = [x0, y0, x0, y0];
                }
            }
            return;
        }
        let side = rw.min(rh);
        let first = order.get(start).map_or(0.0, |&i| area_of(i));
        let mut sum = first;
        let mut lo = first;
        let mut hi = first;
        let mut worst = worst_ratio(sum, lo, hi, side);
        let mut end = start.saturating_add(1);
        while let Some(&i) = order.get(end) {
            let a = area_of(i);
            let (s, l, h) = (sum + a, lo.min(a), hi.max(a));
            let w = worst_ratio(s, l, h, side);
            if w > worst {
                break;
            }
            (sum, lo, hi, worst) = (s, l, h, w);
            end = end.saturating_add(1);
        }
        let last_row = end == m;
        let row = order.get(start..end).unwrap_or(&[]);
        if rw >= rh {
            // A column on the left, items stacked down it.
            let strip = if last_row {
                x1
            } else {
                (x0 + sum / rh).min(x1)
            };
            let mut cursor = y0;
            for (k, &i) in row.iter().enumerate() {
                let next = if k.saturating_add(1) == row.len() {
                    y1
                } else {
                    (cursor + area_of(i) / sum * rh).min(y1)
                };
                if let Some(slot) = out.get_mut(i) {
                    *slot = [x0, cursor, strip, next];
                }
                cursor = next;
            }
            x0 = strip;
        } else {
            // A row along the top, items side by side.
            let strip = if last_row {
                y1
            } else {
                (y0 + sum / rw).min(y1)
            };
            let mut cursor = x0;
            for (k, &i) in row.iter().enumerate() {
                let next = if k.saturating_add(1) == row.len() {
                    x1
                } else {
                    (cursor + area_of(i) / sum * rw).min(x1)
                };
                if let Some(slot) = out.get_mut(i) {
                    *slot = [cursor, y0, next, strip];
                }
                cursor = next;
            }
            y0 = strip;
        }
        start = end;
    }
}

/// The worst aspect ratio of a row of areas summing to `sum`, smallest
/// `lo`, largest `hi`, laid along `side`. All positive.
fn worst_ratio(sum: f64, lo: f64, hi: f64, side: f64) -> f64 {
    let s2 = sum * sum;
    let side2 = side * side;
    (side2 * hi / s2).max(s2 / (side2 * lo))
}

/// A parent's children with bytes, ready to place: the largest
/// [`MAX_TILES`] in order, the rest summed.
struct Ordered {
    /// (index into the parent's children, bytes), largest first, ties in
    /// input order.
    kept: Vec<(u32, u64)>,
    rest_count: u32,
    rest_bytes: u64,
    /// The rest's weight, summed in `f64` so it never saturates.
    rest_weight: f64,
}

impl Ordered {
    fn of(children: &[MapNode]) -> Self {
        let mut kept: Vec<(u32, u64)> = children
            .iter()
            .enumerate()
            .filter(|(_, c)| c.item.bytes > 0)
            .filter_map(|(i, c)| Some((u32::try_from(i).ok()?, c.item.bytes)))
            .collect();
        let by_size = |a: &(u32, u64), b: &(u32, u64)| b.1.cmp(&a.1).then(a.0.cmp(&b.0));
        let mut rest_count = 0u32;
        let mut rest_bytes = 0u64;
        let mut rest_weight = 0.0f64;
        if kept.len() > MAX_TILES {
            kept.select_nth_unstable_by(MAX_TILES, by_size);
            for &(_, b) in kept.get(MAX_TILES..).unwrap_or(&[]) {
                rest_count = rest_count.saturating_add(1);
                rest_bytes = rest_bytes.saturating_add(b);
                rest_weight += b as f64;
            }
            kept.truncate(MAX_TILES);
        }
        kept.sort_unstable_by(by_size);
        Ordered {
            kept,
            rest_count,
            rest_bytes,
            rest_weight,
        }
    }
}

/// What one placed region stands for.
#[derive(Clone, Copy)]
enum Placed {
    Item(u32),
    Merged(u32, u64),
}

/// Scratch buffers reused across every placement of one [`layout`] call.
#[derive(Default)]
struct Scratch {
    weights: Vec<f64>,
    edges: Vec<Edges>,
    order: Vec<usize>,
}

/// Places `items` inside `area`: tiles narrower or shorter than `min` merge
/// into one region placed last; a merged region under `min` is dropped.
fn place(
    items: &Ordered,
    area: Rect,
    min: f32,
    scratch: &mut Scratch,
    out: &mut Vec<(Rect, Placed)>,
) {
    out.clear();
    if !area.is_usable() || area.w < min || area.h < min {
        return;
    }
    let n = items.kept.len();
    if n == 0 {
        return;
    }
    let edges = outer_edges(area);
    let total: f64 = items.kept.iter().map(|&(_, b)| b as f64).sum::<f64>() + items.rest_weight;
    let px = (edges[2] - edges[0]) * (edges[3] - edges[1]);
    let min2 = f64::from(min) * f64::from(min);
    // Items whose share of the area is under min x min can never pass: a
    // prefix, since `kept` is largest first.
    let mut k = items
        .kept
        .iter()
        .take_while(|&&(_, b)| px * (b as f64) / total >= min2)
        .count();
    let min64 = f64::from(min);
    let small = |e: &Edges| e[2] - e[0] < min64 || e[3] - e[1] < min64;
    // Bounded: `k` falls every turn it does not break.
    loop {
        let (rest_count, rest_bytes, rest_weight) = rest_after(items, k);
        scratch.weights.clear();
        scratch
            .weights
            .extend(items.kept.iter().take(k).map(|&(_, b)| b as f64));
        if rest_count > 0 {
            scratch.weights.push(rest_weight);
        }
        squarify_edges(
            &scratch.weights,
            edges,
            &mut scratch.edges,
            &mut scratch.order,
        );
        let first_small = scratch.edges.iter().take(k).position(small);
        match first_small {
            Some(j) if j < k => {
                k = j;
                continue;
            }
            _ => {}
        }
        for (&(i, _), &e) in items.kept.iter().take(k).zip(scratch.edges.iter()) {
            out.push((to_rect(e), Placed::Item(i)));
        }
        if rest_count > 0
            && let Some(&e) = scratch.edges.get(k)
            && !small(&e)
        {
            let what = match items.kept.get(k) {
                // Exactly one item left, and it is one we can name.
                Some(&(i, _)) if rest_count == 1 => Placed::Item(i),
                _ => Placed::Merged(rest_count, rest_bytes),
            };
            out.push((to_rect(e), what));
        }
        return;
    }
}

/// Count, bytes (saturating) and weight of `kept[k..]` plus the rest.
fn rest_after(items: &Ordered, k: usize) -> (u32, u64, f64) {
    let tail = items.kept.get(k..).unwrap_or(&[]);
    let mut count = items.rest_count;
    let mut bytes = items.rest_bytes;
    let mut weight = items.rest_weight;
    for &(_, b) in tail {
        count = count.saturating_add(1);
        bytes = bytes.saturating_add(b);
        weight += b as f64;
    }
    (count, bytes, weight)
}

/// Where a folder tile's children go: inside the inset, and below the
/// header band when the tile is tall enough for a label in it.
fn inner(r: Rect, inset: f32, header: f32) -> Rect {
    let band = if r.h >= LABEL_MIN_H + header {
        header
    } else {
        0.0
    };
    Rect {
        x: r.x + inset,
        y: r.y + inset + band,
        w: (r.w - 2.0 * inset).max(0.0),
        h: (r.h - 2.0 * inset - band).max(0.0),
    }
}

fn tile(rect: Rect, depth: u8, first: u32, placed: Placed) -> Tile {
    let (path, merged) = match (depth, placed) {
        (0, Placed::Item(i)) => ((i, None), None),
        (_, Placed::Item(i)) => ((first, Some(i)), None),
        (0, Placed::Merged(n, b)) => ((u32::MAX, None), Some((n, b))),
        (_, Placed::Merged(n, b)) => ((first, None), Some((n, b))),
    };
    Tile {
        rect,
        depth,
        path,
        labelled: rect.w >= LABEL_MIN_W && rect.h >= LABEL_MIN_H,
        merged,
    }
}

/// A non-negative finite option value; anything else is 0.
fn sane(v: f32) -> f32 {
    if v.is_finite() && v > 0.0 { v } else { 0.0 }
}

/// Squarified layout (Bruls, Huizing, van Wijk) of the root's children
/// inside `bounds`, then of each depth-0 folder's children inside its tile
/// (after `inset` and, when the tile is at least `LABEL_MIN_H + header`
/// tall, `header`).
///
/// Tiles under [`MIN_TILE_PX`] merge, per parent, into one `Merged` tile
/// placed by the same algorithm as a normal item (last, with the merged
/// bytes as weight); a merged tile still under the minimum is dropped. At
/// most [`MAX_TILES`] tiles are returned: when over, the minimum is raised
/// for both depths (the smallest merge further) and the layout is redone.
/// Zero-byte items get no tile. Each item's own `bytes` is its weight;
/// nothing is recomputed from children.
///
/// Deterministic: the same input gives the same tiles in the same order,
/// every depth-0 tile first, then the depth-1 tiles grouped by parent in
/// depth-0 order. Unusable `bounds` (not finite, or not positive) give no
/// tiles.
pub fn layout(root: &MapNode, bounds: Rect, options: LayoutOptions) -> Vec<Tile> {
    let mut tiles = Vec::new();
    if !bounds.is_usable() {
        return tiles;
    }
    let inset = sane(options.inset);
    let header = sane(options.header);
    let top = Ordered::of(&root.children);
    let mut nested: HashMap<u32, Ordered> = HashMap::new();
    let mut scratch = Scratch::default();
    let mut level0 = Vec::new();
    let mut level1 = Vec::new();
    let mut min = MIN_TILE_PX;
    for _ in 0..MAX_ROUNDS {
        tiles.clear();
        place(&top, bounds, min, &mut scratch, &mut level0);
        tiles.extend(level0.iter().map(|&(r, p)| tile(r, 0, 0, p)));
        let mut over = tiles.len() > MAX_TILES;
        for &(rect, placed) in &level0 {
            if over {
                break;
            }
            let Placed::Item(i) = placed else {
                continue;
            };
            let Some(child) = root.children.get(i as usize) else {
                continue;
            };
            if !child.item.is_folder || child.children.is_empty() {
                continue;
            }
            let items = nested
                .entry(i)
                .or_insert_with(|| Ordered::of(&child.children));
            place(
                items,
                inner(rect, inset, header),
                min,
                &mut scratch,
                &mut level1,
            );
            tiles.extend(level1.iter().map(|&(r, p)| tile(r, 1, i, p)));
            over = tiles.len() > MAX_TILES;
        }
        if !over {
            return tiles;
        }
        min *= GROWTH;
    }
    tiles.truncate(MAX_TILES);
    tiles
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{EntryFlags, NodeKind, ScanSummary};
    use crate::search::index::{IndexFlags, IndexParts, NO_TIME};
    use crate::search::kind::classify_entry;
    use crate::sizes::host::Priority;
    use std::time::{Duration, Instant};

    static NO_CANCEL: AtomicBool = AtomicBool::new(false);

    // -- index fixtures ----------------------------------------------------

    struct Spec {
        name: String,
        parent: u32,
        kind: NodeKind,
        node: u64,
        size: Option<u64>,
        rsize: Option<u32>,
    }

    /// A hand-built index in pre-order; entry 0 is the root (node 2).
    struct Tree(Vec<Spec>, ScanSummary);

    impl Tree {
        fn new() -> Self {
            Tree(
                vec![Spec {
                    name: "Volume".to_owned(),
                    parent: 0,
                    kind: NodeKind::Directory,
                    node: 2,
                    size: None,
                    rsize: None,
                }],
                ScanSummary::default(),
            )
        }

        fn push(
            &mut self,
            parent: u32,
            name: &str,
            kind: NodeKind,
            node: u64,
            size: Option<u64>,
        ) -> u32 {
            self.0.push(Spec {
                name: name.to_owned(),
                parent,
                kind,
                node,
                size,
                rsize: None,
            });
            (self.0.len() - 1) as u32
        }

        fn dir(&mut self, parent: u32, name: &str, node: u64) -> u32 {
            self.push(parent, name, NodeKind::Directory, node, None)
        }

        fn file(&mut self, parent: u32, name: &str, node: u64, size: u64) -> u32 {
            self.push(parent, name, NodeKind::File, node, Some(size))
        }

        fn index(&self) -> Index {
            let mut names = String::new();
            let mut offsets = vec![0u32];
            for s in &self.0 {
                names.push_str(&s.name);
                names.push('\0');
                offsets.push(names.len() as u32);
            }
            let flags: Vec<u16> = self
                .0
                .iter()
                .map(|s| {
                    let folder = s.kind == NodeKind::Directory;
                    let class = classify_entry(&s.name, folder, None);
                    IndexFlags::new(
                        s.kind,
                        EntryFlags::EMPTY,
                        s.rsize.is_some_and(|r| r > 0),
                        class,
                    )
                    .bits()
                })
                .collect();
            Index::from_parts(IndexParts {
                names: names.into_boxed_str(),
                offsets: offsets.into(),
                parent: self.0.iter().map(|s| s.parent).collect(),
                node: self.0.iter().map(|s| s.node).collect(),
                size: self
                    .0
                    .iter()
                    .map(|s| match s.kind {
                        NodeKind::Directory => NO_SIZE,
                        _ => s.size.unwrap_or(NO_SIZE),
                    })
                    .collect(),
                modified: vec![NO_TIME; self.0.len()].into(),
                rsize: self.0.iter().map(|s| s.rsize.unwrap_or(NO_RSIZE)).collect(),
                flags: flags.into(),
                identity: None,
                summary: self.1,
            })
        }
    }

    fn build(index: &Index) -> IndexSizes {
        IndexSizes::build(index, &NO_CANCEL).unwrap()
    }

    fn names(node: &MapNode) -> Vec<&str> {
        node.children.iter().map(|c| c.item.name.as_str()).collect()
    }

    #[test]
    fn index_map_two_levels_sorted_with_dominant_kinds() {
        let mut tree = Tree::new();
        let photos = tree.dir(0, "Photos", 10);
        tree.file(photos, "a.jpg", 11, 500);
        tree.file(photos, "b.png", 12, 300);
        tree.file(photos, "notes.txt", 13, 100);
        let inner = tree.dir(photos, "Raw", 14);
        tree.file(inner, "c.mp4", 15, 50);
        let docs = tree.dir(0, "docs", 16);
        tree.file(docs, "a.txt", 17, 400);
        tree.file(docs, "song.mp3", 18, 300);
        tree.file(docs, "b.txt", 19, 300);
        let big = tree.file(0, "Big.mp4", 20, 2000);
        tree.0.last_mut().unwrap().rsize = Some(24);
        tree.dir(0, "Empty", 21);
        let index = tree.index();
        let sizes = build(&index);

        let map = index_map(&index, &sizes, 0, 2, &NO_CANCEL).unwrap();
        assert_eq!(map.item.name, "Volume");
        assert_eq!(map.item.target, MapTarget::Index(0));
        assert_eq!(map.item.bytes, 3974);
        assert_eq!(map.item.class, KindClass::Video);
        assert!(!map.item.lower_bound);
        assert_eq!(names(&map), ["Big.mp4", "docs", "Photos", "Empty"]);

        let file = &map.children[0];
        assert_eq!(file.item.target, MapTarget::Index(big));
        assert!(!file.item.is_folder);
        assert_eq!(file.item.bytes, 2024);
        assert_eq!(file.item.class, KindClass::Video);
        assert!(file.children.is_empty());

        // docs and Photos tie at 1,000 bytes: case-insensitive name order.
        let d = &map.children[1];
        assert_eq!(d.item.bytes, 1000);
        assert_eq!(d.item.class, KindClass::Document);
        assert_eq!(names(d), ["a.txt", "b.txt", "song.mp3"]);
        let p = &map.children[2];
        assert_eq!(p.item.target, MapTarget::Index(photos));
        assert_eq!(p.item.class, KindClass::Image);
        assert_eq!(names(p), ["a.jpg", "b.png", "notes.txt", "Raw"]);
        // Depth 2 stops below level 1: Raw has no children here.
        let raw = &p.children[3];
        assert!(raw.item.is_folder);
        assert_eq!(raw.item.class, KindClass::Video);
        assert!(raw.children.is_empty());

        let empty = &map.children[3];
        assert_eq!(empty.item.bytes, 0);
        assert_eq!(empty.item.class, KindClass::Other);

        // Depth 0 is the folder alone; depth 1 has no grandchildren.
        assert!(
            index_map(&index, &sizes, 0, 0, &NO_CANCEL)
                .unwrap()
                .children
                .is_empty()
        );
        let one = index_map(&index, &sizes, 0, 1, &NO_CANCEL).unwrap();
        assert!(one.children.iter().all(|c| c.children.is_empty()));
        // A sub-folder as the root.
        let sub = index_map(&index, &sizes, photos, 2, &NO_CANCEL).unwrap();
        assert_eq!(sub.item.bytes, 950);
        assert_eq!(names(&sub.children[3]), ["c.mp4"]);
    }

    #[test]
    fn index_map_rejects_files_and_bad_entries() {
        let mut tree = Tree::new();
        let f = tree.file(0, "a.txt", 10, 5);
        let index = tree.index();
        let sizes = build(&index);
        assert_eq!(index_map(&index, &sizes, f, 2, &NO_CANCEL), None);
        assert_eq!(index_map(&index, &sizes, 99, 2, &NO_CANCEL), None);
        assert_eq!(index_map(&index, &sizes, u32::MAX, 2, &NO_CANCEL), None);
    }

    #[test]
    fn a_file_with_three_names_is_three_tiles() {
        let mut tree = Tree::new();
        let docs = tree.dir(0, "Docs", 10);
        tree.file(docs, "one.jpg", 31, 77);
        tree.file(docs, "two.jpg", 31, 77);
        tree.file(docs, "three.jpg", 31, 77);
        let index = tree.index();
        let sizes = build(&index);
        let map = index_map(&index, &sizes, docs, 2, &NO_CANCEL).unwrap();
        assert_eq!(map.item.bytes, 231);
        assert_eq!(names(&map), ["one.jpg", "three.jpg", "two.jpg"]);
        let tiles = layout(&map, rect(0.0, 0.0, 300.0, 100.0), OPTIONS);
        assert_eq!(tiles.len(), 3);
        assert!(tiles.iter().all(|t| t.merged.is_none()));
    }

    #[test]
    fn a_directory_link_resolves_to_its_target_or_is_a_leaf() {
        // "Linked" is the first report of node 40 and holds the children;
        // "Link" is a second report of the same node with none.
        let mut tree = Tree::new();
        let target = tree.dir(0, "Linked", 40);
        tree.file(target, "x.jpg", 41, 100);
        let other = tree.dir(0, "Other", 50);
        let link = tree.dir(other, "Link", 40);
        // A link whose node has no entry with children: a leaf.
        let lone = tree.dir(0, "Lone", 60);
        tree.dir(0, "Lone again", 60);
        let index = tree.index();
        let sizes = build(&index);

        let map = index_map(&index, &sizes, 0, 2, &NO_CANCEL).unwrap();
        let o = map
            .children
            .iter()
            .find(|c| c.item.name == "Other")
            .unwrap();
        assert_eq!(o.item.bytes, 100);
        let l = &o.children[0];
        assert_eq!(l.item.name, "Link");
        assert_eq!(l.item.target, MapTarget::Index(target));
        assert_eq!(l.item.bytes, 100);
        assert_eq!(l.item.class, KindClass::Image);
        // The link opened as the shown folder shows the target's contents.
        let opened = index_map(&index, &sizes, link, 2, &NO_CANCEL).unwrap();
        assert_eq!(names(&opened), ["x.jpg"]);

        let lone_item = map.children.iter().find(|c| c.item.name == "Lone").unwrap();
        assert!(lone_item.item.is_folder);
        assert!(lone_item.children.is_empty());
        assert_eq!(lone_item.item.bytes, sizes.totals(lone).unwrap().bytes);
    }

    #[test]
    fn index_map_lower_bound_and_cancel() {
        let mut tree = Tree::new();
        let a = tree.dir(0, "A", 10);
        tree.file(a, "a.txt", 11, 5);
        tree.1.skipped = 1;
        let index = tree.index();
        let sizes = build(&index);
        assert!(sizes.is_lower_bound());
        let map = index_map(&index, &sizes, 0, 2, &NO_CANCEL).unwrap();
        assert!(map.item.lower_bound);
        assert!(map.children[0].item.lower_bound);
        assert!(map.children[0].children[0].item.lower_bound);

        let cancel = AtomicBool::new(true);
        assert_eq!(index_map(&index, &sizes, 0, 2, &cancel), None);
    }

    #[test]
    fn cancel_is_seen_inside_a_big_folder() {
        let index = crate::search::bench::synthetic(200_000, 7);
        let sizes = build(&index);
        let cancel = AtomicBool::new(false);
        assert!(index_map(&index, &sizes, 0, 2, &cancel).is_some());
        cancel.store(true, atomic::Ordering::Relaxed);
        assert_eq!(index_map(&index, &sizes, 0, 2, &cancel), None);
    }

    #[test]
    fn with_free_adds_a_last_child_only_when_positive() {
        let mut tree = Tree::new();
        tree.file(0, "a.txt", 10, 500);
        let index = tree.index();
        let sizes = build(&index);
        let map = index_map(&index, &sizes, 0, 2, &NO_CANCEL).unwrap();
        let same = map.clone().with_free(0);
        assert_eq!(same, map);
        let free = map.clone().with_free(1000);
        assert_eq!(free.children.len(), 2);
        let last = free.children.last().unwrap();
        assert_eq!(last.item.target, MapTarget::Free);
        assert_eq!(last.item.bytes, 1000);
        assert!(!last.item.is_folder);
        assert_eq!(free.item.bytes, map.item.bytes);
        // The layout weighs it like any child: the larger tile.
        let tiles = layout(&free, rect(0.0, 0.0, 100.0, 100.0), OPTIONS);
        assert_eq!(tiles.len(), 2);
        assert_eq!(tiles[0].path, (1, None));
        assert!(area(tiles[0].rect) > area(tiles[1].rect));
    }

    // -- host fixtures -------------------------------------------------------

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("disk-atlas-map-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn put(path: &Path, bytes: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![b'x'; bytes]).unwrap();
    }

    fn wait_exact(s: &HostSizes, dir: &Path) {
        let start = Instant::now();
        while !matches!(s.state(dir), SizeState::Exact(_)) {
            assert!(start.elapsed() < Duration::from_secs(20), "never measured");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn host_map_files_as_one_child_classed_by_subtraction() {
        let dir = TempDir::new("files");
        let root = &dir.0;
        put(&root.join("song.mp3"), 4000);
        put(&root.join("a.txt"), 100);
        put(&root.join("Pics").join("a.jpg"), 3000);
        put(&root.join("Pics").join("b.txt"), 10);
        put(&root.join("Pics").join("Deep").join("c.mp4"), 50);
        std::fs::create_dir_all(root.join("Empty")).unwrap();
        let sizes = HostSizes::new(root.clone(), Box::new(|| {}));
        sizes.request(root, Priority::Visible);
        wait_exact(&sizes, root);

        let map = host_map(&sizes, root, 2, &NO_CANCEL).unwrap();
        assert_eq!(map.item.target, MapTarget::Host(root.clone()));
        assert_eq!(map.item.bytes, 7160);
        assert!(!map.item.lower_bound);
        // Audio 4,000 beats images 3,000 for the whole folder.
        assert_eq!(map.item.class, KindClass::Audio);
        let files = &map.children[0];
        assert_eq!(files.item.target, MapTarget::HostFiles(root.clone()));
        assert_eq!(files.item.bytes, 4100);
        assert_eq!(files.item.class, KindClass::Audio);
        assert!(!files.item.is_folder);
        let pics = &map.children[1];
        assert_eq!(pics.item.target, MapTarget::Host(root.join("Pics")));
        assert_eq!(pics.item.bytes, 3060);
        assert_eq!(pics.item.class, KindClass::Image);
        // Pics's own files, and Deep (level 1, no children).
        assert_eq!(names(pics), ["Pics", "Deep"]);
        assert_eq!(
            pics.children[0].item.target,
            MapTarget::HostFiles(root.join("Pics"))
        );
        assert_eq!(pics.children[0].item.class, KindClass::Image);
        assert_eq!(pics.children[1].item.class, KindClass::Video);
        assert!(pics.children[1].children.is_empty());
        let empty = &map.children[2];
        assert_eq!(empty.item.bytes, 0);
        assert!(
            empty.children.is_empty(),
            "no files child when there are none"
        );

        assert_eq!(host_map(&sizes, &root.join("nope"), 2, &NO_CANCEL), None);
        let cancel = AtomicBool::new(true);
        assert_eq!(host_map(&sizes, root, 2, &cancel), None);
    }

    #[test]
    fn host_map_unmeasured_folders_are_lower_bounds() {
        let dir = TempDir::new("unmeasured");
        let root = &dir.0;
        put(&root.join("A").join("B").join("x.txt"), 10);
        put(&root.join("C").join("y.txt"), 10);
        let sizes = HostSizes::new(root.clone(), Box::new(|| {}));
        // Only A/B is asked for: the root and A are in the tree, unread.
        let b = root.join("A").join("B");
        sizes.request(&b, Priority::Visible);
        wait_exact(&sizes, &b);
        let map = host_map(&sizes, root, 2, &NO_CANCEL).unwrap();
        assert!(map.item.lower_bound);
        let a = map.children.iter().find(|c| c.item.name == "A").unwrap();
        assert!(a.item.lower_bound);
        let b_item = &a.children[0];
        assert_eq!(b_item.item.name, "B");
        assert_eq!(b_item.item.bytes, 10);
        assert!(!b_item.item.lower_bound);
        assert!(map.children.iter().all(|c| c.item.name != "C"));
    }

    // -- layout ------------------------------------------------------------

    const OPTIONS: LayoutOptions = LayoutOptions {
        inset: 2.0,
        header: 16.0,
    };

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    fn area(r: Rect) -> f64 {
        f64::from(r.w) * f64::from(r.h)
    }

    fn leaf(name: &str, bytes: u64) -> MapNode {
        MapNode {
            item: MapItem {
                name: name.to_owned(),
                target: MapTarget::Merged,
                is_folder: false,
                bytes,
                class: KindClass::Other,
                lower_bound: false,
            },
            children: Vec::new(),
        }
    }

    fn folder(name: &str, bytes: u64, children: Vec<MapNode>) -> MapNode {
        let mut n = leaf(name, bytes);
        n.item.is_folder = true;
        n.children = children;
        n
    }

    const EPS: f32 = 1e-3;

    fn inside(r: Rect, b: Rect) -> bool {
        r.w >= 0.0
            && r.h >= 0.0
            && r.x >= b.x - EPS
            && r.y >= b.y - EPS
            && r.x + r.w <= b.x + b.w + EPS
            && r.y + r.h <= b.y + b.h + EPS
    }

    fn overlap(a: Rect, b: Rect) -> bool {
        let w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
        let h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
        w > EPS && h > EPS
    }

    fn finite(r: Rect) -> bool {
        [r.x, r.y, r.w, r.h].iter().all(|v| v.is_finite())
    }

    #[test]
    fn squarify_tiles_exactly_in_proportion() {
        let b = rect(10.0, 20.0, 600.0, 400.0);
        let weights = [6, 6, 4, 3, 2, 2, 1, 0];
        let rects = squarify(&weights, b);
        assert_eq!(rects.len(), weights.len());
        let total: u64 = weights.iter().sum();
        let sum: f64 = rects.iter().map(|&r| area(r)).sum();
        assert!((sum - area(b)).abs() < 1e-2 * area(b) / 1000.0 + 1.0);
        for (i, (&w, &r)) in weights.iter().zip(&rects).enumerate() {
            assert!(inside(r, b), "{i}: {r:?}");
            let want = area(b) * w as f64 / total as f64;
            assert!(
                (area(r) - want).abs() <= want * 1e-3 + 1e-2,
                "{i}: {} vs {want}",
                area(r)
            );
            for &o in &rects[..i] {
                assert!(!overlap(r, o));
            }
        }
        assert_eq!(rects[7].w * rects[7].h, 0.0);
        // The classic example: 6,6,4,3,2,2,1 in 6 x 4 gives squares first.
        let r = squarify(&[6, 6, 4, 3, 2, 2, 1], rect(0.0, 0.0, 6.0, 4.0));
        assert!(
            (r[0].w - 3.0).abs() < 1e-4 && (r[0].h - 2.0).abs() < 1e-4,
            "{:?}",
            r[0]
        );
    }

    #[test]
    fn squarify_degenerate_input() {
        assert!(squarify(&[], rect(0.0, 0.0, 10.0, 10.0)).is_empty());
        for b in [
            rect(0.0, 0.0, 0.0, 10.0),
            rect(0.0, 0.0, -5.0, 10.0),
            rect(f32::NAN, 0.0, 10.0, 10.0),
            rect(0.0, 0.0, f32::INFINITY, 10.0),
            rect(f32::MAX, 0.0, f32::MAX, 10.0),
        ] {
            let r = squarify(&[5, 3], b);
            assert_eq!(r.len(), 2);
            assert!(
                r.iter().all(|r| finite(*r) && r.w == 0.0 && r.h == 0.0),
                "{b:?}: {r:?}"
            );
        }
        // All zero: zero-sized rects.
        let r = squarify(&[0, 0], rect(0.0, 0.0, 10.0, 10.0));
        assert!(r.iter().all(|r| r.w == 0.0 && r.h == 0.0));
        // Hostile weights do not overflow.
        let b = rect(0.0, 0.0, 1000.0, 1000.0);
        let r = squarify(&[u64::MAX - 1, u64::MAX - 1, 1], b);
        assert!(r.iter().all(|&r| finite(r) && inside(r, b)));
        assert!((area(r[0]) - 500_000.0).abs() < 1.0);
    }

    #[test]
    fn layout_simple_cases() {
        let b = rect(0.0, 0.0, 400.0, 300.0);
        // Empty folder.
        assert!(layout(&folder("r", 0, vec![]), b, OPTIONS).is_empty());
        // Only zero-byte children.
        assert!(layout(&folder("r", 0, vec![leaf("z", 0)]), b, OPTIONS).is_empty());
        // One child fills the bounds.
        let one = folder("r", 5, vec![leaf("a", 5)]);
        let tiles = layout(&one, b, OPTIONS);
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].rect, b);
        assert_eq!(tiles[0].path, (0, None));
        assert_eq!(tiles[0].depth, 0);
        assert!(tiles[0].labelled);
        assert_eq!(tiles[0].merged, None);
        // 1 px, zero, negative and NaN bounds.
        for bad in [
            rect(0.0, 0.0, 1.0, 1.0),
            rect(0.0, 0.0, 0.0, 300.0),
            rect(0.0, 0.0, -1.0, 300.0),
            rect(f32::NAN, 0.0, 400.0, 300.0),
            rect(0.0, 0.0, f32::INFINITY, 300.0),
        ] {
            assert!(layout(&one, bad, OPTIONS).is_empty(), "{bad:?}");
        }
    }

    #[test]
    fn layout_nests_children_below_the_header() {
        let tree = folder(
            "r",
            100,
            vec![
                folder("big", 75, vec![leaf("x", 50), leaf("y", 25), leaf("z", 0)]),
                leaf("small", 25),
            ],
        );
        let b = rect(0.0, 0.0, 400.0, 400.0);
        let tiles = layout(&tree, b, OPTIONS);
        let paths: Vec<_> = tiles.iter().map(|t| (t.depth, t.path)).collect();
        assert_eq!(
            paths,
            [
                (0, (0, None)),
                (0, (1, None)),
                (1, (0, Some(0))),
                (1, (0, Some(1)))
            ]
        );
        let parent = tiles[0].rect;
        let content = inner(parent, 2.0, 16.0);
        assert_eq!(content.y, parent.y + 18.0);
        for t in &tiles[2..] {
            assert!(inside(t.rect, content));
        }
        let sum: f64 = tiles[2..].iter().map(|t| area(t.rect)).sum();
        assert!((sum - area(content)).abs() < 1.0);
        // A short folder tile keeps no header band.
        let short = inner(rect(0.0, 0.0, 100.0, 40.0), 2.0, 16.0);
        assert_eq!(short, rect(2.0, 2.0, 96.0, 36.0));
    }

    #[test]
    fn small_items_merge_into_one_tile() {
        let mut children = vec![leaf("big", 1_000_000)];
        children.extend((0..50).map(|i| leaf(&format!("s{i}"), 1000)));
        children.push(leaf("zero", 0));
        let tree = folder("r", 0, children);
        let tiles = layout(&tree, rect(0.0, 0.0, 200.0, 100.0), OPTIONS);
        assert_eq!(tiles.len(), 2);
        assert_eq!(tiles[0].path, (0, None));
        assert_eq!(tiles[1].path, (u32::MAX, None));
        assert_eq!(tiles[1].merged, Some((50, 50_000)));
        assert!(
            tiles
                .iter()
                .all(|t| t.rect.w >= MIN_TILE_PX && t.rect.h >= MIN_TILE_PX)
        );
    }

    #[test]
    fn equal_weights_are_deterministic_and_keep_input_order() {
        let tree = folder("r", 0, (0..30).map(|i| leaf(&format!("{i}"), 7)).collect());
        let b = rect(0.0, 0.0, 300.0, 200.0);
        let a = layout(&tree, b, OPTIONS);
        let again = layout(&tree, b, OPTIONS);
        assert_eq!(a, again);
        let order: Vec<u32> = a.iter().map(|t| t.path.0).collect();
        assert_eq!(order, (0..30).collect::<Vec<_>>());
    }

    #[test]
    fn hostile_sizes_stay_inside_the_bounds() {
        let tree = folder(
            "r",
            1,
            vec![
                folder(
                    "lie",
                    1,
                    vec![leaf("a", u64::MAX - 1), leaf("b", u64::MAX - 1)],
                ),
                leaf("max", u64::MAX - 1),
                leaf("one", 1),
            ],
        );
        let b = rect(5.0, 5.0, 800.0, 600.0);
        let tiles = layout(&tree, b, OPTIONS);
        assert!(!tiles.is_empty());
        for t in &tiles {
            assert!(finite(t.rect) && inside(t.rect, b), "{t:?}");
        }
    }

    /// A deterministic LCG for the property tests.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }

        fn bytes(&mut self) -> u64 {
            match self.below(10) {
                0 => 0,
                1 => self.below(100),
                2 => u64::MAX - self.below(3),
                _ => 1 << self.below(40),
            }
        }
    }

    fn random_tree(rng: &mut Lcg, wide: u64) -> MapNode {
        let n = rng.below(wide);
        let children = (0..n)
            .map(|i| {
                if rng.below(3) == 0 {
                    let m = rng.below(30);
                    let kids = (0..m)
                        .map(|j| leaf(&format!("{i}.{j}"), rng.bytes()))
                        .collect();
                    folder(&format!("{i}"), rng.bytes(), kids)
                } else {
                    leaf(&format!("{i}"), rng.bytes())
                }
            })
            .collect();
        folder("root", rng.bytes(), children)
    }

    #[test]
    fn layout_properties_over_random_trees() {
        let mut rng = Lcg(0x5eed);
        for round in 0..300 {
            let tree = random_tree(&mut rng, if round % 10 == 0 { 3000 } else { 60 });
            let b = rect(
                rng.below(50) as f32,
                rng.below(50) as f32,
                1.0 + rng.below(1500) as f32,
                1.0 + rng.below(1000) as f32,
            );
            let tiles = layout(&tree, b, OPTIONS);
            assert!(tiles.len() <= MAX_TILES);
            assert_eq!(
                tiles,
                layout(&tree, b, OPTIONS),
                "round {round}: not deterministic"
            );
            let mut level0: Vec<(u32, Rect)> = Vec::new();
            let mut seen_depth1 = false;
            for t in &tiles {
                assert!(finite(t.rect), "round {round}: {t:?}");
                assert!(inside(t.rect, b), "round {round}: {t:?} outside {b:?}");
                assert!(t.rect.w >= MIN_TILE_PX - EPS && t.rect.h >= MIN_TILE_PX - EPS);
                assert_eq!(
                    t.labelled,
                    t.rect.w >= LABEL_MIN_W && t.rect.h >= LABEL_MIN_H
                );
                match t.depth {
                    0 => {
                        assert!(!seen_depth1, "round {round}: parents come first");
                        if t.merged.is_none() {
                            let c = &tree.children[t.path.0 as usize];
                            assert!(c.item.bytes > 0);
                        }
                        level0.push((t.path.0, t.rect));
                    }
                    1 => {
                        seen_depth1 = true;
                        let parent = level0
                            .iter()
                            .find(|(i, _)| *i == t.path.0)
                            .expect("parent tile");
                        assert!(inside(t.rect, inner(parent.1, 2.0, 16.0)));
                        if let Some(j) = t.path.1 {
                            let c = &tree.children[t.path.0 as usize].children[j as usize];
                            assert!(c.item.bytes > 0);
                        } else {
                            assert!(t.merged.is_some());
                        }
                    }
                    d => panic!("depth {d}"),
                }
            }
            // No two tiles of one parent overlap, nor any two depth-0 tiles.
            for (i, a) in tiles.iter().enumerate() {
                for c in &tiles[..i] {
                    if a.depth == c.depth && (a.depth == 0 || a.path.0 == c.path.0) {
                        assert!(!overlap(a.rect, c.rect), "round {round}: {a:?} {c:?}");
                    }
                }
            }
            let sum0: f64 = tiles
                .iter()
                .filter(|t| t.depth == 0)
                .map(|t| area(t.rect))
                .sum();
            assert!(sum0 <= area(b) * (1.0 + 1e-4) + 1.0);
        }
    }

    #[test]
    fn squarify_properties_over_random_weights() {
        let mut rng = Lcg(42);
        for round in 0..300 {
            let n = rng.below(200) as usize;
            let mut weights: Vec<u64> = (0..n).map(|_| rng.bytes()).collect();
            weights.sort_unstable_by(|a, b| b.cmp(a));
            let b = rect(
                rng.below(100) as f32,
                rng.below(100) as f32,
                1.0 + rng.below(2000) as f32,
                1.0 + rng.below(2000) as f32,
            );
            let rects = squarify(&weights, b);
            assert_eq!(rects.len(), n);
            let total: f64 = weights.iter().map(|&w| w as f64).sum();
            let sum: f64 = rects.iter().map(|&r| area(r)).sum();
            if total > 0.0 {
                assert!(
                    (sum - area(b)).abs() <= area(b) * 1e-4 + 1.0,
                    "round {round}: {sum} vs {}",
                    area(b)
                );
            }
            for (i, &r) in rects.iter().enumerate() {
                assert!(finite(r) && inside(r, b), "round {round}");
                if weights[i] == 0 {
                    assert_eq!(r.w * r.h, 0.0);
                }
                for &o in &rects[..i] {
                    assert!(!overlap(r, o), "round {round}: {r:?} {o:?}");
                }
            }
        }
    }

    #[test]
    fn too_many_tiles_merge_further() {
        let children: Vec<MapNode> = (0..2000)
            .map(|i| {
                let kids = (0..20).map(|j| leaf(&format!("{j}"), 1000 + j)).collect();
                folder(&format!("{i}"), 20_000 + i, kids)
            })
            .collect();
        let tree = folder("r", 0, children);
        let tiles = layout(&tree, rect(0.0, 0.0, 1920.0, 1080.0), OPTIONS);
        assert!(!tiles.is_empty());
        assert!(tiles.len() <= MAX_TILES);
    }

    // -- timings -------------------------------------------------------------

    #[allow(clippy::print_stdout)]
    fn report(label: &str, mut times: Vec<f64>, budget_ms: f64, extra: &str) {
        times.sort_by(f64::total_cmp);
        let median = times[times.len() / 2];
        println!(
            "{label}: median {median:.2} ms of {} (min {:.2}, max {:.2}), budget {budget_ms:.0} ms: {}; {extra}",
            times.len(),
            times[0],
            times[times.len() - 1],
            if median <= budget_ms { "met" } else { "MISSED" },
        );
    }

    fn time_layout(label: &str, tree: &MapNode, budget_ms: f64) {
        let b = rect(0.0, 0.0, 1600.0, 1000.0);
        let mut times = Vec::new();
        let mut count = 0;
        for _ in 0..7 {
            let started = Instant::now();
            let tiles = std::hint::black_box(layout(tree, b, OPTIONS));
            times.push(started.elapsed().as_secs_f64() * 1e3);
            count = tiles.len();
        }
        assert!(count <= MAX_TILES);
        report(
            label,
            times,
            budget_ms,
            &format!("{count} tiles in 1600 x 1000"),
        );
    }

    #[test]
    #[ignore = "timing; run in release with --ignored --nocapture"]
    fn map_bench_layout_2000_by_20() {
        let mut rng = Lcg(1);
        let children: Vec<MapNode> = (0..2000)
            .map(|i| {
                let kids: Vec<MapNode> = (0..20)
                    .map(|j| leaf(&format!("{j}"), 1 + rng.below(1 << 30)))
                    .collect();
                let total = kids.iter().map(|k| k.item.bytes).sum();
                folder(&format!("{i}"), total, kids)
            })
            .collect();
        time_layout(
            "map_bench_layout_2000_by_20",
            &folder("r", 0, children),
            8.0,
        );
    }

    #[test]
    #[ignore = "timing; run in release with --ignored --nocapture"]
    fn map_bench_layout_200k_flat() {
        let mut rng = Lcg(2);
        let children: Vec<MapNode> = (0..200_000)
            .map(|i| leaf(&format!("{i}"), 1 + rng.below(1 << 30)))
            .collect();
        time_layout(
            "map_bench_layout_200k_flat",
            &folder("r", 0, children),
            40.0,
        );
    }

    #[test]
    #[ignore = "timing; run in release with --ignored --nocapture"]
    fn map_bench_index_map_1m() {
        let index = crate::search::bench::synthetic(1_000_000, 42);
        let sizes = build(&index);
        let mut times = Vec::new();
        let mut shape = String::new();
        for _ in 0..7 {
            let started = Instant::now();
            let map = std::hint::black_box(index_map(&index, &sizes, 0, 2, &NO_CANCEL).unwrap());
            times.push(started.elapsed().as_secs_f64() * 1e3);
            let grand: usize = map.children.iter().map(|c| c.children.len()).sum();
            shape = format!(
                "{} entries; {} level-0 and {grand} level-1 items",
                index.len(),
                map.children.len()
            );
        }
        report("map_bench_index_map_1m", times, 150.0, &shape);
    }
}
