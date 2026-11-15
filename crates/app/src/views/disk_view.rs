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

//! The disk view (`r1-redesign.md` 3.4): what a disk is, over the space map of its volume.
//!
//! The partition bar and its Contents | Layout switch are the storage bar's, which the window
//! keeps above the content; this view adds the disk's name, connection, size and map scheme, and
//! the map of the current volume under them.

use gpui::{
    Context, Entity, IntoElement, Render, SharedString, Subscription, Window, div, prelude::*,
};

use crate::format;
use crate::state::facts::scheme_name;
use crate::state::sources::{SourceGroup, Sources};
use crate::state::view::ViewState;
use crate::theme::Theme;
use crate::theme::scale::{space, text};
use crate::views::space_map::SpaceMapView;

/// The disk view.
pub struct DiskView {
    sources: Entity<Sources>,
    view: Entity<ViewState>,
    map: Entity<SpaceMapView>,
    _subscriptions: Vec<Subscription>,
}

impl DiskView {
    /// The header reads the disk `view` names; `map` is drawn under it.
    pub fn new(
        sources: Entity<Sources>,
        view: Entity<ViewState>,
        map: Entity<SpaceMapView>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.observe(&sources, |_, _, cx| cx.notify()),
            cx.observe(&view, |_, _, cx| cx.notify()),
        ];
        Self {
            sources,
            view,
            map,
            _subscriptions: subscriptions,
        }
    }
}

/// "Removable disk · 931.5 GB · GUID Partition Table", the parts that are known.
fn detail_line(group: SourceGroup, removable: bool, size: u64, scheme: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(
        match (group, removable) {
            (SourceGroup::DiskImages, _) => "Disk image",
            (_, true) => "Removable disk",
            _ => "Disk",
        }
        .to_owned(),
    );
    if size > 0 {
        parts.push(format::size(size));
    }
    parts.push(scheme.map_or_else(|| "No partition map".to_owned(), str::to_owned));
    parts.join(" · ")
}

impl Render for DiskView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Theme::get(cx).colors;
        let header = self.view.read(cx).disk().and_then(|id| {
            let sources = self.sources.read(cx);
            let disk = sources.disk(id)?;
            let detail = detail_line(
                disk.group,
                disk.removable,
                disk.size,
                disk.scheme.map(scheme_name),
            );
            Some((SharedString::from(disk.label.to_string()), detail))
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .children(header.map(|(name, detail)| {
                div()
                    .flex_none()
                    .px(space::S16)
                    .pt(space::S12)
                    .pb(space::S8)
                    .child(
                        div()
                            .text_size(text::HEADING.size)
                            .text_color(colors.ink)
                            .font_weight(text::HEADING.weight)
                            .child(name),
                    )
                    .child(
                        div()
                            .text_size(text::SMALL.size)
                            .text_color(colors.ink3)
                            .child(detail),
                    )
            }))
            .child(div().flex_1().min_h_0().child(self.map.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_detail_line_names_what_is_known() {
        assert_eq!(
            detail_line(
                SourceGroup::Disks,
                true,
                1_000_000_000_000,
                Some("GUID Partition Table")
            ),
            "Removable disk · 931.3 GB · GUID Partition Table"
        );
        assert_eq!(
            detail_line(SourceGroup::DiskImages, false, 0, None),
            "Disk image · No partition map"
        );
    }
}
