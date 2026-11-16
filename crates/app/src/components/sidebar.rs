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

//! The sidebar: the places the user can browse, grouped, with the current one marked.
//!
//! Disks and disk images list each device as a collapsible heading (model, size and a thin bar of
//! its partitions to scale) with its readable volumes indented below it, the filesystem name at
//! the right of each. System partitions, formats with no reader and partitions that failed to
//! mount sit behind a quiet "N other partitions" row. A disk whose volumes the host already
//! mounts is left out of Disks; its volumes show under This computer with the disk's model. A
//! device that could not be opened is one selectable row, and a notice under Disks offers to
//! restart with the rights to read it (`r1-redesign.md` 3.1 and 3.2). Host folders and drives are plain rows. "Open disk image" sits at the bottom,
//! with "Open disk image folder" under it where the platform's file prompt cannot pick files and
//! folders together.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::f32::consts::FRAC_PI_2;
use std::rc::Rc;
use std::time::Instant;

use gpui::{
    Action, AnyElement, AnyView, ClickEvent, ClipboardItem, Context, Div, Entity, FocusHandle,
    FontWeight, HighlightStyle, IntoElement, MouseButton, MouseDownEvent, Pixels, Point, Render,
    SharedString, Stateful, StyledText, Subscription, Transformation, Window, canvas, div,
    prelude::*, radians, relative, svg,
};

use crate::assets::icon;
use crate::components::menu::{CloseReason, MenuSlot};
use crate::elevation::{self, Restart};
use crate::format;
use crate::keymap::workspace::{OpenImage, OpenImageFolder, Search};
use crate::keymap::{SIDEBAR, context, menu as menu_keys};
use crate::state::menus::{
    BUILT, DiskFacts, MenuState, MenuTarget, VolumeFacts, build, file_manager_name,
};
use crate::state::navigation::{Location, Navigation};
use crate::state::order::SortSpec;
use crate::state::properties::{PropertiesTarget, PropertiesWindows, Request};
use crate::state::sources::{
    Availability, Disk, DiskId, Source, SourceGroup, SourceId, SourceKind, Sources, SystemKind,
    is_quiet_role,
};
use crate::state::view::ViewState;
use crate::theme::{
    Colors, Theme, metrics,
    motion::{self, Disclosure, Reveal},
    scale::{Density, radius, size, space, text},
};
use disk_atlas_lib::fs::probe::VolumeRole;

/// The groups in the order the sidebar draws them.
const GROUP_ORDER: [SourceGroup; 3] = [
    SourceGroup::Disks,
    SourceGroup::DiskImages,
    SourceGroup::ThisComputer,
];

/// For each group in `GROUP_ORDER` that has at least one entry, the positions in `groups` that
/// belong to it, in their original order. Groups with no entries are left out.
fn group_indices(groups: &[SourceGroup]) -> Vec<(SourceGroup, Vec<usize>)> {
    GROUP_ORDER
        .iter()
        .filter_map(|&group| {
            let indices: Vec<usize> = groups
                .iter()
                .enumerate()
                .filter(|(_, candidate)| **candidate == group)
                .map(|(index, _)| index)
                .collect();
            (!indices.is_empty()).then_some((group, indices))
        })
        .collect()
}

/// Where a partition of a disk is listed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Placement {
    /// A row of its own under the disk.
    Listed,
    /// Behind the disk's "N other partitions" row, with this one-line reason.
    Other(SharedString),
}

/// Decides where a partition goes (`r1-redesign.md` 3.2): a system partition is behind "other
/// partitions" even when readable; so is one with no reader, unknown content or a failed mount.
fn placement(system: Option<SystemKind>, availability: &Availability) -> Placement {
    if let Some(kind) = system {
        return Placement::Other(kind.reason().into());
    }
    match availability {
        Availability::Browsable | Availability::NeedsElevation => Placement::Listed,
        Availability::Unsupported { format: name } => {
            Placement::Other(unsupported_reason(name).into())
        }
        Availability::Failed(text) => Placement::Other(text.clone()),
    }
}

/// The one-line reason a format with no reader is not listed.
fn unsupported_reason(format: &str) -> String {
    match format {
        "Unknown" => "Disk Atlas does not recognise what is stored here.".to_owned(),
        "Empty" => "Nothing is stored here.".to_owned(),
        "NTFS" => "NTFS. Disk Atlas cannot read NTFS yet; Windows can, if you mount it.".to_owned(),
        other => format!("{other}. Disk Atlas cannot read {other} yet."),
    }
}

/// The role set after a container volume's name, in `ink3`: only when the name does not already
/// say it ("Macintosh HD - Data" needs no "Data").
fn role_note(label: &str, role: Option<VolumeRole>) -> Option<&'static str> {
    let name = role?.name();
    (!label.to_lowercase().contains(&name.to_lowercase())).then_some(name)
}

/// The text of "N other partitions".
fn other_title(count: usize) -> String {
    if count == 1 {
        "1 other partition".to_owned()
    } else {
        format!("{count} other partitions")
    }
}

/// The notice under Disks when `count` disks cannot be read without permission.
fn permission_notice(count: usize) -> String {
    if count == 1 {
        "1 disk needs permission before Disk Atlas can read it.".to_owned()
    } else {
        format!("{count} disks need permission before Disk Atlas can read them.")
    }
}

/// One piece of a disk's usage bar: where it starts, how wide it is (both as fractions of the
/// disk), and whether it is the volume being browsed.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Segment {
    left: f32,
    width: f32,
    current: bool,
}

/// The partitions `(start, len, is_current)` of a disk of `total` bytes, to scale. Pieces that
/// lie outside the disk are clamped; empty ones are dropped.
fn bar_segments(parts: &[(u64, u64, bool)], total: u64) -> Vec<Segment> {
    if total == 0 {
        return Vec::new();
    }
    let total = total as f64;
    parts
        .iter()
        .filter_map(|&(start, len, current)| {
            let left = (start as f64 / total).clamp(0.0, 1.0);
            let right = (start.saturating_add(len) as f64 / total).clamp(0.0, 1.0);
            (right > left).then_some(Segment {
                left: left as f32,
                width: (right - left) as f32,
                current,
            })
        })
        .collect()
}

/// One thing a group lists: a host root, or a disk with its volumes.
enum Entry<'a> {
    Host(&'a Source),
    Disk(&'a Disk),
}

impl Entry<'_> {
    fn group(&self) -> SourceGroup {
        match self {
            Self::Host(source) => source.group,
            Self::Disk(disk) => disk.group,
        }
    }
}

/// What a context menu of the sidebar was opened on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hit {
    /// A disk heading.
    Disk(DiskId),
    /// A source row.
    Source(SourceId),
}

/// A part of the sidebar that opens and closes: a disk's volumes, or its other partitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Fold {
    Disk(DiskId),
    Others(DiskId),
}

/// The sidebar: the sources, grouped.
pub struct Sidebar {
    sources: Entity<Sources>,
    navigation: Entity<Navigation>,
    /// Choosing a disk heading opens the disk view.
    view: Entity<ViewState>,
    /// Disks the user opened or closed by hand. Any other disk is open when it holds the
    /// current source.
    open: HashMap<DiskId, bool>,
    /// Disks whose "other partitions" row is open.
    others_open: HashSet<DiskId>,
    /// Folds opening or closing (`r1-redesign.md` 2.2).
    folds: HashMap<Fold, Disclosure>,
    /// The height of each fold's content as last laid out, which its opening grows to.
    fold_heights: Rc<RefCell<HashMap<Fold, Pixels>>>,
    /// The clock of the frame being drawn, so every fold reads the same time.
    frame_now: Instant,
    /// A restart with administrator rights is being asked for.
    restarting: bool,
    /// Holds the focus while a context menu opened here is open, so a chosen item reaches this
    /// view.
    focus: FocusHandle,
    /// The context menu, while one is open.
    menu: MenuSlot,
    /// What the open menu is about.
    hit: Option<Hit>,
    /// Where the focus was before the menu took it here.
    previous: Option<FocusHandle>,
    _subscriptions: Vec<Subscription>,
}

impl Sidebar {
    /// Lists `sources` and opens the one the user picks through `navigation`.
    pub fn new(
        sources: Entity<Sources>,
        navigation: Entity<Navigation>,
        view: Entity<ViewState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.observe(&sources, |_, _, cx| cx.notify()),
            cx.observe(&view, |_, _, cx| cx.notify()),
            cx.observe_in(&navigation, window, |this, _, window, cx| {
                // A menu belongs to the place it was opened in.
                this.menu.dismiss(window, cx);
                cx.notify();
            }),
        ];
        Self {
            focus: cx.focus_handle(),
            menu: MenuSlot::default(),
            hit: None,
            previous: None,
            sources,
            navigation,
            view,
            open: HashMap::new(),
            others_open: HashSet::new(),
            folds: HashMap::new(),
            fold_heights: Rc::new(RefCell::new(HashMap::new())),
            frame_now: Instant::now(),
            restarting: false,
            _subscriptions: subscriptions,
        }
    }

    /// Relaunches the app with administrator rights. The prompt blocks, so it runs on the
    /// background executor; the app quits once the new copy has started.
    fn restart_elevated(&mut self, cx: &mut Context<Self>) {
        if self.restarting {
            return;
        }
        self.restarting = true;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { elevation::restart_elevated() })
                .await;
            match result {
                Ok(Restart::Started) => {
                    cx.update(|cx| cx.quit()).ok();
                }
                Ok(Restart::Declined) => {}
                Err(error) => {
                    tracing::warn!(%error, "could not restart with administrator rights");
                }
            }
            this.update(cx, |this, _| this.restarting = false).ok();
        })
        .detach();
    }

    // ---- context menus ----

    /// The menu target for `hit`.
    fn target_of(&self, hit: Hit, cx: &Context<Self>) -> Option<MenuTarget> {
        let sources = self.sources.read(cx);
        let disk_facts = |disk: &Disk| DiskFacts {
            image: disk.group == SourceGroup::DiskImages,
            can_show: disk.first_source().is_some(),
            can_ask_permission: elevation::can_restart_elevated()
                && disk
                    .whole
                    .and_then(|id| sources.source(id))
                    .is_some_and(|source| source.availability == Availability::NeedsElevation),
        };
        match hit {
            Hit::Disk(id) => Some(MenuTarget::SidebarDisk(disk_facts(sources.disk(id)?))),
            Hit::Source(id) => {
                let source = sources.source(id)?;
                match source.kind {
                    SourceKind::Host => Some(MenuTarget::SidebarPlace),
                    SourceKind::Volume { .. } => Some(MenuTarget::SidebarVolume(VolumeFacts {
                        image: source.group == SourceGroup::DiskImages,
                        browsable: source.availability.is_browsable(),
                    })),
                    SourceKind::WholeDisk { disk } => {
                        Some(MenuTarget::SidebarDisk(disk_facts(sources.disk(disk)?)))
                    }
                }
            }
        }
    }

    /// Opens the menu of `hit` at `at`. The sidebar takes the focus for as long as it is open.
    fn open_menu_for(
        &mut self,
        hit: Hit,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        let Some(target) = self.target_of(hit, cx) else {
            return;
        };
        let state = MenuState {
            built: BUILT,
            can_paste: false,
            sort: SortSpec::default(),
            file_manager: file_manager_name(),
            show_hidden: false,
            show_system: false,
        };
        let items = build(&target, &state);
        if items.is_empty() {
            return;
        }
        self.hit = Some(hit);
        if !self.focus.is_focused(window) {
            self.previous = window.focused(cx);
        }
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
                // Back to where the focus was, unless the menu closed because it moved elsewhere.
                let moved = matches!(
                    reason,
                    CloseReason::FocusLost
                        | CloseReason::Blur
                        | CloseReason::TabNext
                        | CloseReason::TabPrev
                );
                if let Some(previous) = this.previous.take()
                    && !moved
                {
                    window.focus(&previous);
                }
                cx.notify();
            },
        );
    }

    /// The Menu key and Shift+F10: the menu of the place being browsed.
    fn open_menu(&mut self, _: &menu_keys::Open, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self
            .navigation
            .read(cx)
            .current()
            .map(|location| location.source())
        else {
            return;
        };
        let at = window.mouse_position();
        self.open_menu_for(Hit::Source(id), at, window, cx);
    }

    /// The disk the open menu is about.
    fn menu_disk(&self, cx: &Context<Self>) -> Option<DiskId> {
        let sources = self.sources.read(cx);
        match self.hit? {
            Hit::Disk(id) => Some(id),
            Hit::Source(id) => match sources.source(id)?.kind {
                SourceKind::Volume { disk, .. } | SourceKind::WholeDisk { disk } => Some(disk),
                SourceKind::Host => None,
            },
        }
    }

    /// The source the open menu opens.
    fn menu_source(&self, cx: &Context<Self>) -> Option<Source> {
        let sources = self.sources.read(cx);
        match self.hit? {
            Hit::Disk(id) => sources.source(sources.disk(id)?.first_source()?),
            Hit::Source(id) => sources.source(id),
        }
        .cloned()
    }

    fn open_hit(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(source) = self.menu_source(cx) else {
            return false;
        };
        let to = Location::root(&source);
        self.navigation
            .update(cx, |navigation, cx| navigation.navigate(to, cx));
        true
    }

    fn context_open(&mut self, _: &context::Open, _: &mut Window, cx: &mut Context<Self>) {
        self.open_hit(cx);
    }

    /// "Search this volume": opens it, then starts a search as Ctrl+F does.
    fn context_search(
        &mut self,
        _: &context::SearchVolume,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.open_hit(cx) {
            window.dispatch_action(Box::new(Search), cx);
        }
    }

    /// "Properties": a disk, or a volume (a place on this computer included). A whole-disk row
    /// stands for its disk.
    fn context_properties(
        &mut self,
        _: &context::Properties,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(hit) = self.hit else {
            return;
        };
        let target = match hit {
            Hit::Disk(disk) => PropertiesTarget::Disk { disk },
            Hit::Source(source) => match self.sources.read(cx).source(source).map(|s| s.kind) {
                Some(SourceKind::WholeDisk { disk }) => PropertiesTarget::Disk { disk },
                Some(SourceKind::Host | SourceKind::Volume { .. }) => {
                    PropertiesTarget::Volume { source }
                }
                None => return,
            },
        };
        PropertiesWindows::open_here(target, Request::default(), cx);
    }

    fn context_allow(
        &mut self,
        _: &context::AllowReadingDisks,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.restart_elevated(cx);
    }

    fn context_copy_device_path(
        &mut self,
        _: &context::CopyDevicePath,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = self
            .menu_disk(cx)
            .and_then(|id| self.sources.read(cx).disk(id).map(|disk| disk.path.clone()));
        if let Some(path) = path {
            cx.write_to_clipboard(ClipboardItem::new_string(path.display().to_string()));
        }
    }

    /// "Show in Explorer": a place's folder, or the file of a disk image.
    fn context_reveal(
        &mut self,
        _: &context::ShowInFileManager,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = match self.hit {
            Some(Hit::Source(id)) if self.menu_disk(cx).is_none() => self
                .sources
                .read(cx)
                .host_root(id)
                .map(std::path::Path::to_path_buf),
            _ => self
                .menu_disk(cx)
                .and_then(|id| self.sources.read(cx).disk(id).map(|disk| disk.path.clone())),
        };
        if let Some(path) = path {
            cx.reveal_path(&path);
        }
    }

    /// One selectable row: dot, label, and `detail` at the right (a filesystem name or a size).
    /// `indent` moves a volume in under its disk.
    #[allow(
        clippy::too_many_arguments,
        reason = "a row is built from exactly these parts"
    )]
    fn row(
        &self,
        index: usize,
        source: &Source,
        detail: Option<SharedString>,
        indent: bool,
        is_current: bool,
        colors: &Colors,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let target = source.clone();
        let id = source.id;
        let left = if indent {
            metrics::ROW_PADDING + space::S8
        } else {
            metrics::ROW_PADDING
        };
        let row = div()
            .id(("sidebar-row", index))
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .h(Density::default().sidebar_row_height())
            .pl(left)
            .pr(space::S8)
            .gap(space::S8)
            .rounded(radius::ROW)
            .cursor_pointer()
            // Preboot, Recovery and VM are rarely what anyone wants: dimmed.
            .text_color(if is_quiet_role(source.role) {
                colors.ink3
            } else {
                colors.ink
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                let to = Location::root(&target);
                this.navigation
                    .update(cx, |navigation, cx| navigation.navigate(to, cx));
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.open_menu_for(Hit::Source(id), event.position, window, cx);
                }),
            )
            .child(
                div()
                    .flex_none()
                    .size(metrics::DOT)
                    .rounded_full()
                    .bg(if is_current { colors.pin } else { colors.line }),
            )
            .child({
                let label = div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .when(is_current, |label| label.font_weight(FontWeight::BOLD));
                match role_note(&source.label, source.role) {
                    // "Macintosh HD System": the role after the name, quieter.
                    Some(role) => {
                        let text = format!("{} {role}", source.label);
                        let note = source.label.len().saturating_add(1)..text.len();
                        let quiet = HighlightStyle {
                            color: Some(colors.ink3),
                            font_weight: Some(FontWeight::NORMAL),
                            ..HighlightStyle::default()
                        };
                        label.child(StyledText::new(text).with_highlights(vec![(note, quiet)]))
                    }
                    None => label.child(source.label.clone()),
                }
            })
            .children(detail.map(|detail| detail_text(detail, colors)));
        if is_current {
            row.bg(colors.hover)
        } else {
            let (hover, pressed) = (colors.hover, colors.field);
            row.hover(move |style| style.bg(hover))
                .active(move |style| style.bg(pressed))
        }
    }

    /// The rows of one disk: a heading with its bar, its listed volumes and, when there are any,
    /// its "other partitions". A disk that failed to open is one row. `next_index` numbers the
    /// rows for their element ids.
    fn disk_rows(
        &self,
        disk: &Disk,
        next_index: &mut usize,
        current: Option<SourceId>,
        sources: &Sources,
        colors: &Colors,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        if let Some(source) = disk.whole.and_then(|id| sources.source(id)) {
            *next_index += 1;
            let row = self.row(
                *next_index,
                source,
                disk_size(disk),
                false,
                current == Some(source.id),
                colors,
                cx,
            );
            return vec![row.into_any_element()];
        }

        let volumes: Vec<&Source> = disk
            .volumes
            .iter()
            .filter_map(|&id| sources.source(id))
            .collect();
        let holds_current = volumes.iter().any(|source| Some(source.id) == current);
        let is_open = self.open.get(&disk.id).copied().unwrap_or(holds_current);
        let disk_fold = self.fold_frame(Fold::Disk(disk.id));

        *next_index += 1;
        let heading = self
            .disk_heading(disk, sources, current, is_open, *next_index, colors, cx)
            .into_any_element();
        // Closing, the volumes stay drawn until they have folded away.
        if !is_open && disk_fold.is_none() {
            return vec![heading];
        }
        let mut rows = Vec::new();

        let mut others: Vec<(&Source, SharedString)> = Vec::new();
        for source in volumes {
            match placement(source.system, &source.availability) {
                Placement::Listed => {
                    *next_index += 1;
                    rows.push(
                        self.row(
                            *next_index,
                            source,
                            source.format.clone(),
                            true,
                            current == Some(source.id),
                            colors,
                            cx,
                        )
                        .into_any_element(),
                    );
                }
                Placement::Other(reason) => others.push((source, reason)),
            }
        }
        if !others.is_empty() {
            let others_open = self.others_open.contains(&disk.id);
            let others_fold = self.fold_frame(Fold::Others(disk.id));
            *next_index += 1;
            rows.push(
                self.others_row(disk.id, &others, others_open, *next_index, colors, cx)
                    .into_any_element(),
            );
            if others_open || others_fold.is_some() {
                let mut items = Vec::new();
                for (source, reason) in others {
                    *next_index += 1;
                    items.push(
                        self.other_item(*next_index, source, reason, current, colors, cx)
                            .into_any_element(),
                    );
                }
                rows.push(self.fold(Fold::Others(disk.id), items, others_fold));
            }
        }
        vec![heading, self.fold(Fold::Disk(disk.id), rows, disk_fold)]
    }

    /// This frame of `fold`'s opening or closing, `None` at rest.
    fn fold_frame(&self, fold: Fold) -> Option<Reveal> {
        self.folds
            .get(&fold)
            .and_then(|disclosure| disclosure.at(self.frame_now))
    }

    /// Opens or closes `fold`, animated.
    fn start_fold(&mut self, fold: Fold, opening: bool, cx: &Context<Self>) {
        self.folds.insert(
            fold,
            Disclosure::new(opening, Instant::now(), motion::reduced(cx)),
        );
    }

    /// The disclosed `rows` of `fold`. While it opens or closes its height is a share of the
    /// content's height as last measured (the content is clipped, not reflowed), or under
    /// reduced motion its content fades at full height.
    fn fold(&self, fold: Fold, rows: Vec<AnyElement>, frame: Option<Reveal>) -> AnyElement {
        let heights = Rc::clone(&self.fold_heights);
        let content = div()
            .relative()
            .flex()
            .flex_col()
            .flex_none()
            .gap(metrics::HAIRLINE)
            .children(rows)
            .child(
                canvas(
                    move |bounds, _, _| {
                        heights.borrow_mut().insert(fold, bounds.size.height);
                    },
                    |_, (), _, _| {},
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            );
        let Some(frame) = frame else {
            return content.into_any_element();
        };
        let mut outer = div()
            .flex()
            .flex_col()
            .flex_none()
            .overflow_hidden()
            .opacity(frame.opacity);
        if frame.extent < 1.0 {
            let full = self
                .fold_heights
                .borrow()
                .get(&fold)
                .copied()
                .unwrap_or(Pixels::ZERO);
            outer = outer.h(full * frame.extent);
        }
        outer.child(content).into_any_element()
    }

    /// The heading of a disk that has volumes: chevron, model, size, and the partitions to
    /// scale in a thin bar. Clicking it opens or closes the disk.
    #[allow(
        clippy::too_many_arguments,
        reason = "a heading is built from exactly these parts"
    )]
    fn disk_heading(
        &self,
        disk: &Disk,
        sources: &Sources,
        current: Option<SourceId>,
        is_open: bool,
        index: usize,
        colors: &Colors,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let id = disk.id;
        // One part per partition: the volumes of a container share theirs.
        let parts: Vec<(u64, u64, bool)> = sources
            .spans(disk)
            .iter()
            .map(|span| (span.start, span.len, span.holds(current)))
            .collect();
        let (hover, pressed) = (colors.hover, colors.field);
        let showing = self.view.read(cx).disk() == Some(id);
        div()
            .id(("sidebar-disk", index))
            .flex()
            .flex_col()
            .flex_none()
            .gap(space::S4)
            .py(space::S4)
            .px(metrics::ROW_PADDING)
            .rounded(radius::ROW)
            .cursor_pointer()
            .text_color(colors.ink2)
            .hover(move |style| style.bg(hover))
            .active(move |style| style.bg(pressed))
            .when(showing, |row| row.bg(hover))
            // The heading opens the disk view (and the disk's volumes, when they are folded);
            // the chevron alone folds and unfolds.
            .on_click(cx.listener(move |this, _, _, cx| {
                let now = this.open.get(&id).copied().unwrap_or(is_open);
                if !now {
                    this.open.insert(id, true);
                    this.start_fold(Fold::Disk(id), true, cx);
                }
                this.view.update(cx, |view, cx| view.show_disk(id, cx));
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.open_menu_for(Hit::Disk(id), event.position, window, cx);
                }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(space::S8)
                    .child(
                        div()
                            .id(("sidebar-fold", index))
                            .flex_none()
                            .mt(space::S4)
                            .cursor_pointer()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let now = this.open.get(&id).copied().unwrap_or(is_open);
                                this.open.insert(id, !now);
                                this.start_fold(Fold::Disk(id), !now, cx);
                                cx.notify();
                            }))
                            .child(chevron(is_open, colors)),
                    )
                    .child(div().flex_1().min_w_0().child(disk.label.clone()))
                    .children(disk_size(disk).map(|size| detail_text(size, colors))),
            )
            .child(usage_bar(&bar_segments(&parts, disk.size), colors))
    }

    /// The quiet "N other partitions" row. Closed, hovering it lists the reasons.
    fn others_row(
        &self,
        disk: DiskId,
        others: &[(&Source, SharedString)],
        is_open: bool,
        index: usize,
        colors: &Colors,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let reasons: SharedString = others
            .iter()
            .map(|(source, reason)| format!("{}: {reason}", source.label))
            .collect::<Vec<_>>()
            .join("\n")
            .into();
        let hover = colors.hover;
        let row = div()
            .id(("sidebar-others", index))
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .h(Density::default().sidebar_row_height())
            .pl(metrics::ROW_PADDING + space::S8)
            .pr(space::S8)
            .gap(space::S8)
            .rounded(radius::ROW)
            .cursor_pointer()
            .text_size(text::SMALL.size)
            .text_color(colors.ink3)
            .hover(move |style| style.bg(hover))
            .on_click(cx.listener(move |this, _, _, cx| {
                let opening = !this.others_open.remove(&disk);
                if opening {
                    this.others_open.insert(disk);
                }
                this.start_fold(Fold::Others(disk), opening, cx);
                cx.notify();
            }))
            .child(chevron(is_open, colors))
            .child(other_title(others.len()));
        if is_open {
            row
        } else {
            row.tooltip(move |_, cx| tooltip(reasons.clone(), cx))
        }
    }

    /// One partition behind "other partitions": its name and the reason, selectable so the
    /// content area can say the same thing at more length.
    fn other_item(
        &self,
        index: usize,
        source: &Source,
        reason: SharedString,
        current: Option<SourceId>,
        colors: &Colors,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let target = source.clone();
        let is_current = current == Some(source.id);
        let (hover, pressed) = (colors.hover, colors.field);
        let item = div()
            .id(("sidebar-other", index))
            .flex()
            .flex_col()
            .flex_none()
            .py(space::S4)
            .pl(metrics::ROW_PADDING + space::S8 + space::S16)
            .pr(space::S8)
            .rounded(radius::ROW)
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                let to = Location::root(&target);
                this.navigation
                    .update(cx, |navigation, cx| navigation.navigate(to, cx));
            }))
            .child(
                div()
                    .text_color(colors.ink2)
                    .when(is_current, |name| name.font_weight(FontWeight::BOLD))
                    .child(source.label.clone()),
            )
            .child(
                div()
                    .text_size(text::SMALL.size)
                    .text_color(colors.ink3)
                    .child(reason),
            );
        if is_current {
            item.bg(hover)
        } else {
            item.hover(move |style| style.bg(hover))
                .active(move |style| style.bg(pressed))
        }
    }

    /// The notice under Disks: how many disks need permission, and the way to give it.
    fn permission_notice(&self, count: usize, colors: &Colors, cx: &Context<Self>) -> Div {
        let can_restart = elevation::can_restart_elevated();
        let text = if can_restart {
            permission_notice(count)
        } else {
            format!("{} {}", permission_notice(count), elevation::manual_hint())
        };
        let (hover, pressed, line, ink) = (colors.hover, colors.field, colors.line, colors.ink);
        let notice = div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(space::S8)
            .px(metrics::ROW_PADDING)
            .py(space::S8)
            .child(
                div()
                    .text_size(text::SMALL.size)
                    .text_color(colors.ink2)
                    .child(text),
            );
        if !can_restart {
            return notice;
        }
        notice.child(
            div()
                .id("sidebar-allow-disks")
                .flex()
                .items_center()
                .justify_center()
                .w_full()
                .h(size::CONTROL)
                .rounded(radius::ROW)
                .border_1()
                .border_color(line)
                .text_color(ink)
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .active(move |style| style.bg(pressed))
                .on_click(cx.listener(|this, _, _, cx| this.restart_elevated(cx)))
                .child("Allow reading disks"),
        )
    }
}

/// The disclosure chevron: pointing right when closed, down when open.
fn chevron(is_open: bool, colors: &Colors) -> gpui::Svg {
    let turn = if is_open { FRAC_PI_2 } else { 0.0 };
    svg()
        .path(icon::CHEVRON)
        .flex_none()
        .w(metrics::CHEVRON_WIDTH)
        .h(metrics::CHEVRON_HEIGHT)
        .text_color(colors.ink3)
        .with_transformation(Transformation::rotate(radians(turn)))
}

/// The partitions of a disk to scale, in a bar `DISK_BAR_HEIGHT` tall. The volume being browsed
/// is darker than the rest.
fn usage_bar(segments: &[Segment], colors: &Colors) -> Div {
    let (track, current, other) = (colors.track, colors.ink2, colors.ink3);
    div()
        .relative()
        .w_full()
        .h(metrics::DISK_BAR_HEIGHT)
        .rounded(radius::MARK)
        .overflow_hidden()
        .bg(track)
        .children(segments.iter().map(|segment| {
            div()
                .absolute()
                .top_0()
                .h_full()
                .left(relative(segment.left))
                .w(relative(segment.width))
                .border_l(metrics::HAIRLINE)
                .border_color(track)
                .bg(if segment.current { current } else { other })
        }))
}

/// The tooltip view: `text` in a small raised box.
struct Tooltip {
    text: SharedString,
}

impl Render for Tooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Theme::get(cx).colors;
        div()
            .max_w(size::TOOLTIP_MAX_WIDTH)
            .px(space::S8)
            .py(space::S4)
            .rounded(radius::ROW)
            .border_1()
            .border_color(colors.line)
            .bg(colors.raised)
            .text_size(text::SMALL.size)
            .text_color(colors.ink)
            .child(self.text.clone())
    }
}

pub(crate) fn tooltip(text: SharedString, cx: &mut gpui::App) -> AnyView {
    cx.new(|_| Tooltip { text }).into()
}

/// Right-aligned secondary text of a row.
fn detail_text(text: SharedString, colors: &Colors) -> Div {
    div()
        .flex_none()
        .text_size(text::SMALL.size)
        .text_color(colors.ink3)
        .font_weight(FontWeight::NORMAL)
        .child(text)
}

/// A disk's size as text; nothing for a disk whose size is unknown.
fn disk_size(disk: &Disk) -> Option<SharedString> {
    (disk.size > 0).then(|| SharedString::from(format::size(disk.size)))
}

impl Render for Sidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Theme::get(cx).colors;
        let now = Instant::now();
        self.frame_now = now;
        self.folds
            .retain(|_, disclosure| disclosure.at(now).is_some());
        if !self.folds.is_empty() {
            window.request_animation_frame();
            crate::frame_trace::note(cx, "sidebar-group");
        }
        let current: Option<SourceId> = self
            .navigation
            .read(cx)
            .current()
            .map(|location| location.source());
        let sources = self.sources.clone();
        let sources = sources.read(cx);

        // Host roots and disks, each in the order it was added. A disk the host mounts in full
        // is left out: its volumes are under This computer.
        let entries: Vec<Entry<'_>> = sources
            .items()
            .iter()
            .filter(|source| matches!(source.kind, SourceKind::Host))
            .map(Entry::Host)
            .chain(
                sources
                    .disks()
                    .iter()
                    .filter(|disk| {
                        disk.group != SourceGroup::Disks || !sources.is_host_mounted(disk)
                    })
                    .map(Entry::Disk),
            )
            .collect();
        let groups: Vec<SourceGroup> = entries.iter().map(Entry::group).collect();
        // Sparse bundles are folders: where the prompt cannot pick files and folders together,
        // they need a control of their own.
        let folder_button = !cx.can_select_mixed_files_and_dirs();

        let mut list = div()
            .id("sidebar-list")
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(space::S16)
            .pt(space::S8)
            .px(space::S8)
            .pb(metrics::ROW_PADDING)
            .overflow_y_scroll();

        let mut next_index = 0usize;
        for (group, indices) in group_indices(&groups) {
            let mut rows: Vec<AnyElement> = Vec::new();
            for entry in indices.into_iter().filter_map(|index| entries.get(index)) {
                match entry {
                    Entry::Host(source) => {
                        next_index += 1;
                        let detail = sources
                            .host_disk_label(source.id)
                            .or_else(|| source.format.clone());
                        let row = self.row(
                            next_index,
                            source,
                            detail,
                            false,
                            current == Some(source.id),
                            &colors,
                            cx,
                        );
                        rows.push(row.into_any_element());
                    }
                    Entry::Disk(disk) => {
                        rows.extend(self.disk_rows(
                            disk,
                            &mut next_index,
                            current,
                            sources,
                            &colors,
                            cx,
                        ));
                    }
                }
            }
            let locked = if group == SourceGroup::Disks {
                sources
                    .disks()
                    .iter()
                    .filter(|disk| disk.group == SourceGroup::Disks)
                    .filter(|disk| !sources.is_host_mounted(disk))
                    .filter_map(|disk| disk.whole.and_then(|id| sources.source(id)))
                    .filter(|source| source.availability == Availability::NeedsElevation)
                    .count()
            } else {
                0
            };
            if locked > 0 {
                rows.push(
                    self.permission_notice(locked, &colors, cx)
                        .into_any_element(),
                );
            }
            list = list.child(
                div()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .child(
                        div()
                            .px(space::S8)
                            .pb(space::S4)
                            .text_size(text::SMALL_BOLD.size)
                            .font_weight(text::SMALL_BOLD.weight)
                            .text_color(colors.ink3)
                            .child(group.title()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(metrics::HAIRLINE)
                            .children(rows),
                    ),
            );
        }

        div()
            .id("sidebar")
            .key_context(SIDEBAR)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::open_menu))
            .on_action(cx.listener(Self::context_open))
            .on_action(cx.listener(Self::context_search))
            .on_action(cx.listener(Self::context_properties))
            .on_action(cx.listener(Self::context_allow))
            .on_action(cx.listener(Self::context_copy_device_path))
            .on_action(cx.listener(Self::context_reveal))
            .size_full()
            .flex()
            .flex_col()
            .child(list)
            .child(open_image_buttons(&colors, folder_button, cx))
            .children(self.menu.entity())
    }
}

/// The controls at the bottom of the sidebar: "Open disk image" (Ctrl+O) and, when
/// `with_folder` is set, "Open disk image folder" (Ctrl+Shift+O) directly under it.
fn open_image_buttons(colors: &Colors, with_folder: bool, cx: &Context<Sidebar>) -> Div {
    let mut buttons = div()
        .flex_none()
        .flex()
        .flex_col()
        .gap(space::S4)
        .px(space::S8)
        .pb(space::S8)
        .child(image_button(
            "open-disk-image",
            "Open disk image",
            OpenImage,
            colors,
            cx,
        ));
    if with_folder {
        buttons = buttons.child(image_button(
            "open-disk-image-folder",
            "Open disk image folder",
            OpenImageFolder,
            colors,
            cx,
        ));
    }
    buttons
}

/// A plain full-width control that dispatches `action` when clicked.
fn image_button(
    id: &'static str,
    label: &'static str,
    action: impl Action,
    colors: &Colors,
    cx: &Context<Sidebar>,
) -> Stateful<Div> {
    let (hover, pressed) = (colors.hover, colors.field);
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .w_full()
        .h(size::CONTROL)
        .rounded(radius::ROW)
        .border_1()
        .border_color(colors.line)
        .text_color(colors.ink)
        .cursor_pointer()
        .hover(move |style| style.bg(hover))
        .active(move |style| style.bg(pressed))
        .on_click(cx.listener(move |_, _: &ClickEvent, window, cx| {
            window.dispatch_action(action.boxed_clone(), cx);
        }))
        .child(label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use SourceGroup::{DiskImages, Disks, ThisComputer};

    fn unsupported(format: &'static str) -> Availability {
        Availability::Unsupported {
            format: format.into(),
        }
    }

    #[test]
    fn a_readable_data_volume_is_listed() {
        assert_eq!(placement(None, &Availability::Browsable), Placement::Listed);
    }

    #[test]
    fn a_system_partition_is_behind_other_even_when_readable() {
        let Placement::Other(reason) =
            placement(Some(SystemKind::AppleBoot), &Availability::Browsable)
        else {
            panic!("expected Other");
        };
        assert!(reason.starts_with("Apple boot or recovery partition."));
    }

    #[test]
    fn a_format_with_no_reader_gives_its_reason() {
        assert_eq!(
            placement(None, &unsupported("NTFS")),
            Placement::Other(
                "NTFS. Disk Atlas cannot read NTFS yet; Windows can, if you mount it.".into()
            )
        );
        assert_eq!(
            placement(None, &unsupported("ext")),
            Placement::Other("ext. Disk Atlas cannot read ext yet.".into())
        );
        assert_eq!(
            placement(None, &unsupported("Unknown")),
            Placement::Other("Disk Atlas does not recognise what is stored here.".into())
        );
    }

    #[test]
    fn the_role_follows_a_name_that_does_not_say_it() {
        assert_eq!(
            role_note("Macintosh HD", Some(VolumeRole::System)),
            Some("System")
        );
        assert_eq!(
            role_note("Macintosh HD - Data", Some(VolumeRole::Data)),
            None
        );
        assert_eq!(role_note("RECOVERY", Some(VolumeRole::Recovery)), None);
        assert_eq!(role_note("Backups", None), None);
    }

    #[test]
    fn a_failed_mount_keeps_its_own_words() {
        let failed = Availability::Failed("This volume could not be read (bad header).".into());
        assert_eq!(
            placement(None, &failed),
            Placement::Other("This volume could not be read (bad header).".into())
        );
    }

    #[test]
    fn titles_and_notices_follow_the_count() {
        assert_eq!(other_title(1), "1 other partition");
        assert_eq!(other_title(3), "3 other partitions");
        assert_eq!(
            permission_notice(2),
            "2 disks need permission before Disk Atlas can read them."
        );
        assert_eq!(
            permission_notice(1),
            "1 disk needs permission before Disk Atlas can read it."
        );
    }

    #[test]
    fn bar_segments_scale_and_clamp() {
        let segments = bar_segments(
            &[(0, 250, false), (250, 500, true), (900, 500, false)],
            1000,
        );
        assert_eq!(
            segments,
            vec![
                Segment {
                    left: 0.0,
                    width: 0.25,
                    current: false
                },
                Segment {
                    left: 0.25,
                    width: 0.5,
                    current: true
                },
                Segment {
                    left: 0.9,
                    width: 0.1,
                    current: false
                },
            ]
        );
        // Empty and out-of-range pieces are dropped; an unknown size gives no bar.
        assert!(bar_segments(&[(0, 0, false), (2000, 10, false)], 1000).is_empty());
        assert!(bar_segments(&[(0, 10, false)], 0).is_empty());
    }

    #[test]
    fn empty_input_gives_no_groups() {
        assert!(group_indices(&[]).is_empty());
    }

    #[test]
    fn groups_follow_the_fixed_order_and_keep_item_order() {
        let groups = [ThisComputer, Disks, ThisComputer, DiskImages, Disks];
        assert_eq!(
            group_indices(&groups),
            vec![
                (Disks, vec![1, 4]),
                (DiskImages, vec![3]),
                (ThisComputer, vec![0, 2]),
            ]
        );
    }

    #[test]
    fn groups_without_sources_are_left_out() {
        assert_eq!(
            group_indices(&[ThisComputer, ThisComputer]),
            vec![(ThisComputer, vec![0, 1])]
        );
    }
}
