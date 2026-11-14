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

//! The color tokens of the interface, one palette for day and one for night.
//!
//! The values are the "Color" table in `.claude/docs/ui-design.md` (1.2). All text pairs meet
//! 4.5:1 contrast; keep it that way when adding a token (a test checks it).

use gpui::{Hsla, rgb};

use super::glyphs::Glyph;

/// Every color the interface may use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Colors {
    /// Behind floating windows in mockups only.
    pub desk: Hsla,
    /// Window material: top bar, sidebar.
    pub chrome: Hsla,
    /// Content panel.
    pub sheet: Hsla,
    /// Popovers, menus, the selected segment of a segmented control.
    pub raised: Hsla,
    /// Search field, segmented-control track.
    pub field: Hsla,
    /// Hover, current sidebar row.
    pub hover: Hsla,
    /// Preview wells.
    pub well: Hsla,
    /// Disk strip background, bar tracks.
    pub track: Hsla,
    /// Separators.
    pub line: Hsla,
    /// Text, filled button.
    pub ink: Hsla,
    /// Secondary text.
    pub ink2: Hsla,
    /// Labels, hints.
    pub ink3: Hsla,
    /// Selected row fill.
    pub mark: Hsla,
    /// Text on `mark`.
    pub on_mark: Hsla,
    /// Overprint lines and dots: file pieces, current volume, the selected region.
    pub pin: Hsla,
    /// Disk strip occupancy shading, drawn at 4 to 80 percent.
    pub relief: Hsla,
    /// Free space hatching, fill.
    pub water: Hsla,
    /// Free space hatching, line.
    pub water_line: Hsla,
    /// The sheet's one-pixel outline at elevation 1 (`line` by day, `#22303A` by night).
    pub sheet_edge: Hsla,
    /// Tint per kind glyph, indexed by `Glyph`; read through [`Colors::tint`].
    kind_tints: [Hsla; 12],
    /// Alpha of a kind's wash: 12 percent by day, 13 by night.
    wash_alpha: f32,
}

/// The hex values of one palette.
struct Palette {
    desk: u32,
    chrome: u32,
    sheet: u32,
    raised: u32,
    field: u32,
    hover: u32,
    well: u32,
    track: u32,
    line: u32,
    ink: u32,
    ink2: u32,
    ink3: u32,
    mark: u32,
    pin: u32,
    relief: u32,
    water: u32,
    water_line: u32,
    sheet_edge: u32,
    /// In `Glyph::ALL` order.
    kinds: [u32; 12],
    wash_alpha: f32,
}

const DAY: Palette = Palette {
    desk: 0xC9D2CF,
    chrome: 0xE4EAE8,
    sheet: 0xFAFBFA,
    raised: 0xFFFFFF,
    field: 0xD9E1DE,
    hover: 0xEBF0EE,
    well: 0xEEF2F0,
    track: 0xE8EDEB,
    line: 0xD5DDDA,
    ink: 0x15201D,
    ink2: 0x46534F,
    ink3: 0x5E6B67,
    mark: 0xB0166C,
    pin: 0xB0166C,
    relief: 0x2B3532,
    water: 0xDCEAEF,
    water_line: 0xA9C7D3,
    sheet_edge: 0xD5DDDA,
    // Folder, Document, Picture, Audio, Movie, Code, Archive, Application, 3D model, Font,
    // Disk image, Other.
    kinds: [
        0x4E7487, 0x2F6DA8, 0x2C7D5F, 0x7A5BB0, 0xB5582F, 0x8A6D12, 0x7D5F45, 0x4A55B8, 0x127C8A,
        0x6B6158, 0x556B2F, 0x66726F,
    ],
    wash_alpha: 0.12,
};

const NIGHT: Palette = Palette {
    desk: 0x0B1113,
    chrome: 0x121A1C,
    sheet: 0x172124,
    raised: 0x24323A,
    field: 0x1C272A,
    hover: 0x243236,
    well: 0x101719,
    track: 0x1F2B2F,
    line: 0x2D3C40,
    ink: 0xE4ECE9,
    ink2: 0xAEBBB7,
    ink3: 0x8D9C98,
    mark: 0xC8207F,
    pin: 0xF25CA2,
    relief: 0xC9D6D2,
    water: 0x183039,
    water_line: 0x26495A,
    sheet_edge: 0x22303A,
    kinds: [
        0x8EB4C6, 0x7FB0E3, 0x6FC3A0, 0xB79BE6, 0xEB9A72, 0xD9B65A, 0xC4A285, 0x9AA3F0, 0x5CC6D3,
        0xBFB3A6, 0xA8BF7A, 0xA7B2AF,
    ],
    wash_alpha: 0.13,
};

impl Colors {
    fn from_palette(p: &Palette) -> Self {
        let c = |hex: u32| -> Hsla { rgb(hex).into() };
        Self {
            desk: c(p.desk),
            chrome: c(p.chrome),
            sheet: c(p.sheet),
            raised: c(p.raised),
            field: c(p.field),
            hover: c(p.hover),
            well: c(p.well),
            track: c(p.track),
            line: c(p.line),
            ink: c(p.ink),
            ink2: c(p.ink2),
            ink3: c(p.ink3),
            mark: c(p.mark),
            on_mark: c(0xFFFFFF),
            pin: c(p.pin),
            relief: c(p.relief),
            water: c(p.water),
            water_line: c(p.water_line),
            sheet_edge: c(p.sheet_edge),
            kind_tints: p.kinds.map(c),
            wash_alpha: p.wash_alpha,
        }
    }

    /// The day palette.
    pub fn day() -> Self {
        Self::from_palette(&DAY)
    }

    /// The night palette.
    pub fn night() -> Self {
        Self::from_palette(&NIGHT)
    }

    /// The tint of a kind: its glyph color, also fine as text on `sheet`.
    pub fn tint(&self, glyph: Glyph) -> Hsla {
        self.kind_tints[glyph as usize]
    }

    /// The wash of a kind (icon tile, space-map fill): its tint at 12 percent (day) or 13
    /// percent (night) alpha.
    pub fn wash(&self, glyph: Glyph) -> Hsla {
        self.tint(glyph).alpha(self.wash_alpha)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Rgba;

    fn luminance(hsla: Hsla) -> f32 {
        let c = Rgba::from(hsla);
        let lin = |v: f32| {
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
    }

    /// WCAG contrast ratio of two opaque colors.
    fn contrast(a: Hsla, b: Hsla) -> f32 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    #[test]
    fn palettes_differ_and_share_only_on_mark() {
        let (day, night) = (Colors::day(), Colors::night());
        assert_ne!(day.chrome, night.chrome);
        assert_ne!(day.mark, night.mark);
        assert_eq!(day.on_mark, night.on_mark);
    }

    #[test]
    fn contrast_matches_the_documented_figures() {
        let (day, night) = (Colors::day(), Colors::night());
        let near = |got: f32, want: f32| assert!((got - want).abs() < 0.06, "{got} vs {want}");
        near(contrast(day.ink, day.sheet), 16.1);
        near(contrast(night.ink, night.sheet), 13.7);
        near(contrast(day.ink2, day.sheet), 7.8);
        near(contrast(night.ink2, night.sheet), 8.3);
        near(contrast(day.ink3, day.sheet), 5.4);
        near(contrast(night.ink3, night.sheet), 5.7);
        near(contrast(day.ink3, day.chrome), 4.6);
        near(contrast(night.ink3, night.hover), 4.6);
        near(contrast(day.on_mark, day.mark), 6.6);
        near(contrast(night.on_mark, night.mark), 5.3);
        near(contrast(night.pin, night.sheet), 5.3);
    }

    #[test]
    fn every_text_pair_meets_4_5_to_1() {
        for (name, c) in [("day", Colors::day()), ("night", Colors::night())] {
            let mut pairs = vec![("on_mark/mark", c.on_mark, c.mark)];
            for (label, ink) in [("ink", c.ink), ("ink2", c.ink2), ("ink3", c.ink3)] {
                for ground in [c.sheet, c.chrome, c.hover] {
                    pairs.push((label, ink, ground));
                }
            }
            pairs.push(("pin/sheet", c.pin, c.sheet));
            for glyph in Glyph::ALL {
                pairs.push((glyph.label(), c.tint(glyph), c.sheet));
            }
            for (label, fg, bg) in pairs {
                let ratio = contrast(fg, bg);
                assert!(ratio >= 4.5, "{name}: {label} is {ratio:.2}:1");
            }
        }
    }

    #[test]
    fn wash_is_the_tint_at_a_low_alpha() {
        let (day, night) = (Colors::day(), Colors::night());
        assert!((day.wash(Glyph::Folder).a - 0.12).abs() < 1e-6);
        assert!((night.wash(Glyph::Folder).a - 0.13).abs() < 1e-6);
        assert_eq!(day.wash(Glyph::Font).h, day.tint(Glyph::Font).h);
    }
}
