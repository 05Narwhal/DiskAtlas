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

//! Packages: folders the Finder shows as one item. Source: `r10-everyday.md`
//! section 5.4. Pure functions of a name, a kind and flags.

use crate::fs::{EntryFlags, NodeKind};

/// Extensions of folders that are packages, with what to call them, in the
/// order of the design. Matched without regard to case.
const PACKAGES: &[(&str, &str)] = &[
    ("app", "Application"),
    ("bundle", "Bundle"),
    ("plugin", "Plug-in"),
    ("kext", "Kernel extension"),
    ("prefpane", "Preference pane"),
    ("qlgenerator", "Quick Look plug-in"),
    ("mdimporter", "Spotlight importer"),
    ("saver", "Screen saver"),
    ("wdgt", "Widget"),
    ("component", "Component"),
    ("action", "Automator action"),
    ("workflow", "Automator workflow"),
    ("pkg", "Installer package"),
    ("mpkg", "Installer package"),
    ("rtfd", "Rich text document"),
    ("photoslibrary", "Photos library"),
    ("xcodeproj", "Xcode project"),
    ("playground", "Playground"),
    ("dsym", "Debug symbols"),
    ("sparsebundle", "Sparse bundle"),
    ("scptd", "Script bundle"),
    ("download", "Unfinished download"),
];

/// The lower-cased extension of `name` when it is short enough to be one of
/// ours; the listed ones are at most 13 bytes.
fn extension_matches(name: &str) -> Option<&'static str> {
    let (_, ext) = name.rsplit_once('.')?;
    PACKAGES
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(ext))
        .map(|&(_, kind)| kind)
}

/// True when the entry is a package: a folder with the Finder's bundle bit
/// ([`EntryFlags::PACKAGE`]), or one whose extension is a package's. A file
/// is never one, whatever its name.
pub fn is_package(name: &str, kind: NodeKind, flags: EntryFlags) -> bool {
    kind == NodeKind::Directory && (flags.contains(EntryFlags::PACKAGE) || extension_matches(name).is_some())
}

/// What to call the package `name` in the Kind column: "Application" for
/// `.app`, "Package" when only the bundle bit says so.
pub fn package_kind(name: &str) -> &'static str {
    extension_matches(name).unwrap_or("Package")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_extension_and_the_bundle_bit_make_a_package() {
        for (ext, kind) in PACKAGES {
            let name = format!("Thing.{ext}");
            assert!(is_package(&name, NodeKind::Directory, EntryFlags::EMPTY), "{name}");
            assert_eq!(package_kind(&name), *kind);
        }
        assert!(is_package("Thing.APP", NodeKind::Directory, EntryFlags::EMPTY));
        assert!(is_package("Plain", NodeKind::Directory, EntryFlags::PACKAGE));
        assert_eq!(package_kind("Plain"), "Package");
    }

    #[test]
    fn files_and_other_folders_are_not_packages() {
        assert!(!is_package("x.app", NodeKind::File, EntryFlags::EMPTY));
        // On a classic file the bundle bit means a BNDL resource.
        assert!(!is_package("Tool", NodeKind::File, EntryFlags::PACKAGE));
        assert!(!is_package("Documents", NodeKind::Directory, EntryFlags::EMPTY));
        assert!(!is_package("app", NodeKind::Directory, EntryFlags::EMPTY));
        assert!(!is_package("x.apps", NodeKind::Directory, EntryFlags::EMPTY));
    }
}
