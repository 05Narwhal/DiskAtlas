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

//! Design tokens: colors for day and night, the interface typeface, sizes.
//!
//! The redesign tokens (`ui-design.md` Tokens and Motion) are `colors`, `scale`, `motion` and
//! `glyphs`; `metrics` keeps the sizes the redesign has no token for (columns, icons, the disk
//! strip, Quick Look).
//!
//! Components read everything from here; no color, size or font literal belongs anywhere else.
#![allow(dead_code)] // Tokens and icons are a fixed vocabulary; some have no user until a later milestone.

pub mod colors;
pub mod fonts;
pub mod glyphs;
pub mod motion;
pub mod scale;

use gpui::{App, Global, WindowAppearance};

pub use colors::Colors;
pub use glyphs::Glyph;

/// Which of the two palettes is in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appearance {
    /// Light palette.
    Day,
    /// Dark palette.
    Night,
}

impl From<WindowAppearance> for Appearance {
    fn from(appearance: WindowAppearance) -> Self {
        match appearance {
            WindowAppearance::Dark | WindowAppearance::VibrantDark => Self::Night,
            _ => Self::Day,
        }
    }
}

/// The active appearance and its palette, stored as a GPUI global.
pub struct Theme {
    /// Day or night.
    pub appearance: Appearance,
    /// The palette that matches `appearance`.
    pub colors: Colors,
}

impl Global for Theme {}

impl Theme {
    fn new(appearance: Appearance) -> Self {
        let colors = match appearance {
            Appearance::Day => Colors::day(),
            Appearance::Night => Colors::night(),
        };
        Self { appearance, colors }
    }

    /// The current theme. Panics only if `theme::init` was never called.
    pub fn get(cx: &App) -> &Theme {
        cx.global::<Theme>()
    }

    /// Switches day/night and redraws every window. No-op when unchanged.
    pub fn set_appearance(appearance: Appearance, cx: &mut App) {
        if cx
            .try_global::<Theme>()
            .is_some_and(|theme| theme.appearance == appearance)
        {
            return;
        }
        cx.set_global(Theme::new(appearance));
        cx.refresh_windows();
    }
}

/// The palette to use: the `theme` setting (`system`, `day`, `night`) over the system's own.
pub fn resolve(setting: &str, system: Appearance) -> Appearance {
    match setting {
        "day" => Appearance::Day,
        "night" => Appearance::Night,
        _ => system,
    }
}

/// Applies the `theme` setting (when the settings are loaded) over `system`.
pub fn apply_setting(system: Appearance, cx: &mut App) {
    let setting = cx
        .try_global::<crate::state::settings::Settings>()
        .map_or("system", |settings| settings.choice_of("theme"));
    Theme::set_appearance(resolve(setting, system), cx);
}

/// Loads the bundled fonts and installs the theme matching the system appearance. Call once at
/// startup.
pub fn init(cx: &mut App) -> anyhow::Result<()> {
    // Install the theme first so a font failure still leaves a working palette.
    let system: Appearance = cx.window_appearance().into();
    let setting = cx
        .try_global::<crate::state::settings::Settings>()
        .map_or("system", |settings| settings.choice_of("theme"));
    cx.set_global(Theme::new(resolve(setting, system)));
    fonts::load(cx)
}

/// Sizes from ui-design.md "Metrics" and "Type".
pub mod metrics {
    use gpui::{Pixels, px};

    /// Horizontal inset of list rows and the column header from the list's edges.
    pub const ROW_INSET: Pixels = px(6.);
    /// Horizontal padding inside a list row, a sidebar row and the column header.
    pub const ROW_PADDING: Pixels = px(10.);
    /// Gap between columns of the file list.
    pub const COLUMN_GAP: Pixels = px(14.);
    /// Smallest width of the Name column; it takes the space the others leave.
    pub const COLUMN_NAME_MIN: Pixels = px(220.);
    /// Smallest width of the Folder column in search results; it shares the free space with the
    /// Name column.
    pub const COLUMN_FOLDER_MIN: Pixels = px(160.);
    /// Width of the Modified column.
    pub const COLUMN_MODIFIED: Pixels = px(128.);
    /// Width of the Size column: wide enough for "over 999.9 GB" and "1023.9 GB…" (folder sizes
    /// of the host, R14.3).
    pub const COLUMN_SIZE: Pixels = px(96.);
    /// Width of the Resource fork column.
    pub const COLUMN_RESOURCE_FORK: Pixels = px(84.);
    /// Width of the Kind column.
    pub const COLUMN_KIND: Pixels = px(180.);
    /// Folder and file icons in the list.
    pub const ICON: Pixels = px(16.);
    /// Glyphs in top bar buttons.
    pub const ICON_SMALL: Pixels = px(14.);
    /// Glyphs in the window's caption buttons.
    pub const ICON_CAPTION: Pixels = px(12.);
    /// Gap between a row's icon and its name.
    pub const ICON_GAP: Pixels = px(9.);
    /// Width of the breadcrumb separator.
    pub const CHEVRON_WIDTH: Pixels = px(6.);
    /// Height of the breadcrumb separator.
    pub const CHEVRON_HEIGHT: Pixels = px(10.);
    /// Width of a window caption button (minimize, maximize, close).
    pub const CAPTION_BUTTON_WIDTH: Pixels = px(42.);
    /// Height of the strip along the top edge that resizes the window on Windows. Matches the
    /// band GPUI hit-tests as the top resize edge (`SM_CYFRAME` at 100% scale).
    pub const WINDOW_RESIZE_BAND: Pixels = px(4.);
    /// Space kept free at the left of the top bar for the macOS window buttons.
    pub const TRAFFIC_LIGHT_SPACE: Pixels = px(72.);
    /// Diameter of the dot before a volume in the sidebar.
    pub const DOT: Pixels = px(6.);
    /// Height of the bar of partitions under a disk in the sidebar.
    pub const DISK_BAR_HEIGHT: Pixels = px(4.);
    /// Side of the kind tile in the details pane's preview well.
    pub const DETAILS_KIND_TILE: Pixels = px(64.);
    /// Side of the glyph on that tile (the 16 px glyph, drawn at twice its size).
    pub const DETAILS_KIND_ICON: Pixels = px(32.);
    /// Height of the preview well in the details pane.
    pub const PREVIEW_WELL_HEIGHT: Pixels = px(132.);
    /// Width of the label column in the details pane's list of facts.
    pub const DETAILS_LABEL_WIDTH: Pixels = px(84.);
    /// Width of the Details tab's label column.
    pub const QUICK_LOOK_LABEL_WIDTH: Pixels = px(168.);
    /// Width of the Quick Look window when it first opens.
    pub const QUICK_LOOK_WINDOW_WIDTH: Pixels = px(840.);
    /// Height of the Quick Look window when it first opens.
    pub const QUICK_LOOK_WINDOW_HEIGHT: Pixels = px(560.);
    /// Smallest width of the Quick Look window.
    pub const QUICK_LOOK_MIN_WIDTH: Pixels = px(480.);
    /// Smallest height of the Quick Look window.
    pub const QUICK_LOOK_MIN_HEIGHT: Pixels = px(360.);
    /// Height of the title bar of an auxiliary window (`views/aux_window.rs`).
    pub const AUX_BAR_HEIGHT: Pixels = px(32.);
    /// Where macOS places the traffic lights inside that bar.
    pub const AUX_TRAFFIC_LIGHT_X: Pixels = px(14.);
    pub const AUX_TRAFFIC_LIGHT_Y: Pixels = px(10.);
    /// Height of one line of text in the Quick Look preview (13 px type).
    pub const QUICK_LOOK_LINE_HEIGHT: Pixels = px(18.);
    /// Height of one row of the Quick Look Details tab.
    pub const QUICK_LOOK_ROW_HEIGHT: Pixels = px(26.);
    /// Height of a Quick Look tab button.
    pub const QUICK_LOOK_TAB_HEIGHT: Pixels = px(28.);
    /// Height of the Quick Look header (`r1-redesign.md` 5.2).
    pub const QUICK_LOOK_HEADER_HEIGHT: Pixels = px(56.);
    /// Side of the kind tile in the Quick Look header.
    pub const QUICK_LOOK_KIND_TILE: Pixels = px(32.);
    /// Side of the file icon drawn on the kind tile.
    pub const QUICK_LOOK_KIND_ICON: Pixels = px(18.);
    /// Height of a segment of the Quick Look tabs.
    pub const QUICK_LOOK_SEGMENT_HEIGHT: Pixels = px(30.);
    /// Side padding of a segment of the Quick Look tabs.
    pub const QUICK_LOOK_SEGMENT_PADDING: Pixels = px(16.);
    /// Space between the segmented control's edge and its segments.
    pub const QUICK_LOOK_SEGMENT_INSET: Pixels = px(2.);
    /// Height of a button of the Data | Resource switch in the header's meta line.
    pub const QUICK_LOOK_FORK_HEIGHT: Pixels = px(20.);
    /// Width of a Quick Look toolbar button.
    pub const QUICK_LOOK_TOOL_WIDTH: Pixels = px(34.);
    /// Margin between the content well and the panel's edges.
    pub const QUICK_LOOK_WELL_MARGIN: Pixels = px(16.);
    /// Inner margin around a picture in the Quick Look well.
    pub const QUICK_LOOK_IMAGE_INSET: Pixels = px(24.);
    /// A picture is never drawn larger than this multiple of its pixel size.
    pub const QUICK_LOOK_IMAGE_MAX_SCALE: f32 = 2.0;
    /// Lines of text the details pane's preview well shows.
    pub const PEEK_LINES: usize = 12;
    /// Height of one line of text in the details pane's preview well (12 px type).
    pub const PEEK_LINE_HEIGHT: Pixels = px(15.);
    /// Height of the details pane's preview well when it shows text: `PEEK_LINES` lines and
    /// the well's padding.
    pub const PEEK_TEXT_WELL_HEIGHT: Pixels = px(192.);
    /// Thickness of separators and outlines.
    pub const HAIRLINE: Pixels = px(1.);
    /// Width of the search field in the top bar.
    pub const SEARCH_FIELD_WIDTH: Pixels = px(240.);
    /// Horizontal padding inside a filter control.
    pub const FILTER_CONTROL_PADDING: Pixels = px(10.);
    /// Smallest width of a menu.
    pub const MENU_MIN_WIDTH: Pixels = px(220.);
    /// Largest width of a menu.
    pub const MENU_MAX_WIDTH: Pixels = px(360.);
    /// How far a menu rises while it fades in.
    pub const MENU_RISE: Pixels = px(4.);
    /// How far from the window's edge a menu stays.
    pub const MENU_WINDOW_MARGIN: Pixels = px(8.);
    /// Height of one row of a menu.
    pub const MENU_ITEM_HEIGHT: Pixels = px(28.);
    /// Width of the column that holds a menu row's check mark.
    pub const MENU_CHECK_WIDTH: Pixels = px(16.);
    /// Width of a text caret.
    pub const CARET_WIDTH: Pixels = px(1.);

    /// Space under the disk strip's bar labels, before the separator.
    pub const STRIP_PADDING_BOTTOM: Pixels = px(12.);
    /// Gap between the disk name and its size in the disk strip's header.
    pub const STRIP_HEADER_GAP: Pixels = px(10.);
    /// Height of the disk strip's bar.
    pub const STRIP_BAR_HEIGHT: Pixels = px(16.);
    /// Gap between neighbouring segments of the disk strip.
    pub const STRIP_SEGMENT_GAP: Pixels = px(2.);
    /// Radius of the disk strip's outer corners.
    pub const STRIP_RADIUS: Pixels = px(2.);
    /// Narrowest a disk strip segment is drawn, so a tiny partition stays visible and clickable.
    pub const STRIP_SEGMENT_MIN_WIDTH: Pixels = px(4.);
    /// Opacity of a volume's relief fill while its fullness is not known (within the 6 to 100
    /// percent range of `relief`).
    pub const STRIP_FILL_OPACITY: f32 = 0.35;
    /// Extra opacity factor for the volumes of the strip that are not the current one.
    pub const STRIP_DIM_OPACITY: f32 = 0.45;
    /// Narrowest an accent mark for the selected file's extents is drawn on the strip, so a 4 KiB
    /// file on a 2 TB disk is still visible.
    pub const STRIP_MARK_MIN_WIDTH: Pixels = px(2.);
    /// Opacity of an empty region of a volume in the strip's shading: the low end of the 6 to 100
    /// percent range of `relief`.
    pub const STRIP_SHADE_FLOOR: f32 = 0.06;
    /// Number of opacity steps in the strip's shading. Fewer steps merge more neighbouring regions
    /// into one run.
    pub const STRIP_SHADE_LEVELS: u8 = 16;
    /// Slices a volume's allocation map is asked for; each becomes at most one shaded run.
    pub const STRIP_MAP_BUCKETS: usize = 512;
    /// Extents of the selected file closer together than this share of the volume (1 in this
    /// many) are drawn as one mark, which bounds the marks however fragmented the file is.
    pub const STRIP_MARK_MERGE_DENOMINATOR: u64 = 4096;

    /// Height of the storage bar (`r10-everyday.md` 4).
    pub const STORAGE_BAR_HEIGHT: Pixels = px(20.);
    /// Radius of a partition of the storage bar.
    pub const STORAGE_BAR_RADIUS: Pixels = px(6.);
    /// Height of the selection bracket under the bar.
    pub const STORAGE_BRACKET_HEIGHT: Pixels = px(4.);
    /// Height of the two label lines under a partition.
    pub const STORAGE_LABEL_HEIGHT: Pixels = px(34.);
    /// Opacity of a housekeeping item that is shown in the list (`r10-everyday.md` 5.1).
    pub const SYSTEM_ITEM_OPACITY: f32 = 0.55;
    /// Side of a legend chip's tint square.
    pub const STORAGE_CHIP_SWATCH: Pixels = px(8.);
    /// Opacity of the other segments while one is hovered.
    pub const STORAGE_DIM_OPACITY: f32 = 0.4;
    /// Opacity of the kinds of a partition that is not the current one.
    pub const STORAGE_OTHER_OPACITY: f32 = 0.55;
    /// Opacity of `relief` for the system segment.
    pub const STORAGE_SYSTEM_OPACITY: f32 = 0.35;
    /// Opacity of `relief` for used space when the kinds are not known.
    pub const STORAGE_USED_OPACITY: f32 = 0.30;
    /// Stroke and gap of the hatching of free and unreadable space, in pixels.
    pub const STORAGE_HATCH: (f32, f32) = (1.0, 6.0);
    /// Share of the bar the sweep band covers while measuring.
    pub const STORAGE_SWEEP_BAND: f32 = 0.25;
    /// Opacity of `relief` in the sweep band.
    pub const STORAGE_SWEEP_OPACITY: f32 = 0.18;
    /// The legend holds at most this many lines.
    pub const STORAGE_LEGEND_LINES: usize = 2;
    /// Average width of a character as a share of the font size, to guess how many chips fit.
    pub const STORAGE_CHAR_WIDTH: f32 = 0.56;
    /// The Properties window opens this wide (`r10-everyday.md` section 7)...
    pub const PROPERTIES_WIDTH: Pixels = px(420.);
    /// ... and this tall,
    pub const PROPERTIES_HEIGHT: Pixels = px(560.);
    /// and cannot be made narrower than this
    pub const PROPERTIES_MIN_WIDTH: Pixels = px(360.);
    /// or shorter than this.
    pub const PROPERTIES_MIN_HEIGHT: Pixels = px(400.);
    /// Height of the header: the kind tile, the name and the line under it.
    pub const PROPERTIES_HEADER_HEIGHT: Pixels = px(96.);
    /// Width of the label column of a Properties row.
    pub const PROPERTIES_LABEL_WIDTH: Pixels = px(120.);
    /// Height of the strip that shows a file's pieces, and of a volume's used-space bar.
    pub const PROPERTIES_STRIP_HEIGHT: Pixels = px(12.);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_theme_setting_overrides_the_system() {
        assert_eq!(resolve("day", Appearance::Night), Appearance::Day);
        assert_eq!(resolve("night", Appearance::Day), Appearance::Night);
        assert_eq!(resolve("system", Appearance::Night), Appearance::Night);
        assert_eq!(resolve("junk", Appearance::Day), Appearance::Day);
    }

    #[test]
    fn window_appearance_maps_to_theme_appearance() {
        assert_eq!(Appearance::from(WindowAppearance::Dark), Appearance::Night);
        assert_eq!(
            Appearance::from(WindowAppearance::VibrantDark),
            Appearance::Night
        );
        assert_eq!(Appearance::from(WindowAppearance::Light), Appearance::Day);
        assert_eq!(
            Appearance::from(WindowAppearance::VibrantLight),
            Appearance::Day
        );
    }
}
