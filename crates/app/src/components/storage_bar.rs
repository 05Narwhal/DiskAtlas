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

//! The storage bar (`r10-everyday.md` 4): the partitions of the disk to scale, inside the current
//! one a segment per kind of file, then free space, a legend, and the measure of the selection.
//!
//! The geometry is two plain functions, [`layout`] and [`legend`], tested without a window. The
//! view reads only what [`Sources`], [`Sizes`] and [`Search`] already hold: no disk is touched
//! here, and the bytes per kind arrive once, when the size table of the volume is built.

use std::time::{Duration, Instant};

use disk_atlas_lib::search::kind::{KindClass, KindSet, classify};
use disk_atlas_lib::sizes::ClassBytes;
use gpui::{
    Animation, AnimationExt, AnyElement, ClickEvent, Context, Div, Entity, FocusHandle, FontWeight,
    Hsla, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    ParentElement, Pixels, Point, Render, SharedString, Stateful, Styled, Subscription, Window,
    div, pattern_slash, prelude::*, px, relative, svg,
};

use super::menu::{CloseReason, MenuItem, MenuSlot};
use super::sidebar::tooltip;
use crate::format;
use crate::state::listing::Listing;
use crate::state::navigation::{Location, Navigation};
use crate::state::order::{SortColumn, SortSpec};
use crate::state::properties::{PropertiesTarget, PropertiesWindows, Request};
use crate::state::search::{Filters, IndexState, Search, SearchEvent};
use crate::state::settings::Settings;
use crate::state::sizes::Sizes;
use crate::state::sources::{Availability, SourceId, SourceKind, Sources, SourcesEvent};
use crate::state::view::ViewState;
use crate::theme::{
    Colors, Theme,
    glyphs::Glyph,
    metrics, motion,
    scale::{radius, size, space, text},
};

use super::disk_strip::{self, DiskStrip, SegmentKind};

gpui::actions!(
    storage_bar,
    [
        /// Open the partition the context menu was opened on.
        OpenPartition,
        /// Properties of that partition (R18).
        PartitionProperties,
        /// Search the volume for the kind, largest first.
        ShowLargest,
        /// Show the kind on the space map (R7).
        ShowOnSpaceMap,
    ]
);

/// Legend numbers move at most this often while a volume is being measured.
const LEGEND_INTERVAL: Duration = Duration::from_millis(250);

/// A partition is never narrower than this many pixels.
pub const MIN_PARTITION_PX: u32 = 28;
/// A kind narrower than this many pixels joins Other.
pub const MIN_SEGMENT_PX: u32 = 3;
/// Space between partitions, in pixels.
pub const PARTITION_GAP_PX: u32 = 2;
/// Space between segments, in pixels.
pub const SEGMENT_GAP_PX: u32 = 1;

/// The kinds the bar draws, in the order it draws them. The order never changes, so the bar does
/// not reshuffle while it is measured and two disks can be compared. Other and System follow.
const KIND_ORDER: [KindClass; 10] = [
    KindClass::Image,
    KindClass::Video,
    KindClass::Audio,
    KindClass::Document,
    KindClass::Application,
    KindClass::Code,
    KindClass::Archive,
    KindClass::DiskImage,
    KindClass::Model,
    KindClass::Font,
];

/// The most pieces one partition holds: ten kinds, Other, System, not measured, free.
const MAX_PIECES: usize = 14;

/// One partition of the bar.
#[derive(Debug, Clone, Copy)]
pub struct BarPartition {
    /// Its size in bytes: the bar's scale.
    pub bytes: u64,
    /// The partition being browsed.
    pub current: bool,
    /// What it holds.
    pub fill: Fill,
}

/// What a partition is filled with.
#[derive(Debug, Clone, Copy)]
pub enum Fill {
    /// Kinds are known. `accounted` <= `used`: the rest is drawn as not yet measured.
    Kinds {
        classes: ClassBytes,
        used: u64,
        accounted: u64,
    },
    /// Only the system's numbers.
    UsedFree {
        used: u64,
    },
    /// No reader for this format, or it failed to mount.
    Unreadable,
    Unallocated,
}

/// What one stretch of the bar stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Piece {
    Kind(KindClass),
    System,
    NotMeasured,
    Used,
    Free,
    Hatched,
    Outline,
}

/// A stretch of a partition: `x` counts from the left edge of the whole bar.
#[derive(Debug, PartialEq)]
pub struct Rect1 {
    pub x: f32,
    pub w: f32,
    pub piece: Piece,
}

/// A partition placed on the bar.
pub struct Laid {
    pub x: f32,
    pub w: f32,
    pub pieces: Vec<Rect1>,
}

/// A slot of the pixel apportioning.
#[derive(Clone, Copy, Default)]
struct Slot {
    fixed: bool,
    rest: u128,
}

/// Shares `total` whole pixels among `weights`, each item at least `min`, the rest in proportion
/// to the weights, the sum exactly `total` (largest remainder). When `min` cannot be given to
/// everyone the pixels are shared evenly. `slots` is scratch of the same length.
fn apportion(weights: &[u128], total: u32, min: u32, out: &mut [u32], slots: &mut [Slot]) {
    let n = weights.len().min(out.len()).min(slots.len());
    if n == 0 {
        return;
    }
    let count = u32::try_from(n).unwrap_or(u32::MAX);
    if min.saturating_mul(count) > total {
        let base = total / count;
        let extra = usize::try_from(total % count).unwrap_or(0);
        for (index, slot) in out.iter_mut().take(n).enumerate() {
            *slot = base + u32::from(index < extra);
        }
        return;
    }
    for slot in slots.iter_mut().take(n) {
        *slot = Slot::default();
    }
    // Items whose share would be under `min` get `min`; the others share what is left.
    let (mut remaining, mut weight_sum);
    loop {
        let fixed = slots.iter().take(n).filter(|slot| slot.fixed).count();
        remaining = u128::from(total).saturating_sub(u128::from(min).saturating_mul(fixed as u128));
        weight_sum = weights
            .iter()
            .zip(slots.iter())
            .take(n)
            .filter(|(_, slot)| !slot.fixed)
            .map(|(&w, _)| w)
            .sum::<u128>();
        if weight_sum == 0 {
            break;
        }
        let mut changed = false;
        for (&weight, slot) in weights.iter().zip(slots.iter_mut()).take(n) {
            if !slot.fixed && weight.saturating_mul(remaining) < u128::from(min) * weight_sum {
                slot.fixed = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut given = 0u32;
    for ((&weight, slot), px_out) in weights.iter().zip(slots.iter_mut()).zip(out.iter_mut()) {
        if slot.fixed || weight_sum == 0 {
            *px_out = if slot.fixed { min } else { 0 };
            slot.rest = 0;
        } else {
            let share = weight.saturating_mul(remaining);
            *px_out = u32::try_from(share / weight_sum).unwrap_or(u32::MAX);
            slot.rest = share % weight_sum;
        }
        given = given.saturating_add(*px_out);
    }
    // Leftover pixels go to the largest remainders, earliest first on a tie.
    let mut leftover = total.saturating_sub(given);
    while leftover > 0 {
        let pick = slots
            .iter()
            .take(n)
            .enumerate()
            .filter(|(_, slot)| !slot.fixed)
            .max_by(|a, b| a.1.rest.cmp(&b.1.rest).then(b.0.cmp(&a.0)))
            .map(|(index, _)| index)
            // Nothing to share in proportion: the first item takes the rest.
            .unwrap_or(0);
        if let Some(px_out) = out.get_mut(pick) {
            *px_out = px_out.saturating_add(1);
        }
        if let Some(slot) = slots.get_mut(pick) {
            slot.rest = 0;
            // A fixed item picked here (all fixed) must not be picked forever.
            slot.fixed = slot.fixed && weight_sum != 0;
        }
        leftover -= 1;
    }
}

/// Whole pixels of a width given in pixels.
fn whole_px(width: f32) -> u32 {
    if width.is_finite() && width > 0.0 {
        width.floor().min(u32::MAX as f32) as u32
    } else {
        0
    }
}

/// How the used part of a partition divides: (kind bytes after scaling, other, system,
/// not measured). All of them within `used`.
struct Split {
    kinds: [u64; KIND_ORDER.len()],
    other: u64,
    system: u64,
    not_measured: u64,
    free: u64,
}

impl Split {
    fn of(classes: &ClassBytes, used: u64, accounted: u64, bytes: u64) -> Self {
        let sum = classes.total();
        let (num, den) = if sum > used && sum > 0 {
            (u128::from(used), u128::from(sum))
        } else {
            (1, 1)
        };
        let scaled = |b: u64| -> u64 {
            u64::try_from(u128::from(b).saturating_mul(num) / den).unwrap_or(u64::MAX)
        };
        let mut kinds = [0u64; KIND_ORDER.len()];
        for (slot, &class) in kinds.iter_mut().zip(KIND_ORDER.iter()) {
            *slot = scaled(classes.of(class));
        }
        let other = scaled(
            classes
                .of(KindClass::Other)
                .saturating_add(classes.of(KindClass::Folder)),
        );
        let mut system = scaled(classes.system);
        // Used space no file accounts for is not measured yet, or is the volume's own: catalog,
        // journal, slack.
        let remainder = used.saturating_sub(sum);
        let not_measured = remainder.min(used.saturating_sub(accounted));
        system = system.saturating_add(remainder - not_measured);
        Self {
            kinds,
            other,
            system,
            not_measured,
            free: bytes.saturating_sub(used),
        }
    }
}

/// The pieces of one partition `w` px wide starting at `x`.
fn pieces_of(partition: &BarPartition, x: u32, w: u32) -> Vec<Rect1> {
    let rect = |x: u32, w: u32, piece: Piece| Rect1 {
        x: x as f32,
        w: w as f32,
        piece,
    };
    let mut items: [(Piece, u64); MAX_PIECES] = [(Piece::Free, 0); MAX_PIECES];
    let mut count = 0usize;
    let mut push = |piece: Piece, bytes: u64| {
        if bytes > 0
            && let Some(slot) = items.get_mut(count)
        {
            *slot = (piece, bytes);
            count += 1;
        }
    };
    match partition.fill {
        Fill::Unreadable => return vec![rect(x, w, Piece::Hatched)],
        Fill::Unallocated => return vec![rect(x, w, Piece::Outline)],
        Fill::UsedFree { used } => {
            let used = used.min(partition.bytes);
            push(Piece::Used, used);
            push(Piece::Free, partition.bytes - used);
        }
        Fill::Kinds {
            classes,
            used,
            accounted,
        } => {
            let used = used.min(partition.bytes);
            let split = Split::of(&classes, used, accounted.min(used), partition.bytes);
            let mut other = split.other;
            // A kind that would be under `MIN_SEGMENT_PX` joins Other.
            let denominator = u128::from(partition.bytes.max(1));
            let mut kept = [0u64; KIND_ORDER.len()];
            for (keep, &bytes) in kept.iter_mut().zip(split.kinds.iter()) {
                let ideal = u128::from(bytes).saturating_mul(u128::from(w)) / denominator;
                if ideal < u128::from(MIN_SEGMENT_PX) {
                    other = other.saturating_add(bytes);
                } else {
                    *keep = bytes;
                }
            }
            for (&class, &bytes) in KIND_ORDER.iter().zip(kept.iter()) {
                push(Piece::Kind(class), bytes);
            }
            push(Piece::Kind(KindClass::Other), other);
            push(Piece::System, split.system);
            push(Piece::NotMeasured, split.not_measured);
            push(Piece::Free, split.free);
        }
    }
    if count == 0 {
        return vec![rect(x, w, Piece::Free)];
    }
    let gaps = u32::try_from(count - 1)
        .unwrap_or(u32::MAX)
        .saturating_mul(SEGMENT_GAP_PX);
    let inner = w.saturating_sub(gaps);
    let mut weights = [0u128; MAX_PIECES];
    for (weight, item) in weights.iter_mut().zip(items.iter()).take(count) {
        *weight = u128::from(item.1);
    }
    let mut widths = [0u32; MAX_PIECES];
    let mut slots = [Slot::default(); MAX_PIECES];
    apportion(
        &weights[..count],
        inner,
        MIN_SEGMENT_PX,
        &mut widths[..count],
        &mut slots[..count],
    );
    let mut out = Vec::with_capacity(count);
    let mut at = x;
    for (item, &width) in items.iter().zip(widths.iter()).take(count) {
        out.push(rect(at, width, item.0));
        at = at.saturating_add(width).saturating_add(SEGMENT_GAP_PX);
    }
    out
}

/// `width` in px. Partitions to scale with `MIN_PARTITION_PX` and 2 px gaps; inside, the fixed
/// kind order, `MIN_SEGMENT_PX`, 1 px gaps. One `Vec` per partition for the pieces. Widths are
/// whole pixels and, with the gaps, add up to `width`.
pub fn layout(partitions: &[BarPartition], width: f32) -> Vec<Laid> {
    let count = partitions.len();
    if count == 0 {
        return Vec::new();
    }
    let gaps = u32::try_from(count - 1)
        .unwrap_or(u32::MAX)
        .saturating_mul(PARTITION_GAP_PX);
    let available = whole_px(width).saturating_sub(gaps);
    let weights: Vec<u128> = partitions.iter().map(|p| u128::from(p.bytes)).collect();
    let mut widths = vec![0u32; count];
    let mut slots = vec![Slot::default(); count];
    apportion(
        &weights,
        available,
        MIN_PARTITION_PX,
        &mut widths,
        &mut slots,
    );
    let mut out = Vec::with_capacity(count);
    let mut at = 0u32;
    for (partition, &w) in partitions.iter().zip(widths.iter()) {
        out.push(Laid {
            x: at as f32,
            w: w as f32,
            pieces: pieces_of(partition, at, w),
        });
        at = at.saturating_add(w).saturating_add(PARTITION_GAP_PX);
    }
    out
}

/// True when a kind of `bytes` in `denominator` would be under `MIN_SEGMENT_PX` on `width` px.
fn is_folded(bytes: u64, denominator: u64, width: f64) -> bool {
    bytes as f64 / denominator as f64 * width < f64::from(MIN_SEGMENT_PX)
}

/// The kinds a chip or segment of `class` stands for, so that a click finds the items the chip
/// counts: Other is Other, folders and every kind folded into it. `bar_width` is as for
/// [`legend`].
pub fn kinds_of(classes: &ClassBytes, used: u64, bar_width: f32, class: KindClass) -> KindSet {
    let set = KindSet::EMPTY.with(class);
    if class != KindClass::Other {
        return set;
    }
    let denominator = used.max(classes.total());
    let width = f64::from(bar_width.max(0.0));
    let mut set = set;
    if classes.of(KindClass::Folder) > 0 {
        set = set.with(KindClass::Folder);
    }
    for &kind in &KIND_ORDER {
        let bytes = classes.of(kind);
        if bytes > 0 && denominator > 0 && is_folded(bytes, denominator, width) {
            set = set.with(kind);
        }
    }
    set
}

/// `(piece, x, w)` of a partition's pieces, for easing between two layouts.
type Shown = Vec<(Piece, f32, f32)>;

/// The pieces of `to` at progress `t` (0 to 1) from `from`: a piece in both moves from its old
/// place to its new one; a piece that is new grows from nothing where it will be.
fn ease_pieces(from: &[(Piece, f32, f32)], to: &[(Piece, f32, f32)], t: f32) -> Shown {
    let t = t.clamp(0.0, 1.0);
    to.iter()
        .map(|&(piece, x, w)| {
            let (fx, fw) = from
                .iter()
                .find(|(p, ..)| *p == piece)
                .map_or((x, 0.0), |&(_, fx, fw)| (fx, fw));
            (piece, fx + (x - fx) * t, fw + (w - fw) * t)
        })
        .collect()
}

/// The pieces of a partition at `origin`, `width` px wide, with the used part drawn `t` of the
/// way (0 to 1) from the left edge and the free space taking the rest.
fn grow_pieces(pieces: &[(Piece, f32, f32)], origin: f32, width: f32, t: f32) -> Shown {
    let t = t.clamp(0.0, 1.0);
    let end = origin + width;
    let mut out: Shown = Vec::with_capacity(pieces.len());
    for &(piece, x, w) in pieces {
        if piece == Piece::Free {
            let start = out
                .last()
                .map_or(origin, |&(_, px, pw)| px + pw + 1.0)
                .min(end);
            out.push((piece, start, (end - start).max(0.0)));
        } else {
            out.push((piece, origin + (x - origin) * t, w * t));
        }
    }
    out
}

/// The legend's chips, in the bar's order: the kinds that got a segment, the ones too narrow
/// folded into Other, then System. `bar_width` is the width in px that the used space takes.
/// Their bytes add up to `used` (or `classes.total()` when that is more).
pub fn legend(classes: &ClassBytes, used: u64, bar_width: f32) -> Vec<(Piece, u64)> {
    let denominator = used.max(classes.total());
    if denominator == 0 {
        return Vec::new();
    }
    let width = f64::from(bar_width.max(0.0));
    let mut out = Vec::with_capacity(MAX_PIECES);
    let mut other = classes
        .of(KindClass::Other)
        .saturating_add(classes.of(KindClass::Folder));
    for &class in &KIND_ORDER {
        let bytes = classes.of(class);
        if bytes == 0 {
            continue;
        }
        if is_folded(bytes, denominator, width) {
            other = other.saturating_add(bytes);
        } else {
            out.push((Piece::Kind(class), bytes));
        }
    }
    if other > 0 {
        out.push((Piece::Kind(KindClass::Other), other));
    }
    // What the volume uses that no file accounts for (catalog, journal, slack) is System too,
    // as in the bar.
    let system = classes
        .system
        .saturating_add(used.saturating_sub(classes.total()));
    if system > 0 {
        out.push((Piece::System, system));
    }
    out
}

/// The kind's name as the bar and legend say it.
pub fn piece_name(piece: Piece) -> &'static str {
    match piece {
        Piece::Kind(KindClass::Image) => "Pictures",
        Piece::Kind(KindClass::Video) => "Movies",
        Piece::Kind(KindClass::Audio) => "Audio",
        Piece::Kind(KindClass::Document) => "Documents",
        Piece::Kind(KindClass::Application) => "Applications",
        Piece::Kind(KindClass::Code) => "Code and data",
        Piece::Kind(KindClass::Archive) => "Archives",
        Piece::Kind(KindClass::DiskImage) => "Disk images",
        Piece::Kind(KindClass::Model) => "3D models",
        Piece::Kind(KindClass::Font) => "Fonts",
        Piece::Kind(KindClass::Other | KindClass::Folder) => "Other",
        Piece::System => "System",
        Piece::NotMeasured => "Not measured yet",
        Piece::Used => "Used",
        Piece::Free => "Free",
        Piece::Hatched => "Unreadable",
        Piece::Outline => "Unallocated",
    }
}

/// "38 %" of `part` in `whole`, rounded; "under 1 %" for a sliver.
fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "0 %".to_owned();
    }
    let hundredths = u128::from(part) * 100 / u128::from(whole);
    if hundredths == 0 && part > 0 {
        "under 1 %".to_owned()
    } else {
        format!("{hundredths} %")
    }
}

/// What a click or Enter on a stop does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Act {
    /// Open this partition.
    Open(SourceId),
    /// Search the current volume for this kind, largest first.
    Search(KindClass),
    Nothing,
}

/// A stop of the keyboard walk: a partition, or a segment of the current one.
#[derive(Clone)]
struct Stop {
    name: SharedString,
    act: Act,
}

/// What a partition is, as the view knows it.
struct PartInfo {
    source: Option<SourceId>,
    name: SharedString,
    /// "252.7 GB used of 465.1 GB", the label's second line.
    caption: SharedString,
    current: bool,
    bytes: u64,
    used: Option<u64>,
}

/// What the right end of the first line says besides a selection.
enum Status {
    None,
    /// The file list is being read; the number is how many entries so far.
    Reading(u64),
    /// The index is built and the table is not.
    Measuring,
    /// "at least X counted".
    LowerBound(u64),
    /// Sizes are off: offer to calculate them.
    Calculate(SourceId),
}

/// Everything a frame draws, worked out from the entities before painting.
struct Model {
    title: SharedString,
    detail: SharedString,
    parts: Vec<PartInfo>,
    bars: Vec<BarPartition>,
    current: Option<usize>,
    /// The kinds of the current partition, for the legend and tooltips.
    classes: Option<(ClassBytes, f32)>,
    status: Status,
    /// The current source is on a disk Disk Atlas reads: Layout mode is offered.
    is_disk: bool,
}

/// The storage bar view.
pub struct StorageBar {
    sources: Entity<Sources>,
    navigation: Entity<Navigation>,
    sizes: Entity<Sizes>,
    search: Entity<Search>,
    listing: Entity<Listing>,
    /// Layout mode is offered in the disk view only.
    view: Entity<ViewState>,
    /// Layout mode's drawing (`r1-redesign.md` 3.3), kept as it was.
    disk_strip: Entity<DiskStrip>,
    focus: FocusHandle,
    /// The piece the pointer is on; the others of its partition dim.
    hover: Option<Piece>,
    /// The stop the keyboard is on.
    cursor: usize,
    stops: Vec<Stop>,
    /// A kind search was started and its results should be sorted by size once they show.
    pending_sort: bool,
    /// The last input was a key: only then is the keyboard stop drawn.
    by_keyboard: bool,
    /// The context menu being shown, and what its items act on.
    menu: MenuSlot,
    menu_act: Act,
    /// What the current partition's kinds fold into Other: the legend inputs, as last drawn.
    fold: Option<(ClassBytes, u64, f32)>,
    /// Volumes whose kinds have already grown in.
    grown: Vec<SourceId>,
    /// The grow-in being played.
    grow: Option<(SourceId, Instant)>,
    /// The width easing being played.
    ease: Option<(Instant, Shown)>,
    /// What was drawn last, to see what changed.
    prev: Option<Prev>,
    /// The legend as last shown while measuring, so its numbers move four times a second at most.
    legend_shown: Option<(Instant, Vec<(Piece, u64)>)>,
    legend_timer: Option<gpui::Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// What the current partition looked like in the last frame.
struct Prev {
    source: Option<SourceId>,
    width: f32,
    target: Shown,
    shown: Shown,
}

impl StorageBar {
    /// Draws the disk of the current location. `search` and `listing` carry out a click on a
    /// kind; `sizes` holds the bytes per kind; `view` says whether the disk view is open.
    #[allow(
        clippy::too_many_arguments,
        reason = "the bar reads these entities and nothing else"
    )]
    pub fn new(
        sources: Entity<Sources>,
        navigation: Entity<Navigation>,
        sizes: Entity<Sizes>,
        search: Entity<Search>,
        listing: Entity<Listing>,
        view: Entity<ViewState>,
        disk_strip: Entity<DiskStrip>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.observe(&sources, |_, _, cx| cx.notify()),
            cx.observe(&navigation, |_, _, cx| cx.notify()),
            cx.observe(&sizes, |_, _, cx| cx.notify()),
            cx.observe(&listing, |_, _, cx| cx.notify()),
            cx.observe(&view, |_, _, cx| cx.notify()),
            cx.observe_global::<Settings>(|_, cx| cx.notify()),
            cx.subscribe(&sources, |_, _, _: &SourcesEvent, cx| cx.notify()),
            cx.subscribe(&search, |this, _, event: &SearchEvent, cx| {
                if matches!(event, SearchEvent::Changed) {
                    this.sort_pending_results(cx);
                }
                cx.notify();
            }),
        ];
        Self {
            sources,
            navigation,
            sizes,
            search,
            listing,
            view,
            disk_strip,
            focus: cx.focus_handle().tab_stop(true),
            hover: None,
            cursor: 0,
            stops: Vec::new(),
            pending_sort: false,
            by_keyboard: false,
            menu: MenuSlot::default(),
            menu_act: Act::Nothing,
            fold: None,
            grown: Vec::new(),
            grow: None,
            ease: None,
            prev: None,
            legend_shown: None,
            legend_timer: None,
            _subscriptions: subscriptions,
        }
    }

    /// The results of a kind search are shown: largest first.
    fn sort_pending_results(&mut self, cx: &mut Context<Self>) {
        if self.pending_sort && self.listing.read(cx).in_results() {
            self.pending_sort = false;
            self.listing.update(cx, |listing, cx| {
                listing.set_sort(
                    SortSpec {
                        column: SortColumn::Size,
                        descending: true,
                    },
                    cx,
                );
            });
        }
    }

    fn perform(&mut self, act: Act, cx: &mut Context<Self>) {
        match act {
            Act::Open(source) => {
                let to = self.sources.read(cx).source(source).map(Location::root);
                if let Some(to) = to {
                    self.navigation
                        .update(cx, |navigation, cx| navigation.navigate(to, cx));
                }
            }
            Act::Search(class) => {
                self.pending_sort = true;
                // The kinds the chip counts: Other holds the folded kinds too.
                let kinds = match &self.fold {
                    Some((classes, used, width)) => kinds_of(classes, *used, *width, class),
                    None => KindSet::EMPTY.with(class),
                };
                self.search.update(cx, |search, cx| {
                    search.activate(cx);
                    let filters = Filters {
                        kinds,
                        this_folder: false,
                        // The bar counts hidden files, so this search shows them: the numbers match. The filter
                        // ends with the search; the setting is not touched.
                        include_hidden: true,
                        ..search.filters().clone()
                    };
                    search.set_filters(filters, cx);
                });
                self.sort_pending_results(cx);
            }
            Act::Nothing => {}
        }
    }

    /// Opens the context menu of a stop (`r10-everyday.md` 4): a partition has Open and
    /// Properties, a kind has "Show the largest ..." and "Show on the space map".
    fn open_menu(
        &mut self,
        act: Act,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let items = match act {
            Act::Open(_) => vec![
                MenuItem::action("Open", OpenPartition),
                MenuItem::action("Properties", PartitionProperties),
            ],
            Act::Search(class) => vec![
                MenuItem::action(
                    format!("Show the largest {}", piece_name(Piece::Kind(class))),
                    ShowLargest,
                ),
                MenuItem::action("Show on the space map", ShowOnSpaceMap),
            ],
            Act::Nothing => return,
        };
        self.menu_act = act;
        window.focus(&self.focus);
        let focus = self.focus.clone();
        self.menu.show(
            items,
            at,
            Some(focus),
            window,
            cx,
            |this, reason, window, cx| {
                this.menu.clear();
                // `menu_act` stays: a chosen item's action arrives after the menu closed.
                if matches!(reason, CloseReason::Escape) {
                    window.focus(&this.focus);
                }
                cx.notify();
            },
        );
    }

    /// An item of the menu was chosen.
    fn choose(&mut self, _: &OpenPartition, _: &mut Window, cx: &mut Context<Self>) {
        let act = self.menu_act;
        self.perform(act, cx);
    }

    /// "Show on the space map": the map, with the kind's regions emphasised.
    fn show_on_map(&mut self, _: &ShowOnSpaceMap, _: &mut Window, cx: &mut Context<Self>) {
        if let Act::Search(class) = self.menu_act {
            self.view
                .update(cx, |view, cx| view.show_kind_on_map(class, cx));
        }
    }

    fn choose_kind(&mut self, _: &ShowLargest, _: &mut Window, cx: &mut Context<Self>) {
        let act = self.menu_act;
        self.perform(act, cx);
    }

    /// "Properties" of the partition the menu was opened on.
    fn partition_properties(
        &mut self,
        _: &PartitionProperties,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Act::Open(source) = self.menu_act {
            PropertiesWindows::open_here(
                PropertiesTarget::Volume { source },
                Request::default(),
                cx,
            );
        }
    }

    /// Works out what to draw, or `None` when nothing is to be drawn here.
    fn model(&self, cx: &gpui::App) -> Option<Model> {
        let sources = self.sources.read(cx);
        let sizes = self.sizes.read(cx);
        let search = self.search.read(cx);
        let current = self.navigation.read(cx).current()?.source();
        let current_source = sources.source(current)?;
        match current_source.kind {
            SourceKind::Host => {
                // Drives of this computer only: Home and folders are not a drive.
                let root = sources.host_root(current)?;
                if !is_drive_root(root) {
                    return None;
                }
                let info = current_source.fs.info();
                let total = info.total_bytes?;
                let used = total.saturating_sub(info.free_bytes.unwrap_or(0));
                // TODO(R14): HostSizes gives a drive's kinds and "See what fills this drive".
                // Until then a host drive draws the system's used and free only.
                let name = current_source.label.clone();
                Some(Model {
                    title: name.clone(),
                    detail: format::size(total).to_string().into(),
                    parts: vec![PartInfo {
                        source: Some(current),
                        name,
                        caption: used_of(used, total).into(),
                        current: true,
                        bytes: total,
                        used: Some(used),
                    }],
                    bars: vec![BarPartition {
                        bytes: total,
                        current: true,
                        fill: Fill::UsedFree { used },
                    }],
                    current: Some(0),
                    classes: None,
                    status: Status::None,
                    is_disk: false,
                })
            }
            SourceKind::WholeDisk { disk } => {
                let disk = sources.disk(disk)?;
                Some(Model {
                    title: disk.label.clone(),
                    detail: detail_of(disk.size).into(),
                    parts: vec![PartInfo {
                        source: Some(current),
                        name: current_source.label.clone(),
                        caption: "Cannot be read".into(),
                        current: true,
                        bytes: disk.size,
                        used: None,
                    }],
                    bars: vec![BarPartition {
                        bytes: disk.size,
                        current: true,
                        fill: Fill::Unreadable,
                    }],
                    current: Some(0),
                    classes: None,
                    status: Status::None,
                    is_disk: true,
                })
            }
            SourceKind::Volume { disk, .. } => {
                let disk = sources.disk(disk)?;
                let volumes: Vec<(SourceId, u64, u64)> = disk
                    .volumes
                    .iter()
                    .filter_map(|&id| match sources.source(id)?.kind {
                        SourceKind::Volume { start, len, .. } => Some((id, start, len)),
                        SourceKind::Host | SourceKind::WholeDisk { .. } => None,
                    })
                    .collect();
                let ranges: Vec<(u64, u64)> = volumes.iter().map(|&(_, s, l)| (s, l)).collect();
                let mut parts = Vec::new();
                let mut bars = Vec::new();
                let mut current_index = None;
                let mut current_classes = None;
                for segment in disk_strip::layout(disk.size, &ranges) {
                    let SegmentKind::Volume(index) = segment.kind else {
                        parts.push(PartInfo {
                            source: None,
                            name: "Unallocated".into(),
                            caption: format::size(segment.len).into(),
                            current: false,
                            bytes: segment.len,
                            used: None,
                        });
                        bars.push(BarPartition {
                            bytes: segment.len,
                            current: false,
                            fill: Fill::Unallocated,
                        });
                        continue;
                    };
                    let Some(&(id, _, _)) = volumes.get(index) else {
                        continue;
                    };
                    let Some(source) = sources.source(id) else {
                        continue;
                    };
                    let is_current = id == current;
                    let info = source.fs.info();
                    let used = info
                        .total_bytes
                        .zip(info.free_bytes)
                        .map(|(total, free)| total.saturating_sub(free));
                    let (fill, caption) = if !source.availability.is_browsable() {
                        let label = match (&source.availability, &source.format) {
                            (Availability::Unsupported { format }, _) => format.to_string(),
                            (_, Some(format)) => format.to_string(),
                            _ => "Cannot be read".to_owned(),
                        };
                        (Fill::Unreadable, label)
                    } else if let Some((classes, scale)) = sizes.classes(id) {
                        let used = used.unwrap_or_else(|| classes.total());
                        if is_current {
                            current_classes = Some((classes, scale));
                        }
                        (
                            Fill::Kinds {
                                classes,
                                used,
                                accounted: used,
                            },
                            used_of(used, segment.len),
                        )
                    } else if is_current
                        && matches!(
                            search.index_state(id),
                            Some(IndexState::Building { .. } | IndexState::Ready(_))
                        )
                    {
                        // The file list is read, or the table is being made: the used part is
                        // not measured yet.
                        let used = used.unwrap_or(0);
                        (
                            Fill::Kinds {
                                classes: ClassBytes::default(),
                                used,
                                accounted: 0,
                            },
                            used_of(used, segment.len),
                        )
                    } else {
                        let used = used.unwrap_or(0);
                        (Fill::UsedFree { used }, used_of(used, segment.len))
                    };
                    if is_current {
                        current_index = Some(bars.len());
                    }
                    let known_used = match fill {
                        Fill::Kinds { used, .. } | Fill::UsedFree { used } => Some(used),
                        _ => None,
                    };
                    parts.push(PartInfo {
                        source: Some(id),
                        name: source.label.clone(),
                        caption: caption.into(),
                        current: is_current,
                        bytes: segment.len,
                        used: known_used,
                    });
                    bars.push(BarPartition {
                        bytes: segment.len,
                        current: is_current,
                        fill,
                    });
                }
                let status = match search.index_state(current) {
                    _ if sizes.classes(current).is_some() => {
                        if sizes.is_lower_bound(current) {
                            Status::LowerBound(
                                current_classes.map_or(0, |(classes, _)| classes.total()),
                            )
                        } else {
                            Status::None
                        }
                    }
                    Some(IndexState::Building { entries }) => Status::Reading(*entries),
                    Some(IndexState::Ready(_)) => Status::Measuring,
                    Some(IndexState::Failed(_)) => Status::None,
                    Some(IndexState::Absent) | None => {
                        if index_on_open(cx) {
                            Status::Reading(0)
                        } else {
                            Status::Calculate(current)
                        }
                    }
                };
                Some(Model {
                    title: disk.label.clone(),
                    detail: detail_of(disk.size).into(),
                    parts,
                    bars,
                    current: current_index,
                    classes: current_classes,
                    status,
                    is_disk: true,
                })
            }
        }
    }
}

/// The most selected rows read for their bytes.
const SELECTION_ROWS: usize = 4096;

/// What the selection weighs: how many items, how many bytes, and for one file its kind. Folders
/// count by their totals when those are known; files by their size. `None` when nothing has a
/// size to show.
fn selection_measure(listing: &Listing) -> Option<(usize, u64, Option<KindClass>)> {
    let count = listing.selected_count();
    if count == 0 {
        return None;
    }
    if let Some(bytes) = listing.selection_total() {
        return Some((count, bytes, None));
    }
    let positions = listing.selected_positions(SELECTION_ROWS);
    let (mut bytes, mut files, mut class) = (0u64, 0usize, None);
    for position in &positions {
        let Some(row) = listing.row(*position) else {
            continue;
        };
        if row.entry.is_browsable() {
            continue;
        }
        files += 1;
        bytes = bytes.saturating_add(row.entry.size.unwrap_or(0));
        class = Some(classify(row.entry.name, None));
    }
    if files == 0 {
        return None;
    }
    Some((
        count,
        bytes,
        (count == 1 && files == 1).then_some(class).flatten(),
    ))
}

/// "252.7 GB used of 465.1 GB".
fn used_of(used: u64, total: u64) -> String {
    format!("{} used of {}", format::size(used), format::size(total))
}

/// "931.5 GB, read-only": the app never writes to a disk, so it says so.
fn detail_of(size: u64) -> String {
    if size == 0 {
        "read-only".to_owned()
    } else {
        format!("{}, read-only", format::size(size))
    }
}

/// True for the top of a drive: `C:\`, `/`, or a mount under the usual mount folders.
pub(crate) fn is_drive_root(root: &std::path::Path) -> bool {
    if root.parent().is_none() {
        return true;
    }
    ["/Volumes", "/media", "/mnt", "/run/media"]
        .iter()
        .any(|base| {
            root.starts_with(base)
                && root.components().count() <= 3 + usize::from(*base == "/run/media")
        })
}

/// The setting "Read disks when opened". On when there are no settings (tests).
fn index_on_open(cx: &gpui::App) -> bool {
    cx.try_global::<Settings>()
        .is_none_or(|settings| settings.bool_of("index_on_open"))
}

/// The storage bar's setting: `full`, `compact` or `hidden`.
fn mode(cx: &gpui::App) -> &'static str {
    cx.try_global::<Settings>()
        .map_or("full", |settings| settings.choice_of("storage_bar"))
}

/// The color of a piece of the bar, before dimming.
fn piece_color(piece: Piece, colors: &Colors) -> Hsla {
    match piece {
        Piece::Kind(class) => colors.tint(Glyph::from_class(class)),
        Piece::System => colors.relief.opacity(metrics::STORAGE_SYSTEM_OPACITY),
        Piece::Used => colors.relief.opacity(metrics::STORAGE_USED_OPACITY),
        Piece::NotMeasured => colors.track,
        Piece::Free => colors.water,
        Piece::Hatched | Piece::Outline => colors.track,
    }
}

impl Render for StorageBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = mode(cx);
        let colors = Theme::get(cx).colors;
        let model = if mode == "hidden" {
            None
        } else {
            self.model(cx)
        };
        let Some(model) = model else {
            self.stops.clear();
            return div().id("storage-bar");
        };
        // Contents | Layout belongs to the disk view.
        let in_disk_view = self.view.read(cx).disk().is_some();
        let switch = (model.is_disk && in_disk_view).then(|| self.mode_switch(&colors, cx));
        if self.view.read(cx).layout_mode() && model.is_disk && in_disk_view {
            // Layout mode: the strip of r1-redesign.md 3.3, unchanged, with the switch over it.
            self.stops.clear();
            return div()
                .id("storage-bar")
                .relative()
                .flex_none()
                .child(self.disk_strip.clone())
                .when_some(switch, |this, switch| {
                    this.child(
                        div()
                            .absolute()
                            .top(space::S8)
                            .right(space::S16)
                            .child(switch),
                    )
                });
        }
        let reduced = motion::reduced(cx);
        // The bar spans the content panel: the window less the sidebar and the side padding.
        let width = f32::from(window.viewport_size().width)
            - f32::from(size::SIDEBAR)
            - 2.0 * f32::from(space::S16);
        let mut laid = layout(&model.bars, width);
        if self.animate(&model, &mut laid, reduced, window) {
            crate::frame_trace::note(cx, "storage-bar");
        }

        // The legend's chips and every tooltip read the current partition's kinds.
        let current = model.current.and_then(|index| {
            let bar = model.bars.get(index)?;
            let part = model.parts.get(index)?;
            let laid = laid.get(index)?;
            Some((index, bar, part, laid))
        });
        let fold = match (current, &model.classes) {
            (Some((_, bar, _, laid)), Some((classes, _))) => {
                let (used, bytes) = match bar.fill {
                    Fill::Kinds { used, .. } => (used, bar.bytes.max(1)),
                    _ => (0, bar.bytes.max(1)),
                };
                Some((*classes, used, laid.w * (used as f32 / bytes as f32)))
            }
            _ => None,
        };
        self.fold = fold;
        let mut chips: Vec<(Piece, u64)> = fold.map_or_else(Vec::new, |(classes, used, width)| {
            legend(&classes, used, width)
        });
        // While measuring, the numbers move four times a second at most.
        let measuring = matches!(model.status, Status::Reading(_) | Status::Measuring)
            || matches!(
                current.map(|c| c.1.fill),
                Some(Fill::Kinds { used, accounted, .. }) if accounted < used
            );
        if measuring {
            let hold = match &self.legend_shown {
                Some((at, shown)) if *shown != chips && at.elapsed() < LEGEND_INTERVAL => {
                    Some(shown.clone())
                }
                Some((_, shown)) if *shown == chips => None,
                _ => {
                    self.legend_shown = Some((Instant::now(), chips.clone()));
                    None
                }
            };
            if let Some(shown) = hold {
                chips = shown;
                if self.legend_timer.is_none() {
                    self.legend_timer = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(LEGEND_INTERVAL).await;
                        this.update(cx, |this, cx| {
                            this.legend_timer = None;
                            cx.notify();
                        })
                        .ok();
                    }));
                }
            }
        } else {
            self.legend_shown = None;
        }
        let used_total = current.and_then(|(_, _, part, _)| part.used).unwrap_or(0);

        // The keyboard walk: each partition, and the current one's segments after it.
        let mut stops: Vec<Stop> = Vec::new();
        let mut stop_of: Vec<(usize, Option<Piece>)> = Vec::new();
        for (index, part) in model.parts.iter().enumerate() {
            stops.push(Stop {
                name: format!("{}, {}", part.name, part.caption).into(),
                act: part.source.map_or(Act::Nothing, Act::Open),
            });
            stop_of.push((index, None));
            if Some(index) == model.current {
                for &(piece, bytes) in &chips {
                    let act = match piece {
                        Piece::Kind(class) => Act::Search(class),
                        _ => Act::Nothing,
                    };
                    stops.push(Stop {
                        name: format!(
                            "{}, {}, {} of what is used",
                            piece_name(piece),
                            format::size(bytes),
                            percent(bytes, used_total)
                        )
                        .into(),
                        act,
                    });
                    stop_of.push((index, Some(piece)));
                }
            }
        }
        self.cursor = self.cursor.min(stops.len().saturating_sub(1));
        let focused = self.focus.is_focused(window) && self.by_keyboard;
        let cursor_at = if focused {
            stop_of.get(self.cursor).copied()
        } else {
            None
        };
        self.stops = stops;

        // First line: the disk on the left; on the right the selection, the keyboard's stop, or
        // what the measuring is up to.
        let (selection, selected_class) = match selection_measure(self.listing.read(cx)) {
            Some((count, bytes, class)) => (Some((count, bytes)), class),
            None => (None, None),
        };
        let right = self.right_text(
            &model,
            selection,
            focused,
            current.map(|c| c.2),
            &colors,
            cx,
        );

        let bar_row = self.bar_row(&model, &laid, reduced, cursor_at, &colors, cx);
        let bracket = selection.and_then(|(_, bytes)| {
            let (_, bar, _, laid) = current?;
            let scaled = (bytes as f64 / bar.bytes.max(1) as f64 * f64::from(laid.w)) as f32;
            // A file's measure starts at its kind's segment; a folder's or several items', at the
            // partition's left edge.
            let start = selected_class
                .and_then(|class| {
                    let wanted = Piece::Kind(class);
                    laid.pieces.iter().find(|r| r.piece == wanted).or_else(|| {
                        laid.pieces
                            .iter()
                            .find(|r| r.piece == Piece::Kind(KindClass::Other))
                    })
                })
                .map_or(laid.x, |r| r.x);
            let scaled = scaled.min((laid.x + laid.w - start).max(2.0));
            Some(
                div()
                    .absolute()
                    .top_0()
                    .left(px(start))
                    .w(px(scaled.clamp(2.0, laid.w.max(2.0))))
                    .h(metrics::STORAGE_BRACKET_HEIGHT)
                    .border_l_1()
                    .border_r_1()
                    .border_b_1()
                    .border_color(colors.pin),
            )
        });
        let labels = div()
            .relative()
            .w_full()
            .h(metrics::STORAGE_LABEL_HEIGHT)
            .children(model.parts.iter().zip(laid.iter()).map(|(part, laid)| {
                div()
                    .absolute()
                    .top_0()
                    .left(px(laid.x))
                    .w(px(laid.w))
                    .flex()
                    .flex_col()
                    .text_size(text::SMALL.size)
                    .overflow_hidden()
                    .child(
                        div()
                            .truncate()
                            .text_color(colors.ink)
                            .font_weight(if part.current {
                                FontWeight::BOLD
                            } else {
                                FontWeight::NORMAL
                            })
                            .child(part.name.clone()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_color(colors.ink2)
                            .child(part.caption.clone()),
                    )
            }));
        let legend_row = if mode == "full" && !chips.is_empty() {
            Some(self.legend_row(&chips, used_total, width, cursor_at, &colors, cx))
        } else {
            None
        };

        div()
            .id("storage-bar")
            .key_context("StorageBar")
            .track_focus(&self.focus)
            .capture_any_mouse_down(cx.listener(|this, _: &MouseDownEvent, _, _| {
                this.by_keyboard = false;
            }))
            .on_action(cx.listener(Self::choose))
            .on_action(cx.listener(Self::choose_kind))
            .on_action(cx.listener(Self::show_on_map))
            .on_action(cx.listener(Self::partition_properties))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let last = this.stops.len().saturating_sub(1);
                let keys = event.keystroke.modifiers;
                this.by_keyboard = true;
                match event.keystroke.key.as_str() {
                    // The Menu key and Shift+F10 open the menu of the stop.
                    "menu" => {
                        let act = this.stops.get(this.cursor).map_or(Act::Nothing, |s| s.act);
                        this.open_menu(act, window.mouse_position(), window, cx);
                    }
                    "f10" if keys.shift => {
                        let act = this.stops.get(this.cursor).map_or(Act::Nothing, |s| s.act);
                        this.open_menu(act, window.mouse_position(), window, cx);
                    }
                    "left" => this.cursor = this.cursor.saturating_sub(1),
                    "right" => this.cursor = (this.cursor + 1).min(last),
                    "enter" => {
                        if let Some(stop) = this.stops.get(this.cursor) {
                            let act = stop.act;
                            this.perform(act, cx);
                        }
                    }
                    _ => return,
                }
                cx.stop_propagation();
                cx.notify();
            }))
            .flex_none()
            .flex()
            .flex_col()
            .gap(space::S8)
            .pt(space::S12)
            .px(space::S16)
            .pb(space::S12)
            .border_b_1()
            .border_color(colors.line)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .justify_between()
                    .gap(metrics::STRIP_HEADER_GAP)
                    .child(
                        div()
                            .min_w_0()
                            .flex()
                            .flex_row()
                            .items_baseline()
                            .gap(metrics::STRIP_HEADER_GAP)
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .font_weight(FontWeight::BOLD)
                                    .child(model.title.clone()),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(colors.ink3)
                                    .child(model.detail.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(space::S12)
                            .child(right)
                            .when_some(switch, ParentElement::child),
                    ),
            )
            .child(bar_row)
            // The bracket row is always there, so the bar does not jump when a selection appears.
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(metrics::STORAGE_BRACKET_HEIGHT)
                    .when_some(bracket, ParentElement::child),
            )
            .child(labels)
            .when_some(legend_row, ParentElement::child)
            .when_some(self.menu.entity(), ParentElement::child)
    }
}

impl StorageBar {
    /// The motion of the current partition (`r10-everyday.md` 4): when a volume's kinds arrive
    /// they grow in from the left over EMPHASIZED, once per volume; when the pieces change after
    /// that they ease over STANDARD. With reduced motion nothing moves. `laid` is changed to what
    /// this frame draws. True while something moves.
    fn animate(
        &mut self,
        model: &Model,
        laid: &mut [Laid],
        reduced: bool,
        window: &mut Window,
    ) -> bool {
        let Some(index) = model.current else {
            self.prev = None;
            self.ease = None;
            return false;
        };
        let (Some(part), Some(l)) = (model.parts.get(index), laid.get_mut(index)) else {
            return false;
        };
        let source = part.source;
        let now = Instant::now();
        let target: Shown = l.pieces.iter().map(|r| (r.piece, r.x, r.w)).collect();
        if reduced {
            self.grow = None;
            self.ease = None;
        } else if let Some(id) = source
            && model.classes.is_some()
            && !self.grown.contains(&id)
        {
            self.grown.push(id);
            self.grow = Some((id, now));
            self.ease = None;
        }
        let progress = |start: Instant, timing: motion::Timing| {
            let t = now.duration_since(start).as_secs_f32() / timing.duration.as_secs_f32();
            (t < 1.0).then(|| timing.easing.apply(t))
        };
        let mut shown = target.clone();
        let mut moving = false;
        let growing = self
            .grow
            .filter(|(id, _)| Some(*id) == source)
            .and_then(|(_, start)| progress(start, motion::EMPHASIZED));
        if let Some(t) = growing {
            shown = grow_pieces(&target, l.x, l.w, t);
            moving = true;
        } else if !reduced {
            if let Some(prev) = &self.prev
                && prev.source == source
                && (prev.width - l.w).abs() < 0.5
                && prev.target != target
            {
                self.ease = Some((now, prev.shown.clone()));
            }
            if let Some((start, from)) = &self.ease {
                match progress(*start, motion::STANDARD) {
                    Some(t) => {
                        shown = ease_pieces(from, &target, t);
                        moving = true;
                    }
                    None => self.ease = None,
                }
            }
        }
        if moving {
            window.request_animation_frame();
        }
        for (rect, &(_, x, w)) in l.pieces.iter_mut().zip(shown.iter()) {
            rect.x = x;
            rect.w = w;
        }
        self.prev = Some(Prev {
            source,
            width: l.w,
            target,
            shown,
        });
        moving
    }

    /// The "Contents | Layout" switch of a disk view (`r10-everyday.md` 4, `r1-redesign.md` 3.4).
    fn mode_switch(&self, colors: &Colors, cx: &mut Context<Self>) -> Div {
        let layout_mode = self.view.read(cx).layout_mode();
        let option = |label: &'static str, layout: bool, cx: &mut Context<Self>| {
            let active = layout_mode == layout;
            div()
                .id(label)
                .px(space::S8)
                .py(space::S2)
                .rounded(radius::ROW)
                .text_size(text::SMALL.size)
                .cursor_pointer()
                .when(active, |d| d.bg(colors.hover).text_color(colors.ink))
                .when(!active, |d| {
                    d.text_color(colors.ink3)
                        .hover(|style| style.bg(colors.hover))
                })
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.view
                        .update(cx, |view, cx| view.set_layout_mode(layout, cx));
                }))
                .child(label)
        };
        div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(space::S2)
            .child(option("Contents", false, cx))
            .child(option("Layout", true, cx))
    }

    /// The text at the right of the first line.
    fn right_text(
        &self,
        model: &Model,
        selection: Option<(usize, u64)>,
        focused: bool,
        current: Option<&PartInfo>,
        colors: &Colors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let small = |text: SharedString| -> Div {
            div()
                .flex_none()
                .text_size(text::SMALL.size)
                .text_color(colors.ink3)
                .child(text)
        };
        if focused && let Some(stop) = self.stops.get(self.cursor) {
            return small(stop.name.clone()).into_any_element();
        }
        if let Some((count, bytes)) = selection {
            let part = current.map_or(SharedString::default(), |p| p.name.clone());
            let share = current.map_or(String::new(), |p| {
                format!(" · {} of {}", percent(bytes, p.bytes), part)
            });
            let what = if count == 1 {
                "1 selected".to_owned()
            } else {
                format!("{count} selected")
            };
            return div()
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap(space::S4)
                .text_size(text::SMALL.size)
                .text_color(colors.ink2)
                .child(div().size(metrics::DOT).rounded_full().bg(colors.pin))
                .child(format!("{what} · {}{share}", format::size(bytes)))
                .into_any_element();
        }
        match &model.status {
            Status::None => div().into_any_element(),
            Status::Reading(0) => small("Reading the file list…".into()).into_any_element(),
            Status::Reading(entries) => {
                small(format!("Reading the file list… {} items", format::grouped(*entries)).into())
                    .into_any_element()
            }
            Status::Measuring => small("Measuring…".into()).into_any_element(),
            Status::LowerBound(bytes) => {
                small(format!("at least {} counted", format::size(*bytes)).into())
                    .into_any_element()
            }
            Status::Calculate(source) => {
                let source = *source;
                let ink = colors.ink;
                div()
                    .id("storage-calculate")
                    .flex_none()
                    .cursor_pointer()
                    .text_size(text::SMALL.size)
                    .text_color(ink)
                    .underline()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.search
                            .update(cx, |search, cx| search.calculate_sizes(source, cx));
                    }))
                    .child("Calculate sizes")
                    .into_any_element()
            }
        }
    }

    /// The bar itself: each partition clipped to a rounded track, its pieces inside.
    fn bar_row(
        &self,
        model: &Model,
        laid: &[Laid],
        reduced: bool,
        cursor_at: Option<(usize, Option<Piece>)>,
        colors: &Colors,
        cx: &mut Context<Self>,
    ) -> Div {
        let hover = self.hover;
        let row = div()
            .relative()
            .w_full()
            .h(metrics::STORAGE_BAR_HEIGHT)
            .flex_none();
        row.children(
            model
                .parts
                .iter()
                .zip(laid.iter())
                .enumerate()
                .map(|(pi, (part, laid))| {
                    let bar = model.bars.get(pi);
                    let mut track = div()
                        .id(("storage-part", pi))
                        .absolute()
                        .top_0()
                        .left(px(laid.x))
                        .w(px(laid.w))
                        .h_full()
                        .rounded(metrics::STORAGE_BAR_RADIUS)
                        .overflow_hidden()
                        .when(part.source.is_some(), |d| d.cursor_pointer());
                    if matches!(cursor_at, Some((i, None)) if i == pi) {
                        track = track.border_2().border_color(colors.ink);
                    }
                    let tip: SharedString = match bar.map(|b| &b.fill) {
                        Some(Fill::Unreadable) => {
                            format!(
                                "{}: {}. Disk Atlas cannot read it.",
                                part.name, part.caption
                            )
                        }
                        Some(Fill::Unallocated) => {
                            format!("Unallocated space, {}", format::size(part.bytes))
                        }
                        _ => format!("{}, {}", part.name, part.caption),
                    }
                    .into();
                    let target = part.source;
                    // The tooltip says the real size of a partition squeezed to the minimum width.
                    let tip2 = tip.clone();
                    let mut track = track
                        .when(!self.menu.entity().is_some(), |d| {
                            d.tooltip(move |_, cx| tooltip(tip2.clone(), cx))
                        })
                        .when_some(target, |d, source| {
                            d.on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                                    cx.stop_propagation();
                                    this.open_menu(Act::Open(source), e.position, window, cx);
                                }),
                            )
                            .on_click(cx.listener(
                                move |this, _: &ClickEvent, _, cx| {
                                    this.perform(Act::Open(source), cx);
                                },
                            ))
                        });
                    for (si, rect) in laid.pieces.iter().enumerate() {
                        track = track.child(self.piece_element(
                            (pi, si),
                            part,
                            bar,
                            rect,
                            laid.x,
                            hover,
                            reduced,
                            cursor_at,
                            colors,
                            cx,
                        ));
                    }
                    track
                }),
        )
    }

    /// One piece of a partition.
    #[allow(
        clippy::too_many_arguments,
        reason = "a piece is built from exactly these parts"
    )]
    fn piece_element(
        &self,
        (pi, si): (usize, usize),
        part: &PartInfo,
        bar: Option<&BarPartition>,
        rect: &Rect1,
        origin: f32,
        hover: Option<Piece>,
        reduced: bool,
        cursor_at: Option<(usize, Option<Piece>)>,
        colors: &Colors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let piece = rect.piece;
        let id = ("storage-piece", pi * MAX_PIECES + si);
        let mut element: Stateful<Div> = div()
            .id(id)
            .absolute()
            .top_0()
            .left(px(rect.x - origin))
            .w(px(rect.w))
            .h_full()
            .bg(piece_color(piece, colors));
        let is_current = bar.map_or(part.current, |b| b.current);
        let dim_others = is_current && hover.is_some_and(|h| h != piece);
        let mut opacity = if is_current {
            1.0
        } else {
            metrics::STORAGE_OTHER_OPACITY
        };
        if dim_others {
            opacity *= metrics::STORAGE_DIM_OPACITY;
        }
        if matches!(piece, Piece::Kind(_))
            || (!is_current && piece == Piece::Kind(KindClass::Other))
        {
            element = element.opacity(opacity);
        } else if dim_others {
            element = element.opacity(metrics::STORAGE_DIM_OPACITY);
        }
        match piece {
            Piece::Free => {
                let (stroke, gap) = metrics::STORAGE_HATCH;
                element = element.child(div().size_full().bg(pattern_slash(
                    colors.water_line,
                    stroke,
                    gap,
                )));
            }
            Piece::Hatched => {
                let (stroke, gap) = metrics::STORAGE_HATCH;
                element = element.child(div().size_full().bg(pattern_slash(
                    colors.line,
                    stroke,
                    gap,
                )));
            }
            Piece::Outline => {
                element = element
                    .bg(gpui::transparent_black())
                    .border_1()
                    .border_dashed()
                    .border_color(colors.line);
            }
            Piece::NotMeasured => {
                element = element.overflow_hidden().child(sweep(reduced, colors));
            }
            Piece::Kind(_) | Piece::System | Piece::Used => {}
        }
        if matches!(cursor_at, Some((i, Some(p))) if i == pi && p == piece) {
            element = element.border_2().border_color(colors.ink);
        }
        // Tooltip in words.
        let used = part.used.unwrap_or(0);
        let tip: Option<String> = match (piece, bar.map(|b| b.fill)) {
            (Piece::Free, _) => Some(format!(
                "Free: {}",
                format::size(part.bytes.saturating_sub(used))
            )),
            (Piece::Used, _) => Some(format!("Used: {}", format::size(used))),
            (Piece::NotMeasured, _) => Some("Not measured yet".to_owned()),
            (Piece::Kind(_) | Piece::System, Some(Fill::Kinds { classes, used, .. })) => {
                let bytes = legend_bytes(&classes, used, rect.w, piece);
                Some(format!(
                    "{}: {}, {} of what is used",
                    piece_name(piece),
                    format::size(bytes),
                    percent(bytes, used)
                ))
            }
            _ => None,
        };
        if let Some(tip) = tip.filter(|_| !self.menu.entity().is_some()) {
            let tip: SharedString = tip.into();
            element = element.tooltip(move |_, cx| tooltip(tip.clone(), cx));
        }
        if is_current && matches!(piece, Piece::Kind(_) | Piece::System) {
            let act = match piece {
                Piece::Kind(class) => Act::Search(class),
                _ => Act::Nothing,
            };
            element = element
                .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                    let next = hovered.then_some(piece);
                    if (*hovered || this.hover == Some(piece)) && this.hover != next {
                        this.hover = next;
                        cx.notify();
                    }
                }))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    this.perform(act, cx);
                }));
            if act != Act::Nothing {
                element = element.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.open_menu(act, e.position, window, cx);
                    }),
                );
            }
            if act != Act::Nothing {
                element = element.cursor_pointer();
            }
        }
        element.into_any_element()
    }

    /// The legend: a chip per drawn kind, two lines at most, the rest folded into "and N more".
    fn legend_row(
        &self,
        chips: &[(Piece, u64)],
        used: u64,
        width: f32,
        cursor_at: Option<(usize, Option<Piece>)>,
        colors: &Colors,
        cx: &mut Context<Self>,
    ) -> Div {
        // How many chips fit on two lines, guessed from the characters they hold.
        let char_width = f32::from(text::SMALL.size) * metrics::STORAGE_CHAR_WIDTH;
        let pad = f32::from(space::S24) + f32::from(metrics::STORAGE_CHIP_SWATCH);
        let (mut line, mut at, mut shown) = (1usize, 0.0f32, 0usize);
        for &(piece, bytes) in chips {
            let chars = piece_name(piece).chars().count() + format::size(bytes).chars().count();
            let chip =
                chars as f32 * char_width + pad + f32::from(crate::theme::metrics::ICON_CAPTION);
            if at + chip > width {
                line += 1;
                at = 0.0;
            }
            if line > metrics::STORAGE_LEGEND_LINES {
                break;
            }
            at += chip;
            shown += 1;
        }
        // "and N more" needs room too: give up one chip for it when some are folded.
        if shown < chips.len() {
            shown = shown.saturating_sub(1);
        }
        let hover = self.hover;
        let mut row = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_x(space::S16)
            .gap_y(space::S4)
            .text_size(text::SMALL.size)
            .text_color(colors.ink2);
        for (index, &(piece, bytes)) in chips.iter().take(shown).enumerate() {
            let dim = hover.is_some_and(|h| h != piece);
            let act = match piece {
                Piece::Kind(class) => Act::Search(class),
                _ => Act::Nothing,
            };
            let focused = matches!(cursor_at, Some((_, Some(p))) if p == piece);
            let tip: SharedString = format!(
                "{}: {}, {} of what is used",
                piece_name(piece),
                format::size(bytes),
                percent(bytes, used)
            )
            .into();
            let swatch = div()
                .flex_none()
                .size(metrics::STORAGE_CHIP_SWATCH)
                .rounded(radius::MARK)
                .bg(piece_color(piece, colors));
            let glyph = match piece {
                Piece::Kind(class) => Some(Glyph::from_class(class)),
                _ => None,
            };
            let mut chip = div()
                .id(("storage-chip", index))
                .flex()
                .flex_row()
                .items_center()
                .gap(space::S4)
                .rounded(radius::MARK)
                .when(dim, |d| d.opacity(metrics::STORAGE_DIM_OPACITY))
                .when(focused, |d| d.border_1().border_color(colors.ink))
                .child(swatch)
                .when_some(glyph, |d, glyph| {
                    d.child(
                        svg()
                            .path(glyph.path())
                            .flex_none()
                            .size(crate::theme::metrics::ICON_CAPTION)
                            .text_color(colors.tint(glyph)),
                    )
                })
                .child(piece_name(piece))
                .child(div().text_color(colors.ink3).child(format::size(bytes)))
                .when(!self.menu.entity().is_some(), |d| {
                    d.tooltip(move |_, cx| tooltip(tip.clone(), cx))
                })
                .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                    let next = hovered.then_some(piece);
                    if (*hovered || this.hover == Some(piece)) && this.hover != next {
                        this.hover = next;
                        cx.notify();
                    }
                }));
            if act != Act::Nothing {
                chip = chip.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, e: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.open_menu(act, e.position, window, cx);
                    }),
                );
                chip = chip.cursor_pointer().on_click(cx.listener(
                    move |this, _: &ClickEvent, _, cx| {
                        this.perform(act, cx);
                    },
                ));
            }
            row = row.child(chip);
        }
        if shown < chips.len() {
            row = row.child(
                div()
                    .text_color(colors.ink3)
                    .child(format!("and {} more", chips.len() - shown)),
            );
        }
        row
    }
}

/// The bytes a kind piece stands for, as the legend counts them (small kinds folded).
fn legend_bytes(classes: &ClassBytes, used: u64, width_px: f32, piece: Piece) -> u64 {
    // The legend folds by the used part's width; a piece's own width is the best stand-in for it
    // here, so the numbers of a segment and its chip agree for every kind that has a segment.
    let _ = width_px;
    legend(classes, used, f32::MAX)
        .into_iter()
        .find(|(p, _)| *p == piece)
        .map_or(0, |(_, bytes)| bytes)
}

/// The measuring sweep: a lighter band that crosses the bar once per `SWEEP_PASS`; with reduced
/// motion a still stripe.
fn sweep(reduced: bool, colors: &Colors) -> AnyElement {
    let band = div()
        .absolute()
        .top_0()
        .h_full()
        .w(relative(metrics::STORAGE_SWEEP_BAND))
        .bg(colors.relief.opacity(metrics::STORAGE_SWEEP_OPACITY));
    if reduced {
        return band.left_0().into_any_element();
    }
    band.with_animation(
        "storage-sweep",
        Animation::new(motion::SWEEP_PASS).repeat(),
        |band, t| {
            let travel = 1.0 + metrics::STORAGE_SWEEP_BAND;
            band.left(relative(-metrics::STORAGE_SWEEP_BAND + t * travel))
        },
    )
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classes(of: &[(KindClass, u64)], system: u64) -> ClassBytes {
        let mut classes = ClassBytes::default();
        for &(class, bytes) in of {
            classes.by_class[usize::from(class.bits())] = bytes;
        }
        classes.system = system;
        classes
    }

    fn kinds(classes: ClassBytes, bytes: u64, used: u64) -> BarPartition {
        BarPartition {
            bytes,
            current: true,
            fill: Fill::Kinds {
                classes,
                used,
                accounted: used,
            },
        }
    }

    fn total_width(laid: &[Laid]) -> f32 {
        let last = laid.last().expect("a partition");
        last.x + last.w
    }

    #[test]
    fn kinds_come_in_the_fixed_order() {
        let c = classes(
            &[
                (KindClass::Font, 100),
                (KindClass::Image, 300),
                (KindClass::Audio, 200),
                (KindClass::Video, 150),
                (KindClass::Document, 50),
            ],
            80,
        );
        let laid = layout(&[kinds(c, 2000, 1000)], 800.0);
        let order: Vec<Piece> = laid[0].pieces.iter().map(|r| r.piece).collect();
        assert_eq!(
            order,
            [
                Piece::Kind(KindClass::Image),
                Piece::Kind(KindClass::Video),
                Piece::Kind(KindClass::Audio),
                Piece::Kind(KindClass::Document),
                Piece::Kind(KindClass::Font),
                // 120 of the used 1000 bytes are in no file: the volume's own.
                Piece::System,
                Piece::Free,
            ]
        );
    }

    #[test]
    fn a_kind_under_three_pixels_joins_other() {
        // 2 bytes of 1000 on 400 px is 0.8 px.
        let c = classes(&[(KindClass::Image, 900), (KindClass::Font, 2)], 0);
        let laid = layout(&[kinds(c, 1000, 1000)], 400.0);
        let pieces: Vec<Piece> = laid[0].pieces.iter().map(|r| r.piece).collect();
        assert_eq!(
            pieces,
            [
                Piece::Kind(KindClass::Image),
                Piece::Kind(KindClass::Other),
                Piece::System
            ]
        );
    }

    #[test]
    fn a_small_partition_on_a_big_disk_gets_28_pixels() {
        let mb = 1u64 << 20;
        let free = |bytes: u64| BarPartition {
            bytes,
            current: false,
            fill: Fill::UsedFree { used: bytes / 2 },
        };
        let laid = layout(&[free(100 * mb), free(1 << 40)], 800.0);
        assert_eq!(laid[0].w, 28.0);
        assert_eq!(laid[1].x, 30.0);
        assert_eq!(laid[1].w, 800.0 - 28.0 - 2.0);
    }

    #[test]
    fn widths_are_whole_pixels_and_fill_the_bar() {
        let parts: Vec<BarPartition> = [333u64, 777, 1, 12_345, 4_000]
            .iter()
            .map(|&bytes| BarPartition {
                bytes,
                current: false,
                fill: Fill::UsedFree { used: bytes / 3 },
            })
            .collect();
        for width in [199.0f32, 640.0, 1013.0] {
            let laid = layout(&parts, width);
            assert_eq!(total_width(&laid), width.floor());
            for part in &laid {
                assert_eq!(part.w.fract(), 0.0);
                assert_eq!(part.x.fract(), 0.0);
                // Pieces and their 1 px gaps fill the partition.
                let last = part.pieces.last().expect("a piece");
                assert_eq!(last.x + last.w, part.x + part.w);
                for piece in &part.pieces {
                    assert_eq!(piece.w.fract(), 0.0);
                }
            }
        }
    }

    #[test]
    fn unreadable_and_unallocated_are_one_piece() {
        let parts = [
            BarPartition {
                bytes: 100,
                current: false,
                fill: Fill::Unreadable,
            },
            BarPartition {
                bytes: 100,
                current: false,
                fill: Fill::Unallocated,
            },
        ];
        let laid = layout(&parts, 200.0);
        assert_eq!(
            laid[0].pieces,
            [Rect1 {
                x: 0.0,
                w: 99.0,
                piece: Piece::Hatched
            }]
        );
        assert_eq!(laid[1].pieces[0].piece, Piece::Outline);
        assert_eq!(laid[1].pieces[0].x, 101.0);
    }

    #[test]
    fn overcommitted_files_are_scaled_to_the_used_space() {
        // The files claim 3000 bytes on a volume that uses 1000.
        let c = classes(&[(KindClass::Image, 2000), (KindClass::Video, 1000)], 0);
        let laid = layout(&[kinds(c, 2000, 1000)], 1000.0);
        let pieces = &laid[0].pieces;
        // Pictures two thirds of the used half, Movies one third; free the other half.
        assert_eq!(pieces[0].piece, Piece::Kind(KindClass::Image));
        assert_eq!(pieces[1].piece, Piece::Kind(KindClass::Video));
        assert_eq!(pieces[2].piece, Piece::Free);
        assert_eq!(pieces.len(), 3);
        assert!((pieces[0].w - 2.0 * pieces[1].w).abs() <= 2.0, "{pieces:?}");
        assert!((pieces[2].w - 498.0).abs() <= 2.0, "{pieces:?}");
    }

    #[test]
    fn what_is_not_accounted_for_yet_is_not_measured() {
        let c = classes(&[(KindClass::Image, 200)], 0);
        let part = BarPartition {
            bytes: 1000,
            current: true,
            fill: Fill::Kinds {
                classes: c,
                used: 600,
                accounted: 200,
            },
        };
        let laid = layout(&[part], 1000.0);
        let pieces: Vec<Piece> = laid[0].pieces.iter().map(|r| r.piece).collect();
        assert_eq!(
            pieces,
            [
                Piece::Kind(KindClass::Image),
                Piece::NotMeasured,
                Piece::Free
            ]
        );
    }

    #[test]
    fn used_and_free_when_only_the_system_knows() {
        let part = BarPartition {
            bytes: 1000,
            current: true,
            fill: Fill::UsedFree { used: 250 },
        };
        let laid = layout(&[part], 1000.0);
        let pieces = &laid[0].pieces;
        assert_eq!(pieces[0].piece, Piece::Used);
        assert_eq!(pieces[0].w, 250.0);
        assert_eq!(pieces[1].piece, Piece::Free);
        assert_eq!(pieces[1].w, 749.0);
    }

    #[test]
    fn nothing_to_lay_out_and_nothing_to_scale_by_do_not_panic() {
        assert!(layout(&[], 100.0).is_empty());
        let empty = BarPartition {
            bytes: 0,
            current: true,
            fill: Fill::Kinds {
                classes: ClassBytes::default(),
                used: 0,
                accounted: 0,
            },
        };
        let laid = layout(&[empty, empty], 100.0);
        assert_eq!(laid.len(), 2);
        assert_eq!(total_width(&laid), 100.0);
        // Narrower than the minimums: shared evenly.
        let laid = layout(&[empty, empty, empty], 30.0);
        assert_eq!(total_width(&laid), 30.0);
        assert!(layout(&[empty], f32::NAN)[0].w == 0.0);
    }

    #[test]
    fn the_legend_folds_small_kinds_and_sums() {
        let c = classes(
            &[
                (KindClass::Image, 9000),
                (KindClass::Font, 3),
                (KindClass::Model, 5),
                (KindClass::Other, 10),
                (KindClass::Folder, 7),
            ],
            200,
        );
        let chips = legend(&c, 10_000, 400.0);
        assert_eq!(
            chips,
            [
                (Piece::Kind(KindClass::Image), 9000),
                // Font and 3D model are under 3 px each and join Other with its own 17 bytes.
                (Piece::Kind(KindClass::Other), 25),
                // Its own 200 bytes and the 775 that no file accounts for.
                (Piece::System, 975),
            ]
        );
        assert_eq!(chips.iter().map(|c| c.1).sum::<u64>(), 10_000);
        assert!(legend(&ClassBytes::default(), 0, 400.0).is_empty());
    }

    #[test]
    fn percent_is_honest_about_slivers() {
        assert_eq!(percent(38, 100), "38 %");
        assert_eq!(percent(1, 10_000), "under 1 %");
        assert_eq!(percent(0, 0), "0 %");
    }

    #[test]
    fn a_drive_root_is_not_a_folder() {
        assert!(is_drive_root(std::path::Path::new("/")));
        assert!(is_drive_root(std::path::Path::new("/Volumes/Backup")));
        assert!(!is_drive_root(std::path::Path::new(
            "/Volumes/Backup/Photos"
        )));
        assert!(!is_drive_root(std::path::Path::new("/home/julian")));
    }

    #[test]
    fn the_other_chip_stands_for_other_folders_and_every_folded_kind() {
        let c = classes(
            &[
                (KindClass::Image, 9000),
                (KindClass::Font, 3),
                (KindClass::Model, 5),
                (KindClass::Other, 10),
            ],
            0,
        );
        // Font and 3D model fold into Other on 400 px, so a click on Other finds them too.
        let set = kinds_of(&c, 10_000, 400.0, KindClass::Other);
        assert!(set.contains(KindClass::Other));
        assert!(set.contains(KindClass::Font));
        assert!(set.contains(KindClass::Model));
        assert!(!set.contains(KindClass::Image));
        assert!(!set.contains(KindClass::Folder));
        // A kind with a segment of its own is just itself.
        assert_eq!(
            kinds_of(&c, 10_000, 400.0, KindClass::Image),
            KindSet::EMPTY.with(KindClass::Image)
        );
        // On a very wide bar nothing folds.
        let wide = kinds_of(&c, 10_000, 100_000.0, KindClass::Other);
        assert!(!wide.contains(KindClass::Font));
        // Zero bytes do not divide by zero.
        let _ = kinds_of(&ClassBytes::default(), 0, 400.0, KindClass::Other);
    }

    #[test]
    fn easing_moves_pieces_to_their_place_and_grows_new_ones() {
        let from = [(Piece::Free, 0.0, 100.0)];
        let to = [
            (Piece::Kind(KindClass::Image), 0.0, 40.0),
            (Piece::Free, 41.0, 59.0),
        ];
        let start = ease_pieces(&from, &to, 0.0);
        assert_eq!(start[0].2, 0.0);
        assert_eq!(start[1], (Piece::Free, 0.0, 100.0));
        assert_eq!(ease_pieces(&from, &to, 1.0), to.to_vec());
        let half = ease_pieces(&from, &to, 0.5);
        assert_eq!(half[0].2, 20.0);
        assert_eq!(ease_pieces(&from, &to, 7.0), to.to_vec());
    }

    #[test]
    fn growing_in_shows_the_used_part_from_the_left_and_free_takes_the_rest() {
        let pieces = [
            (Piece::Kind(KindClass::Image), 10.0, 40.0),
            (Piece::Kind(KindClass::Video), 51.0, 20.0),
            (Piece::Free, 72.0, 38.0),
        ];
        let at_start = grow_pieces(&pieces, 10.0, 100.0, 0.0);
        assert_eq!(at_start[0].2, 0.0);
        assert_eq!(at_start[2].1, 11.0);
        let end = *at_start.last().unwrap_or(&(Piece::Free, 0.0, 0.0));
        assert_eq!(end.1 + end.2, 110.0);
        let done = grow_pieces(&pieces, 10.0, 100.0, 1.0);
        assert_eq!(done[0], pieces[0]);
        assert_eq!(done[1], pieces[1]);
        assert_eq!(done[2].1 + done[2].2, 110.0);
    }
}
