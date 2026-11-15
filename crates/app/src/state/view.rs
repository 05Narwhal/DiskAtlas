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

//! How the folder shown is presented: as a list, as the space map (`r1-redesign.md` 3.5), or as
//! a disk view (3.4, the disk itself with the map of its volume).
//!
//! The space map and the disk view follow the navigation: they draw the folder the list would
//! draw. Going into a region of the map navigates, so the breadcrumb, Back and Backspace all
//! work. A navigation that did not come from the map ends the disk view.

use disk_atlas_lib::fs::NodeId;
use disk_atlas_lib::search::kind::KindClass;
use gpui::{Context, Entity, EventEmitter, Subscription};

use super::navigation::{Location, Navigation};
use super::sources::{DiskId, SourceId, Sources};

/// What the main area shows for the folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    /// The file list (Ctrl+1).
    #[default]
    List,
    /// The space map (Ctrl+3).
    SpaceMap,
}

/// What changed in [`ViewState`].
pub enum ViewEvent {
    /// The mode, the disk or the highlighted kind changed.
    Changed,
}

impl EventEmitter<ViewEvent> for ViewState {}

/// The mode, and the disk view when one is open.
pub struct ViewState {
    sources: Entity<Sources>,
    navigation: Entity<Navigation>,
    mode: ViewMode,
    disk: Option<DiskId>,
    /// The folder the map itself is navigating to: arriving there keeps the disk view.
    expected: Option<(SourceId, NodeId)>,
    /// A kind the map emphasises ("Show on the space map" in the storage bar).
    highlight: Option<KindClass>,
    /// The disk view shows the Layout drawing of the bar (R15.2), not Contents.
    layout_mode: bool,
    _subscriptions: Vec<Subscription>,
}

/// Whether arriving at `now` ends a disk view whose own navigation is heading for `expected`.
fn leaves_disk_view(expected: Option<(SourceId, NodeId)>, now: Option<(SourceId, NodeId)>) -> bool {
    expected != now
}

impl ViewState {
    /// Follows `navigation`: a folder the map did not ask for ends the disk view.
    pub fn new(
        sources: Entity<Sources>,
        navigation: Entity<Navigation>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![cx.observe(&navigation, |this, navigation, cx| {
            let here = navigation
                .read(cx)
                .current()
                .map(|location| (location.source(), location.node()));
            if this.disk.is_some() && leaves_disk_view(this.expected, here) {
                this.disk = None;
                this.layout_mode = false;
                this.expected = None;
                this.changed(cx);
            } else if this.expected == here {
                this.expected = None;
            }
        })];
        Self {
            sources,
            navigation,
            mode: ViewMode::List,
            disk: None,
            expected: None,
            highlight: None,
            layout_mode: false,
            _subscriptions: subscriptions,
        }
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        cx.emit(ViewEvent::Changed);
        cx.notify();
    }

    /// The mode chosen with Ctrl+1 and Ctrl+3.
    pub fn mode(&self) -> ViewMode {
        self.mode
    }

    /// The disk whose view is open.
    pub fn disk(&self) -> Option<DiskId> {
        self.disk
    }

    /// The map is on screen: in its own mode or in the disk view.
    pub fn shows_map(&self) -> bool {
        self.mode == ViewMode::SpaceMap || self.disk.is_some()
    }

    /// The disk view draws the Layout of the disk: occupancy relief and the selected file's
    /// pieces.
    pub fn layout_mode(&self) -> bool {
        self.layout_mode
    }

    /// Chooses Contents or Layout in the disk view.
    pub fn set_layout_mode(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.layout_mode != on {
            self.layout_mode = on;
            self.changed(cx);
        }
    }

    /// The kind the map emphasises.
    pub fn highlight(&self) -> Option<KindClass> {
        self.highlight
    }

    /// Chooses a mode; that leaves the disk view and any kind being emphasised.
    pub fn set_mode(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        if self.mode == mode && self.disk.is_none() && self.highlight.is_none() {
            return;
        }
        self.mode = mode;
        self.disk = None;
        self.highlight = None;
        self.layout_mode = false;
        self.changed(cx);
    }

    /// Opens the view of `disk`: the disk with the map of its current volume, or of its first
    /// readable one when the folder shown is on another disk.
    pub fn show_disk(&mut self, disk: DiskId, cx: &mut Context<Self>) {
        self.show_disk_in(disk, false, cx);
    }

    /// [`Self::show_disk`] in Layout mode: "Show where it is on the disk".
    pub fn show_disk_layout(&mut self, disk: DiskId, cx: &mut Context<Self>) {
        self.show_disk_in(disk, true, cx);
    }

    fn show_disk_in(&mut self, disk: DiskId, layout: bool, cx: &mut Context<Self>) {
        let target = {
            let sources = self.sources.read(cx);
            let here = self
                .navigation
                .read(cx)
                .current()
                .map(Location::source)
                .and_then(|id| sources.source(id));
            let on_this_disk = here.is_some_and(|source| {
                matches!(source.kind, super::sources::SourceKind::Volume { disk: d, .. } if d == disk)
                    && source.availability.is_browsable()
            });
            if on_this_disk {
                None
            } else {
                sources.disk(disk).and_then(|disk| {
                    disk.volumes
                        .iter()
                        .filter_map(|&id| sources.source(id))
                        .find(|source| source.availability.is_browsable())
                        .map(Location::root)
                })
            }
        };
        self.highlight = None;
        self.layout_mode = layout;
        self.disk = Some(disk);
        if let Some(target) = target {
            self.expected = Some((target.source(), target.node()));
            self.navigation
                .update(cx, |navigation, cx| navigation.navigate(target, cx));
        }
        self.changed(cx);
    }

    /// Navigates from the map without ending the disk view.
    pub fn navigate_from_map(&mut self, to: Location, cx: &mut Context<Self>) {
        self.expected = Some((to.source(), to.node()));
        self.navigation
            .update(cx, |navigation, cx| navigation.navigate(to, cx));
    }

    /// Shows the map with `kind` emphasised.
    pub fn show_kind_on_map(&mut self, kind: KindClass, cx: &mut Context<Self>) {
        self.mode = ViewMode::SpaceMap;
        self.disk = None;
        self.highlight = Some(kind);
        self.changed(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_folder_the_map_asked_for_keeps_the_disk_view() {
        let a = Some((SourceId::for_test(1), NodeId(5)));
        let b = Some((SourceId::for_test(1), NodeId(6)));
        assert!(!leaves_disk_view(a, a));
        assert!(leaves_disk_view(a, b));
        assert!(leaves_disk_view(None, a));
        assert!(leaves_disk_view(a, None));
    }
}
