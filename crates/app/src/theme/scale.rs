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

//! The redesign's type scale, spacing, radius, size and density tokens (`ui-design.md` 1.3, 1.4)
//! and elevation shadows (1.5).

use gpui::{BoxShadow, FontWeight, Pixels, hsla, point, px};

use super::{Appearance, Colors};

/// Spacing scale: 2, 4, 8, 12, 16, 24, 32, 48.
pub mod space {
    use gpui::{Pixels, px};

    pub const S2: Pixels = px(2.);
    pub const S4: Pixels = px(4.);
    pub const S8: Pixels = px(8.);
    pub const S12: Pixels = px(12.);
    pub const S16: Pixels = px(16.);
    pub const S24: Pixels = px(24.);
    pub const S32: Pixels = px(32.);
    pub const S48: Pixels = px(48.);
}

/// Corner radii.
pub mod radius {
    use gpui::{Pixels, px};

    /// Small marks.
    pub const MARK: Pixels = px(4.);
    /// Rows and controls.
    pub const ROW: Pixels = px(6.);
    /// Fields and buttons.
    pub const FIELD: Pixels = px(8.);
    /// Wells and popovers.
    pub const WELL: Pixels = px(10.);
    /// The sheet's corner and the Quick Look content area.
    pub const SHEET: Pixels = px(12.);
}

/// Fixed sizes.
pub mod size {
    use gpui::{Pixels, px};

    /// Top bar height.
    pub const TOP_BAR: Pixels = px(48.);
    /// Sidebar width (collapsible).
    pub const SIDEBAR: Pixels = px(240.);
    /// Details pane width (collapsible).
    pub const DETAILS: Pixels = px(300.);
    /// Control height.
    pub const CONTROL: Pixels = px(32.);
    /// Widest a tooltip grows before its text wraps.
    pub const TOOLTIP_MAX_WIDTH: Pixels = px(320.);
    /// Control height inside a segmented control.
    pub const CONTROL_SEGMENT: Pixels = px(28.);
}

/// The density setting: it changes row heights and icon sizes only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Density {
    Compact,
    #[default]
    Regular,
    Roomy,
}

impl Density {
    /// Every step, smallest first.
    pub const ALL: [Density; 3] = [Density::Compact, Density::Regular, Density::Roomy];

    /// Height of a list row.
    pub const fn row_height(self) -> Pixels {
        match self {
            Density::Compact => px(24.),
            Density::Regular => px(30.),
            Density::Roomy => px(40.),
        }
    }

    /// Height of a sidebar row.
    pub const fn sidebar_row_height(self) -> Pixels {
        match self {
            Density::Compact => px(24.),
            Density::Regular => px(28.),
            Density::Roomy => px(32.),
        }
    }

    /// Side of the icon, icon tile or thumbnail in a row.
    pub const fn icon_size(self) -> Pixels {
        match self {
            Density::Compact => px(16.),
            Density::Regular => px(24.),
            Density::Roomy => px(32.),
        }
    }
}

/// One role of the type scale. Atkinson Hyperlegible Next, tabular figures where numbers align.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextStyle {
    pub size: Pixels,
    pub weight: FontWeight,
    /// Letter spacing in em (0.06 means +0.06 em).
    pub tracking_em: f32,
}

const fn style(size: f32, weight: f32, tracking_em: f32) -> TextStyle {
    TextStyle {
        size: px(size),
        weight: FontWeight(weight),
        tracking_em,
    }
}

/// The type scale. No all-caps labels anywhere.
pub mod text {
    use super::{TextStyle, style};

    /// Disk view title, empty-state heads: 22 / 700, -0.01 em.
    pub const DISPLAY: TextStyle = style(22., 700., -0.01);
    /// Volume title above the list: 20 / 700.
    pub const TITLE: TextStyle = style(20., 700., 0.);
    /// Quick Look file name, details-pane name, section heads: 15 / 700.
    pub const HEADING: TextStyle = style(15., 700., 0.);
    /// Rows, buttons, most text: 13 / 400.
    pub const BODY: TextStyle = style(13., 400., 0.);
    /// Secondary columns, captions: 12 / 400.
    pub const SMALL: TextStyle = style(12., 400., 0.);
    /// Sidebar group labels: 12 / 700.
    pub const SMALL_BOLD: TextStyle = style(12., 700., 0.);
    /// Space-map region names: 11 / 700, +0.06 em. Water names are set in italic.
    pub const MAP_LABEL: TextStyle = style(11., 700., 0.06);
    /// Bytes view (Atkinson Hyperlegible Mono): 12 / 400.
    pub const MONO: TextStyle = style(12., 400., 0.);
}

/// Shadows for the elevation levels of 1.5, ready for `Styled::shadow`. Level 0 is `chrome` and
/// level 3 is the operating system's, so neither has shadows here.
pub mod elevation {
    use super::*;

    fn ring(color: gpui::Hsla) -> BoxShadow {
        BoxShadow {
            color,
            offset: point(px(0.), px(0.)),
            blur_radius: px(0.),
            spread_radius: px(1.),
        }
    }

    /// Level 1, the sheet: a one-pixel outline, and by day a faint drop shadow.
    pub fn sheet(appearance: Appearance, colors: &Colors) -> Vec<BoxShadow> {
        let mut shadows = vec![ring(colors.sheet_edge)];
        if appearance == Appearance::Day {
            shadows.push(BoxShadow {
                // rgba(20, 32, 29, .06)
                color: hsla(165. / 360., 0.23, 0.1, 0.06),
                offset: point(px(0.), px(1.)),
                blur_radius: px(2.),
                spread_radius: px(0.),
            });
        }
        shadows
    }

    /// Level 2, popovers, menus, filter menus and toasts: the `line` outline and a soft drop.
    pub fn popover(appearance: Appearance, colors: &Colors) -> Vec<BoxShadow> {
        let drop = match appearance {
            // rgba(20, 32, 29, .14)
            Appearance::Day => hsla(165. / 360., 0.23, 0.1, 0.14),
            // rgba(0, 0, 0, .35)
            Appearance::Night => hsla(0., 0., 0., 0.35),
        };
        vec![
            ring(colors.line),
            BoxShadow {
                color: drop,
                offset: point(px(0.), px(8.)),
                blur_radius: px(24.),
                spread_radius: px(0.),
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn density_steps_match_the_doc() {
        assert_eq!(Density::default(), Density::Regular);
        let rows = Density::ALL.map(|d| f32::from(d.row_height()));
        assert_eq!(rows, [24., 30., 40.]);
        let sidebar = Density::ALL.map(|d| f32::from(d.sidebar_row_height()));
        assert_eq!(sidebar, [24., 28., 32.]);
        let icons = Density::ALL.map(|d| f32::from(d.icon_size()));
        assert_eq!(icons, [16., 24., 32.]);
    }

    #[test]
    fn spacing_is_the_documented_scale() {
        let scale = [
            space::S2,
            space::S4,
            space::S8,
            space::S12,
            space::S16,
            space::S24,
            space::S32,
            space::S48,
        ]
        .map(f32::from);
        assert_eq!(scale, [2., 4., 8., 12., 16., 24., 32., 48.]);
    }

    #[test]
    fn sizes_match_the_doc() {
        assert_eq!(f32::from(size::TOP_BAR), 48.);
        assert_eq!(f32::from(size::SIDEBAR), 240.);
        assert_eq!(f32::from(size::DETAILS), 300.);
        assert_eq!(f32::from(size::CONTROL), 32.);
        assert_eq!(f32::from(size::CONTROL_SEGMENT), 28.);
    }

    #[test]
    fn elevation_levels_have_the_documented_layers() {
        let (day, night) = (Colors::day(), Colors::night());
        assert_eq!(elevation::sheet(Appearance::Day, &day).len(), 2);
        assert_eq!(elevation::sheet(Appearance::Night, &night).len(), 1);
        assert_eq!(elevation::popover(Appearance::Day, &day).len(), 2);
        assert_eq!(elevation::popover(Appearance::Night, &night).len(), 2);
    }
}
