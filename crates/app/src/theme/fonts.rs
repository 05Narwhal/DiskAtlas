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

//! The bundled interface typeface: Atkinson Hyperlegible Next, embedded in the binary.

use std::{borrow::Cow, sync::Arc};

use gpui::{App, Font, FontFeatures};

/// The family name of the monospaced face of the bytes view, as the font files declare it.
pub const MONO_FONT: &str = "Atkinson Hyperlegible Mono";

/// The family name of the interface typeface, as the font files declare it.
pub const UI_FONT: &str = "Atkinson Hyperlegible Next";

static REGULAR: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/fonts/AtkinsonHyperlegibleNext-Regular.ttf"
));
static ITALIC: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/fonts/AtkinsonHyperlegibleNext-Italic.ttf"
));
static BOLD: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/fonts/AtkinsonHyperlegibleNext-Bold.ttf"
));
static BOLD_ITALIC: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/fonts/AtkinsonHyperlegibleNext-BoldItalic.ttf"
));

static MONO_REGULAR: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/fonts/AtkinsonHyperlegibleMono-Regular.ttf"
));
static MONO_BOLD: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/fonts/AtkinsonHyperlegibleMono-Bold.ttf"
));

/// The monospaced face, for `Styled::font`: every character the same width, so columns line up.
pub fn mono_font() -> Font {
    gpui::font(MONO_FONT)
}

/// Tabular figures, so sizes and dates line up in columns.
pub fn ui_font_features() -> FontFeatures {
    FontFeatures(Arc::new(vec![("tnum".to_string(), 1)]))
}

/// The interface font with tabular figures on, for `Styled::font`.
///
/// GPUI's `Styled` has no setter for font features alone; `.font(ui_font())` sets the family and
/// the features together (and resets weight and style to regular, so set those after it).
pub fn ui_font() -> Font {
    let mut font = gpui::font(UI_FONT);
    font.features = ui_font_features();
    font
}

/// Registers the four bundled font files with the text system.
pub fn load(cx: &mut App) -> anyhow::Result<()> {
    let text_system = cx.text_system();
    text_system.add_fonts(vec![
        Cow::Borrowed(REGULAR),
        Cow::Borrowed(ITALIC),
        Cow::Borrowed(BOLD),
        Cow::Borrowed(BOLD_ITALIC),
        Cow::Borrowed(MONO_REGULAR),
        Cow::Borrowed(MONO_BOLD),
    ])?;

    let names = text_system.all_font_names();
    for wanted in [UI_FONT, MONO_FONT] {
        if !names.iter().any(|name| name == wanted) {
            tracing::warn!(
                wanted,
                found = ?names,
                "bundled typeface family is not known to the text system"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn features_enable_tabular_figures() {
        assert_eq!(
            ui_font_features().tag_value_list(),
            &[("tnum".to_string(), 1)]
        );
    }

    #[test]
    fn font_files_are_embedded() {
        for bytes in [REGULAR, ITALIC, BOLD, BOLD_ITALIC, MONO_REGULAR, MONO_BOLD] {
            // TrueType files start with the version tag 0x00010000.
            assert_eq!(&bytes[..4], &[0, 1, 0, 0]);
        }
    }
}
