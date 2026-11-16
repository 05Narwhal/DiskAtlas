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

//! One tile of the icon view (`r1-redesign.md` 1.7): a picture or the kind's glyph on its wash
//! tile, and the name under it in at most two lines.

use std::sync::Arc;

use disk_atlas_lib::fs::NodeKind;
use disk_atlas_lib::mac::SystemItem;
use disk_atlas_lib::search::kind::classify_entry;
use gpui::{
    App, ElementId, ImageSource, IntoElement, MouseButton, MouseDownEvent, ObjectFit, Pixels,
    RenderImage, RenderOnce, SharedString, StyledImage, Window, div, img, prelude::*, px, svg,
};

use crate::state::icon_grid::LABEL_HEIGHT;
use crate::state::listing::Row;
use crate::theme::{
    Glyph, Theme, metrics,
    scale::{radius, space},
};

type MouseDownHandler = Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App) + 'static>;

/// One entry of the folder, as a tile.
#[derive(IntoElement)]
pub struct IconTile {
    name: SharedString,
    glyph: Glyph,
    /// Decoded picture of the file, when the thumbnail pipeline has one.
    thumbnail: Option<Arc<RenderImage>>,
    link: bool,
    editing: Option<crate::state::rename::RenameEdit>,
    selected: bool,
    cursor: bool,
    system: bool,
    position: usize,
    side: Pixels,
    on_mouse_down: Option<MouseDownHandler>,
    on_context_menu: Option<MouseDownHandler>,
}

impl IconTile {
    /// The tile for `row` at list `position`, `side` pixels across.
    pub fn new(row: &Row<'_>, position: usize, side: Pixels) -> Self {
        let entry = &row.entry;
        Self {
            name: SharedString::from(entry.name.to_owned()),
            glyph: Glyph::from_class(classify_entry(entry.name, entry.is_browsable(), None)),
            thumbnail: None,
            link: entry.kind == NodeKind::Symlink,
            editing: None,
            selected: row.selected,
            cursor: row.cursor,
            system: false,
            position,
            side,
            on_mouse_down: None,
            on_context_menu: None,
        }
    }

    /// Draws `image` in place of the glyph.
    pub fn thumbnail(mut self, image: Option<Arc<RenderImage>>) -> Self {
        self.thumbnail = image;
        self
    }

    /// Draws the name as an edit field.
    pub fn editing(mut self, edit: Option<crate::state::rename::RenameEdit>) -> Self {
        self.editing = edit;
        self
    }

    /// Marks a housekeeping item: drawn quieter.
    pub fn system(mut self, item: Option<SystemItem>) -> Self {
        self.system = item.is_some();
        self
    }

    /// Called on a left press on the tile.
    pub fn on_mouse_down(
        mut self,
        handler: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_mouse_down = Some(Box::new(handler));
        self
    }

    /// Called on a right press on the tile.
    pub fn on_context_menu(
        mut self,
        handler: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_context_menu = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for IconTile {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let colors = Theme::get(cx).colors;
        let (name_color, icon_color) = if self.selected {
            (colors.on_mark, colors.on_mark)
        } else {
            (colors.ink, colors.tint(self.glyph))
        };
        let outlined = self.cursor && !self.selected;
        // The picture's box: the tile less the padding, square.
        let well = self.side - space::S16;
        let glyph_side = (well * 0.5).max(px(16.));

        let picture = match self.thumbnail {
            Some(image) => div()
                .size(well)
                .flex()
                .items_center()
                .justify_center()
                .child(
                    img(ImageSource::Render(image))
                        .max_w(well)
                        .max_h(well)
                        .object_fit(ObjectFit::Contain)
                        .rounded(radius::MARK),
                ),
            None => div()
                .size(well)
                .flex()
                .items_center()
                .justify_center()
                .rounded(radius::WELL)
                .when(!self.selected, |tile| tile.bg(colors.wash(self.glyph)))
                .child(
                    svg()
                        .path(self.glyph.path())
                        .size(glyph_side)
                        .text_color(icon_color),
                ),
        };

        let mut label = div()
            .w_full()
            .h(px(LABEL_HEIGHT))
            .overflow_hidden()
            .text_center()
            .line_clamp(2)
            .text_color(name_color)
            .child(self.name);
        if self.link {
            label = label.italic();
        }
        if let Some(edit) = &self.editing {
            label = div()
                .w_full()
                .h(px(LABEL_HEIGHT))
                .flex()
                .justify_center()
                .child(crate::components::rename_text::rename_text(edit, &colors));
        }

        let mut tile = div()
            .id(ElementId::from(("icon-tile", self.position)))
            .flex_none()
            .w(self.side)
            .h(self.side + px(LABEL_HEIGHT))
            .flex()
            .flex_col()
            .items_center()
            .gap(space::S4)
            .p(space::S8)
            .pb(space::S4)
            .rounded(radius::ROW)
            .child(picture)
            .child(label);
        tile = if self.selected {
            tile.bg(colors.mark)
        } else {
            tile.hover(|style| style.bg(colors.hover))
        };
        if outlined {
            tile = tile.border(metrics::HAIRLINE).border_color(colors.ink3);
        }
        if self.system {
            tile = tile.opacity(metrics::SYSTEM_ITEM_OPACITY);
        }
        if let Some(handler) = self.on_mouse_down {
            tile = tile.on_mouse_down(MouseButton::Left, handler);
        }
        if let Some(handler) = self.on_context_menu {
            tile = tile.on_mouse_down(MouseButton::Right, handler);
        }
        tile
    }
}
