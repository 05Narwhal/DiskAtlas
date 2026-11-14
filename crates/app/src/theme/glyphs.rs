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

//! The twelve kind glyphs of the legend (`ui-design.md` 1.7): one 16 px stroke icon per kind, and
//! the map from the engine's kind class to a glyph.

use disk_atlas_lib::search::kind::KindClass;

/// One glyph of the legend of kinds. Each has an embedded SVG (see [`Glyph::path`]) and a tint in
/// `Colors` (see `Colors::tint`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Glyph {
    Folder,
    Document,
    Picture,
    Audio,
    Movie,
    Code,
    Archive,
    Application,
    Model3d,
    Font,
    DiskImage,
    Other,
}

impl Glyph {
    /// Every glyph, in the order of the token board's table.
    pub const ALL: [Glyph; 12] = [
        Glyph::Folder,
        Glyph::Document,
        Glyph::Picture,
        Glyph::Audio,
        Glyph::Movie,
        Glyph::Code,
        Glyph::Archive,
        Glyph::Application,
        Glyph::Model3d,
        Glyph::Font,
        Glyph::DiskImage,
        Glyph::Other,
    ];

    /// The glyph for a kind class.
    pub const fn from_class(class: KindClass) -> Self {
        match class {
            KindClass::Other => Glyph::Other,
            KindClass::Folder => Glyph::Folder,
            KindClass::Document => Glyph::Document,
            KindClass::Image => Glyph::Picture,
            KindClass::Audio => Glyph::Audio,
            KindClass::Video => Glyph::Movie,
            KindClass::Archive => Glyph::Archive,
            KindClass::Application => Glyph::Application,
            KindClass::Code => Glyph::Code,
            KindClass::DiskImage => Glyph::DiskImage,
            KindClass::Font => Glyph::Font,
            KindClass::Model => Glyph::Model3d,
        }
    }

    /// Path of the embedded SVG, for `gpui::svg().path(..)`.
    pub const fn path(self) -> &'static str {
        match self {
            Glyph::Folder => "icons/kind_folder.svg",
            Glyph::Document => "icons/kind_document.svg",
            Glyph::Picture => "icons/kind_picture.svg",
            Glyph::Audio => "icons/kind_audio.svg",
            Glyph::Movie => "icons/kind_movie.svg",
            Glyph::Code => "icons/kind_code.svg",
            Glyph::Archive => "icons/kind_archive.svg",
            Glyph::Application => "icons/kind_application.svg",
            Glyph::Model3d => "icons/kind_model.svg",
            Glyph::Font => "icons/kind_font.svg",
            Glyph::DiskImage => "icons/kind_disk_image.svg",
            Glyph::Other => "icons/kind_other.svg",
        }
    }

    /// The kind's name, as the legend shows it.
    pub const fn label(self) -> &'static str {
        match self {
            Glyph::Folder => "Folder",
            Glyph::Document => "Document",
            Glyph::Picture => "Picture",
            Glyph::Audio => "Audio",
            Glyph::Movie => "Movie",
            Glyph::Code => "Code and data",
            Glyph::Archive => "Archive",
            Glyph::Application => "Application",
            Glyph::Model3d => "3D model",
            Glyph::Font => "Font",
            Glyph::DiskImage => "Disk image",
            Glyph::Other => "Other",
        }
    }
}

impl From<KindClass> for Glyph {
    fn from(class: KindClass) -> Self {
        Glyph::from_class(class)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::Assets;
    use gpui::AssetSource;
    use std::collections::HashSet;

    #[test]
    fn every_kind_class_maps_to_its_own_glyph() {
        let used: HashSet<Glyph> = KindClass::ALL
            .iter()
            .map(|c| Glyph::from_class(*c))
            .collect();
        // Twelve classes, twelve glyphs.
        assert_eq!(used.len(), KindClass::ALL.len());
        assert!(used.contains(&Glyph::Model3d));
    }

    #[test]
    fn every_glyph_asset_exists_and_is_well_formed() {
        let mut paths = HashSet::new();
        for glyph in Glyph::ALL {
            assert!(paths.insert(glyph.path()), "{glyph:?} shares a path");
            let bytes = Assets
                .load(glyph.path())
                .unwrap()
                .unwrap_or_else(|| panic!("{} is not embedded", glyph.path()));
            let text = std::str::from_utf8(&bytes).expect("utf-8 svg");
            assert!(text.starts_with("<svg"), "{} is not an SVG", glyph.path());
            assert!(text.ends_with("</svg>"), "{} is not closed", glyph.path());
            assert!(text.contains(r#"viewBox="0 0 16 16""#));
            assert!(text.contains(r#"stroke-width="1.4""#));
            // Balanced quotes and angle brackets: a cheap well-formedness check.
            assert_eq!(text.matches('"').count() % 2, 0, "{}", glyph.path());
            assert_eq!(text.matches('<').count(), text.matches('>').count());
            // Every shape is a stroke; none is filled with a color.
            assert!(!text.contains("fill=\"#"), "{} has a fill", glyph.path());
        }
        assert_eq!(paths.len(), 12);
    }

    #[test]
    fn labels_are_distinct() {
        let labels: HashSet<_> = Glyph::ALL.iter().map(|g| g.label()).collect();
        assert_eq!(labels.len(), 12);
    }
}
