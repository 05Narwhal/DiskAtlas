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

//! Hidden items and housekeeping items (`r10-everyday.md` 5.1): the one rule every list uses.
//!
//! Two independent switches. `show_hidden` governs what the volume itself marks hidden (a
//! leading dot, the Finder's invisible flag, the Windows hidden attribute). `show_system_items`
//! governs the housekeeping items of the engine's table (`.DS_Store`, `._` files, `Thumbs.db`
//! and the rest), on every filesystem. A housekeeping item follows `show_system_items` alone,
//! whether or not its volume also marks it hidden. Sizes are not affected: hidden and
//! housekeeping bytes are always counted.

use disk_atlas_lib::fs::{DirEntry, EntryFlags};
use disk_atlas_lib::mac::{SystemItem, system_item};

use super::settings::Settings;

/// The two switches, as the user set them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Visibility {
    /// `show_hidden`: items the volume marks hidden.
    pub show_hidden: bool,
    /// `show_system_items`: housekeeping items.
    pub show_system: bool,
}

impl Visibility {
    /// Both switches on.
    pub const EVERYTHING: Self = Self {
        show_hidden: true,
        show_system: true,
    };

    /// The switches in `settings`.
    pub fn of(settings: &Settings) -> Self {
        Self {
            show_hidden: settings.bool_of("show_hidden"),
            show_system: settings.bool_of("show_system_items"),
        }
    }

    /// The switches in the app's settings; both off where there are none (tests).
    pub fn current(cx: &gpui::App) -> Self {
        cx.try_global::<Settings>()
            .map(Self::of)
            .unwrap_or_default()
    }

    /// The rule: true when an item is shown. `hidden` is the volume's own mark and `system` says
    /// whether the item is a housekeeping item.
    pub const fn shows(self, hidden: bool, system: bool) -> bool {
        if system {
            self.show_system
        } else if hidden {
            self.show_hidden
        } else {
            true
        }
    }
}

/// What a folder listing does with its entries: the switches, whether the folder is the volume's
/// root (the housekeeping table has root-only names), and whether the status bar's "hidden"
/// button has revealed the folder's hidden entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rule {
    /// The switches.
    pub visibility: Visibility,
    /// True when the folder is the root of its volume.
    pub at_root: bool,
    /// True while the folder shows everything, until the user leaves it.
    pub reveal: bool,
}

impl Rule {
    /// The rule that hides nothing.
    pub const fn default_everything() -> Self {
        Self {
            visibility: Visibility::EVERYTHING,
            at_root: false,
            reveal: false,
        }
    }

    /// The housekeeping class of `entry`, if it has one.
    pub fn system_of(&self, entry: &DirEntry<'_>) -> Option<SystemItem> {
        system_item(
            entry.name,
            entry.kind,
            self.at_root,
            entry.flags.contains(EntryFlags::HIDDEN),
        )
    }

    /// True when the switches hide `entry` (whether or not the folder is revealed).
    pub fn hides(&self, entry: &DirEntry<'_>) -> bool {
        !self.visibility.shows(
            entry.flags.contains(EntryFlags::HIDDEN),
            self.system_of(entry).is_some(),
        )
    }
}

/// The tooltip of the status bar's hidden button: it names the two settings.
pub const HIDDEN_TIP: &str = "Show hidden items and Show housekeeping files in Settings decide \
    what is left out. Click to show them in this folder until you leave it.";

#[cfg(test)]
mod tests {
    use disk_atlas_lib::fs::{NodeId, NodeKind};

    use super::*;

    fn entry(name: &str, kind: NodeKind, hidden: bool) -> DirEntry<'_> {
        DirEntry {
            node: NodeId(1),
            name,
            kind,
            size: None,
            resource_size: None,
            modified: None,
            flags: if hidden {
                EntryFlags::default().with(EntryFlags::HIDDEN)
            } else {
                EntryFlags::default()
            },
        }
    }

    const fn vis(show_hidden: bool, show_system: bool) -> Visibility {
        Visibility {
            show_hidden,
            show_system,
        }
    }

    #[test]
    fn a_plain_item_is_always_shown() {
        for hidden in [false, true] {
            for system in [false, true] {
                assert!(vis(hidden, system).shows(false, false));
            }
        }
    }

    #[test]
    fn a_hidden_item_follows_show_hidden_alone() {
        assert!(!vis(false, false).shows(true, false));
        assert!(!vis(false, true).shows(true, false));
        assert!(vis(true, false).shows(true, false));
        assert!(vis(true, true).shows(true, false));
    }

    #[test]
    fn a_housekeeping_item_follows_show_system_items_alone() {
        // Whether or not the volume also marks it hidden.
        for marked in [false, true] {
            assert!(!vis(false, false).shows(marked, true));
            assert!(!vis(true, false).shows(marked, true));
            assert!(vis(false, true).shows(marked, true));
            assert!(vis(true, true).shows(marked, true));
        }
    }

    #[test]
    fn the_two_switches_are_independent_on_real_names() {
        let ds = entry(".DS_Store", NodeKind::File, true);
        let ignore = entry(".gitignore", NodeKind::File, true);
        let plain = entry("notes.txt", NodeKind::File, false);
        let rule = |show_hidden, show_system| Rule {
            visibility: vis(show_hidden, show_system),
            at_root: false,
            reveal: false,
        };
        // Only show_hidden: .gitignore appears, .DS_Store does not.
        assert!(!rule(true, false).hides(&ignore));
        assert!(rule(true, false).hides(&ds));
        // Only show_system_items: the reverse.
        assert!(rule(false, true).hides(&ignore));
        assert!(!rule(false, true).hides(&ds));
        // Both off: both hidden. Both on: neither.
        assert!(rule(false, false).hides(&ignore) && rule(false, false).hides(&ds));
        assert!(!rule(true, true).hides(&ignore) && !rule(true, true).hides(&ds));
        for r in [rule(false, false), rule(true, true), rule(true, false)] {
            assert!(!r.hides(&plain));
        }
    }

    #[test]
    fn root_names_count_only_at_the_root() {
        let spotlight = entry(".Spotlight-V100", NodeKind::Directory, true);
        let at = |at_root| Rule {
            visibility: Visibility::default(),
            at_root,
            reveal: false,
        };
        assert_eq!(
            at(true).system_of(&spotlight),
            Some(SystemItem::SpotlightStore)
        );
        assert_eq!(at(false).system_of(&spotlight), None);
        // Not a housekeeping item below the root, so it is a plain hidden folder there.
        assert!(at(false).hides(&spotlight));
    }

    #[test]
    fn a_revealed_folder_drops_nothing_but_still_counts() {
        let ds = entry(".DS_Store", NodeKind::File, true);
        let rule = Rule {
            visibility: Visibility::default(),
            at_root: false,
            reveal: true,
        };
        assert!(rule.hides(&ds));
    }

    #[test]
    fn all_is_everything() {
        assert!(Visibility::EVERYTHING.shows(true, true));
    }
}
