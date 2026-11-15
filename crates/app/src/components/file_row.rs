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

//! One row of the file list, and the column layout it shares with the column header.
//!
//! A row is built only for a visible position, from a borrowed entry; the only allocations are
//! the texts it shows.

use std::collections::HashMap;
use std::ops::Range;

use disk_atlas_lib::fs::NodeKind;
use disk_atlas_lib::mac::SystemItem;
use disk_atlas_lib::mac::bundle;
use disk_atlas_lib::search::kind::classify_entry;
use gpui::{
    App, Div, ElementId, FontWeight, HighlightStyle, Hsla, IntoElement, MouseButton,
    MouseDownEvent, Pixels, RenderOnce, SharedString, StyledText, Window, div, prelude::*, svg,
};

use crate::components::sidebar::tooltip;
use crate::format;
use crate::state::listing::Row;
use crate::state::settings::Settings;
use crate::state::sizes::{LOWER_BOUND_TIP, SizeState, size_text};
use crate::theme::colors::label_colour;
use crate::theme::{
    Glyph, Theme, metrics,
    scale::{Density, radius},
};

type MouseDownHandler = Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App) + 'static>;
type HoverHandler = Box<dyn Fn(&bool, &mut Window, &mut App) + 'static>;

/// The outer box of a row or of the column header: full width, inset from the panel's edges.
pub fn row_frame() -> Div {
    div()
        .flex_none()
        .w_full()
        .h(Density::default().row_height())
        .px(metrics::ROW_INSET)
}

/// The inner box holding the columns, `outline` pixels of border inside its padding.
pub fn columns(outline: Pixels) -> Div {
    div()
        .size_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(metrics::COLUMN_GAP)
        .px(metrics::ROW_PADDING - outline)
}

/// The Name column: takes what the others leave.
pub fn name_cell() -> Div {
    div()
        .flex_1()
        .min_w(metrics::COLUMN_NAME_MIN)
        .flex()
        .flex_row()
        .items_center()
        .overflow_hidden()
}

/// The Folder column of search results: shares the free space with the Name column.
pub fn folder_cell() -> Div {
    div().flex_1().min_w(metrics::COLUMN_FOLDER_MIN).truncate()
}

/// What separates the names of a folder path in the Folder column. The listing builds the paths
/// with the same one.
const FOLDER_SEPARATOR: &str = "/";

/// How many fitted paths the cache keeps; a screenful is a few dozen.
const FOLDER_CACHE_CAPACITY: usize = 512;

/// Folder paths already cut to the width of the Folder column, so a repaint at the same width
/// and font size shapes no text.
///
/// Keyed by the path text. It forgets everything when the width or the font size changes, and
/// when it holds [`FOLDER_CACHE_CAPACITY`] paths.
#[derive(Debug, Default)]
pub struct FolderCache {
    /// The width and font size the entries were fitted for.
    fitted_for: Option<(Pixels, Pixels)>,
    entries: HashMap<SharedString, SharedString>,
}

impl FolderCache {
    /// Forgets every path.
    pub fn clear(&mut self) {
        self.fitted_for = None;
        self.entries.clear();
    }

    /// The text of `folder` in a column `width` wide: the path itself when it fits, otherwise
    /// with whole names cut from the middle (see [`format::elide_path_middle`]). Text is
    /// measured with the window's current text style, which is the one the cell inherits.
    /// A column not measured yet (`width` zero) shows the path as it is.
    pub fn fit(&mut self, window: &Window, folder: SharedString, width: Pixels) -> SharedString {
        // A hairline of slack, so a path measured to the exact width is not cut by rounding.
        let available = width - metrics::HAIRLINE;
        if folder.is_empty() || available <= Pixels::ZERO {
            return folder;
        }
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        self.get_or_fit(available, font_size, folder, |folder| {
            let system = window.text_system();
            let measure = |text: &str| {
                if text.is_empty() {
                    return 0.0;
                }
                let width = system
                    .layout_line(text, font_size, &[style.to_run(text.len())], None)
                    .width;
                f32::from(width)
            };
            format::elide_path_middle(folder, FOLDER_SEPARATOR, f32::from(available), measure)
                .into_owned()
        })
    }

    /// The cached text for `folder`, or what `elide` makes of it. `elide` runs only on a miss.
    fn get_or_fit(
        &mut self,
        width: Pixels,
        font_size: Pixels,
        folder: SharedString,
        elide: impl FnOnce(&str) -> String,
    ) -> SharedString {
        if self.fitted_for != Some((width, font_size)) {
            self.entries.clear();
            self.fitted_for = Some((width, font_size));
        }
        if let Some(fitted) = self.entries.get(&folder) {
            return fitted.clone();
        }
        let elided = elide(&folder);
        let fitted = if elided.as_str() == folder.as_ref() {
            folder.clone()
        } else {
            SharedString::from(elided)
        };
        if self.entries.len() >= FOLDER_CACHE_CAPACITY {
            self.entries.clear();
        }
        self.entries.insert(folder, fitted.clone());
        fitted
    }
}

/// The Modified column.
pub fn modified_cell() -> Div {
    div()
        .flex_none()
        .w(metrics::COLUMN_MODIFIED)
        .whitespace_nowrap()
        .overflow_hidden()
}

/// The Size column, right-aligned.
pub fn size_cell() -> Div {
    div()
        .flex_none()
        .w(metrics::COLUMN_SIZE)
        .flex()
        .flex_row()
        .justify_end()
        .whitespace_nowrap()
        .overflow_hidden()
}

/// The Resource fork column, right-aligned.
pub fn resource_cell() -> Div {
    div()
        .flex_none()
        .w(metrics::COLUMN_RESOURCE_FORK)
        .flex()
        .flex_row()
        .justify_end()
        .whitespace_nowrap()
        .overflow_hidden()
}

/// The Kind column.
pub fn kind_cell() -> Div {
    div().flex_none().w(metrics::COLUMN_KIND).truncate()
}

/// One entry of the folder, as a list row.
#[derive(IntoElement)]
pub struct FileRow {
    name: SharedString,
    /// Byte ranges of `name` shown in bold: where the search terms matched.
    highlights: Vec<Range<usize>>,
    /// The folder holding the entry. `Some` only in search results, which add the column.
    folder: Option<SharedString>,
    modified: Option<SharedString>,
    size: Option<SharedString>,
    /// True when `size` is a folder's lower bound: drawn in the quiet ink, with a tooltip.
    size_bound: bool,
    /// True when `size` is a folder's total so far, still being measured: drawn in the quiet ink.
    size_running: bool,
    /// `None` when the column is not shown; `Some(None)` for a row with no fork to report.
    resource: Option<Option<SharedString>>,
    kind: &'static str,
    glyph: Glyph,
    /// The entry is a package: drawn as one item unless `packages_as_folders` is on.
    package: Option<&'static str>,
    /// The Finder label, 0 to 7.
    label: u8,
    link: bool,
    selected: bool,
    cursor: bool,
    /// True for a housekeeping item: drawn quieter.
    system: bool,
    /// The list position, which keys the row's element (its hover state).
    position: usize,
    /// How much of the hover fill to draw, `0.0..=1.0`; `None` for a row not under the pointer.
    /// The list owns hover so that only one row fades at a time (`r1-redesign.md` 2.2).
    hover: Option<f32>,
    /// True while the gliding selection plate under the list draws this row's fill.
    under_plate: bool,
    on_mouse_down: Option<MouseDownHandler>,
    on_context_menu: Option<MouseDownHandler>,
    on_hover: Option<HoverHandler>,
}

impl FileRow {
    /// The row for `row` at list `position`; `resource_column` says whether the list shows the
    /// Resource fork column.
    pub fn new(row: &Row<'_>, position: usize, resource_column: bool, local_times: bool) -> Self {
        let entry = &row.entry;
        let browsable = entry.is_browsable();
        Self {
            name: SharedString::from(entry.name.to_owned()),
            highlights: Vec::new(),
            folder: None,
            modified: entry
                .modified
                .map(|time| SharedString::from(format::date_time_on(time, local_times))),
            size: if browsable {
                size_text(row.size)
            } else {
                entry
                    .size
                    .map(|bytes| SharedString::from(format::size(bytes)))
            },
            size_bound: browsable && matches!(row.size, SizeState::AtLeast(_)),
            size_running: browsable && matches!(row.size, SizeState::Running(_)),
            resource: resource_column.then(|| {
                entry
                    .resource_size
                    .filter(|&bytes| bytes > 0)
                    .map(|bytes| SharedString::from(format::size(bytes)))
            }),
            kind: format::kind(entry.name, entry.kind),
            glyph: Glyph::from_class(classify_entry(entry.name, browsable, None)),
            package: bundle::is_package(entry.name, entry.kind, entry.flags)
                .then(|| bundle::package_kind(entry.name)),
            label: entry.flags.label(),
            link: entry.kind == NodeKind::Symlink,
            selected: row.selected,
            cursor: row.cursor,
            system: false,
            position,
            hover: None,
            under_plate: false,
            on_mouse_down: None,
            on_context_menu: None,
            on_hover: None,
        }
    }

    /// Draws `amount` of the hover fill (`None`: the row is not under the pointer).
    pub fn hover(mut self, amount: Option<f32>) -> Self {
        self.hover = amount;
        self
    }

    /// Leaves the selection fill to the gliding plate drawn under the list.
    pub fn under_plate(mut self, under_plate: bool) -> Self {
        self.under_plate = under_plate;
        self
    }

    /// Called when the pointer starts (`true`) or stops (`false`) being over the row.
    pub fn on_hover(mut self, handler: impl Fn(&bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_hover = Some(Box::new(handler));
        self
    }

    /// Marks the row as a housekeeping item: its Kind column says what it is, and it is drawn at
    /// [`metrics::SYSTEM_ITEM_OPACITY`].
    pub fn system(mut self, item: Option<SystemItem>) -> Self {
        if let Some(item) = item {
            self.kind = item.label();
            self.system = true;
        }
        self
    }

    /// Makes the row a search result: adds the Folder column with `folder` and shows the
    /// `highlights` (byte ranges of the name on character boundaries) in bold.
    pub fn in_results(mut self, folder: SharedString, highlights: Vec<Range<usize>>) -> Self {
        self.folder = Some(folder);
        self.highlights = highlights;
        self
    }

    /// Called on a right press on the row, inset included.
    pub fn on_context_menu(
        mut self,
        handler: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_context_menu = Some(Box::new(handler));
        self
    }

    /// Called on a left press on the row, inset included.
    pub fn on_mouse_down(
        mut self,
        handler: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_mouse_down = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for FileRow {
    fn render(mut self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let colors = Theme::get(cx).colors;
        let settings = cx.try_global::<Settings>();
        let as_folders = settings.is_some_and(|s| s.bool_of("packages_as_folders"));
        let label_dots = settings.is_none_or(|s| s.bool_of("label_dots"));
        if let Some(kind) = self.package
            && !as_folders
        {
            self.glyph = Glyph::Application;
            self.kind = kind;
        }
        let label_dot = label_dots
            .then(|| label_colour(self.label))
            .flatten()
            .map(|colour| {
                div()
                    .flex_none()
                    .size(metrics::LABEL_DOT)
                    .rounded_full()
                    .bg(colour)
                    .border_1()
                    .border_color(colors.line)
            });
        let size_id = self.name.clone();
        let (name_color, other_color, icon_color): (Hsla, Hsla, Hsla) = if self.selected {
            (colors.on_mark, colors.on_mark, colors.on_mark)
        } else {
            (colors.ink, colors.ink2, colors.tint(self.glyph))
        };
        let density = Density::default();
        let icon_size = density.icon_size();
        // A compact row draws the 16 px glyph bare; a taller one sets it on its tile.
        let glyph_size = metrics::ICON.min(icon_size);
        // The cursor gets an outline unless the selection color already marks it.
        let outlined = self.cursor && !self.selected;
        let outline = if outlined {
            metrics::HAIRLINE
        } else {
            Pixels::ZERO
        };

        let mut inner = columns(outline).rounded(radius::ROW);
        if self.selected {
            if !self.under_plate {
                inner = inner.bg(colors.mark);
            }
        } else if let Some(amount) = self.hover {
            inner = inner.bg(colors.hover.opacity(amount));
        }
        if outlined {
            inner = inner.border(metrics::HAIRLINE).border_color(colors.ink3);
        }

        let mut name = div().flex_1().min_w_0().truncate().text_color(name_color);
        name = if self.highlights.is_empty() {
            name.child(self.name)
        } else {
            let bold = HighlightStyle {
                font_weight: Some(FontWeight::BOLD),
                ..HighlightStyle::default()
            };
            name.child(
                StyledText::new(self.name)
                    .with_highlights(self.highlights.into_iter().map(|range| (range, bold))),
            )
        };
        if self.link {
            name = name.italic();
        }

        let inner = inner
            .child(
                name_cell()
                    .gap(metrics::ICON_GAP)
                    .child(
                        div()
                            .flex_none()
                            .size(icon_size)
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(radius::MARK)
                            .when(!self.selected, |tile| tile.bg(colors.wash(self.glyph)))
                            .child(
                                svg()
                                    .path(self.glyph.path())
                                    .flex_none()
                                    .size(glyph_size)
                                    .text_color(icon_color),
                            ),
                    )
                    .children(label_dot)
                    .child(name),
            )
            .children(
                self.folder
                    .map(|folder| folder_cell().text_color(other_color).child(folder)),
            )
            .child(
                modified_cell()
                    .text_color(other_color)
                    .children(self.modified),
            )
            .child(if self.size_bound {
                let tip = SharedString::new_static(LOWER_BOUND_TIP);
                let quiet = if self.selected {
                    other_color
                } else {
                    colors.ink3
                };
                size_cell()
                    .id((ElementId::from("size-bound"), size_id))
                    .text_color(quiet)
                    .tooltip(move |_, cx| tooltip(tip.clone(), cx))
                    .children(self.size)
                    .into_any_element()
            } else if self.size_running {
                // A total still growing: quiet, with no tooltip.
                let quiet = if self.selected {
                    other_color
                } else {
                    colors.ink3
                };
                size_cell()
                    .text_color(quiet)
                    .children(self.size)
                    .into_any_element()
            } else {
                size_cell()
                    .text_color(other_color)
                    .children(self.size)
                    .into_any_element()
            })
            .children(
                self.resource
                    .map(|text| resource_cell().text_color(other_color).children(text)),
            )
            .child(kind_cell().text_color(other_color).child(self.kind));

        let inner = inner.when(self.system, |inner| {
            inner.opacity(metrics::SYSTEM_ITEM_OPACITY)
        });
        let mut frame = row_frame().id(("file-row", self.position)).child(inner);
        if let Some(handler) = self.on_hover {
            frame = frame.on_hover(handler);
        }
        if let Some(handler) = self.on_mouse_down {
            frame = frame.on_mouse_down(MouseButton::Left, handler);
        }
        if let Some(handler) = self.on_context_menu {
            frame = frame.on_mouse_down(MouseButton::Right, handler);
        }
        frame
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use gpui::px;

    use super::*;

    fn cut(text: &str) -> String {
        text.replace("middle/", "")
    }

    #[test]
    fn a_repeated_path_at_the_same_width_is_not_fitted_again() {
        let mut cache = FolderCache::default();
        let calls = Cell::new(0);
        let fit = |cache: &mut FolderCache, width| {
            cache.get_or_fit(width, px(13.), "a/middle/b".into(), |text| {
                calls.set(calls.get() + 1);
                cut(text)
            })
        };
        assert_eq!(fit(&mut cache, px(100.)), "a/b");
        assert_eq!(fit(&mut cache, px(100.)), "a/b");
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn a_new_width_or_font_size_fits_again() {
        let mut cache = FolderCache::default();
        let calls = Cell::new(0);
        let mut fit = |width, size| {
            cache.get_or_fit(width, size, "a/middle/b".into(), |text| {
                calls.set(calls.get() + 1);
                cut(text)
            })
        };
        fit(px(100.), px(13.));
        fit(px(90.), px(13.));
        fit(px(90.), px(14.));
        fit(px(90.), px(14.));
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn a_path_that_fits_comes_back_as_the_same_shared_text() {
        let mut cache = FolderCache::default();
        let path = SharedString::from("a/b");
        let fitted = cache.get_or_fit(px(100.), px(13.), path.clone(), |text| text.to_owned());
        assert_eq!(fitted, path);
        assert!(std::ptr::eq(fitted.as_ref(), path.as_ref()));
    }

    #[test]
    fn the_cache_is_bounded() {
        let mut cache = FolderCache::default();
        for index in 0..FOLDER_CACHE_CAPACITY * 3 {
            let path = SharedString::from(format!("volume/{index}/leaf"));
            cache.get_or_fit(px(100.), px(13.), path, |text| text.to_owned());
        }
        assert!(cache.entries.len() <= FOLDER_CACHE_CAPACITY);
    }

    #[test]
    fn clear_forgets_everything() {
        let mut cache = FolderCache::default();
        cache.get_or_fit(px(100.), px(13.), "a/b".into(), |text| text.to_owned());
        cache.clear();
        assert!(cache.entries.is_empty());
    }
}
