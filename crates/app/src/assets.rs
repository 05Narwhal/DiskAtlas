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

//! Embedded assets: the icon set, compiled into the binary and served to GPUI by path.
//!
//! GPUI draws an SVG as a mask tinted with the element's text color, so the files use plain
//! black and every icon takes its color from the theme.
#![allow(dead_code)] // Tokens and icons are a fixed vocabulary; some have no user until a later milestone.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

/// Paths of the embedded icons, for `gpui::svg().path(..)`.
pub mod icon {
    /// Navigate back.
    pub const BACK: &str = "icons/back.svg";
    /// Navigate forward.
    pub const FORWARD: &str = "icons/forward.svg";
    /// Breadcrumb separator.
    pub const CHEVRON: &str = "icons/chevron.svg";
    /// List view toggle.
    pub const LIST_VIEW: &str = "icons/list_view.svg";
    /// Icon view toggle.
    pub const ICON_VIEW: &str = "icons/icon_view.svg";
    /// Window minimize.
    pub const MINIMIZE: &str = "icons/minimize.svg";
    /// Window maximize.
    pub const MAXIMIZE: &str = "icons/maximize.svg";
    /// Window restore.
    pub const RESTORE: &str = "icons/restore.svg";
    /// Window close.
    pub const CLOSE: &str = "icons/close.svg";
    /// Folder row icon.
    pub const FOLDER: &str = "icons/folder.svg";
    /// File row icon.
    pub const FILE: &str = "icons/file.svg";
}

macro_rules! embed {
    ($($name:literal),* $(,)?) => {
        &[$((
            concat!("icons/", $name, ".svg"),
            include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/icons/", $name, ".svg")) as &[u8],
        )),*]
    };
}

static ICONS: &[(&str, &[u8])] = embed![
    "back",
    "forward",
    "chevron",
    "list_view",
    "icon_view",
    "minimize",
    "maximize",
    "restore",
    "close",
    "folder",
    "file",
    "kind_application",
    "kind_archive",
    "kind_audio",
    "kind_code",
    "kind_disk_image",
    "kind_document",
    "kind_folder",
    "kind_font",
    "kind_model",
    "kind_movie",
    "kind_other",
    "kind_picture",
];

/// The asset source handed to `Application::with_assets`. Serves only what is compiled in.
pub struct Assets;

impl AssetSource for Assets {
    /// An unknown path is `Ok(None)`, not an error.
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .filter(|(name, _)| name.starts_with(path))
            .map(|(name, _)| SharedString::from(*name))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_constant_loads() {
        let all = [
            icon::BACK,
            icon::FORWARD,
            icon::CHEVRON,
            icon::LIST_VIEW,
            icon::ICON_VIEW,
            icon::MINIMIZE,
            icon::MAXIMIZE,
            icon::RESTORE,
            icon::CLOSE,
            icon::FOLDER,
            icon::FILE,
        ];
        let glyphs = crate::theme::Glyph::ALL.map(|glyph| glyph.path());
        assert_eq!(all.len() + glyphs.len(), ICONS.len());
        for path in all.into_iter().chain(glyphs) {
            let bytes = Assets.load(path).unwrap().unwrap_or_default();
            assert!(!bytes.is_empty(), "{path} is empty or missing");
            assert!(bytes.starts_with(b"<svg"), "{path} is not an SVG");
        }
    }

    #[test]
    fn unknown_path_is_none() {
        assert!(Assets.load("icons/nope.svg").unwrap().is_none());
        assert!(Assets.load("").unwrap().is_none());
    }

    #[test]
    fn list_returns_icons() {
        assert_eq!(Assets.list("icons/").unwrap().len(), ICONS.len());
        assert!(Assets.list("fonts/").unwrap().is_empty());
    }
}
