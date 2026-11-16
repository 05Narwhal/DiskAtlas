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

//! What each context menu holds (`r10-everyday.md` 6.2), as a pure function of what was
//! right-clicked.
//!
//! [`build`] asks nothing but its two arguments, so every menu is tested without a window. The
//! owner of the clicked element fills a [`MenuTarget`] and a [`MenuState`] from the state it
//! already holds, and draws what comes back with `components::menu::Menu`.
//!
//! An item whose feature this build does not have is left out, never greyed ([`BUILT`]). An item
//! that applies but not right now is greyed and carries its reason as its tooltip.
//!
//! The spec's `SourceFacts`, `RowFacts` and `MenuState` also list fields for features that are
//! not built (sizes, trash, paste of files, the last copy destination, view mode, columns,
//! package, alias). They are added with the feature, in the same change that turns its [`BUILT`]
//! field on, so no field here is written and never read.

use crate::components::menu::MenuItem;
use crate::keymap::{context, file_list, quick_look, search_field, workspace};
use crate::state::order::{SortColumn, SortSpec};

/// Which features this build has. The milestone that builds a feature turns its field on in the
/// same change; an item whose feature is off is left out.
#[derive(Debug, Clone, Copy)]
// Read by `build` once each feature lands; the fields are the spec's list.
#[allow(
    dead_code,
    reason = "each field is read by the milestone that turns it on"
)]
pub struct Built {
    /// Properties (R18).
    pub properties: bool,
    /// Copy, move, rename, new folder, Recycle Bin on the host's own filesystem (R8).
    pub host_operations: bool,
    /// The space map (R7).
    pub space_map: bool,
    /// The disk view's Layout mode (R15.2).
    pub layout_mode: bool,
    /// Pins in the sidebar (M8 section 8.4).
    pub pins: bool,
    /// "Open a copy in its app": its copy path needs M5 (decision 37).
    pub open_copy: bool,
    /// Trash rows (R17a).
    pub trash_rows: bool,
    /// "Show original" for aliases and links (R17a).
    pub show_original: bool,
    /// View, "Show hidden items" and "Show housekeeping files" in the folder menu (R16); the
    /// list honours both settings. "Folders first" is not read yet.
    pub visibility_toggles: bool,
    /// "Copy to this computer" for rows of a disk Disk Atlas reads (M5 task 7).
    pub copy_to_computer: bool,
}

/// What this build has: only what works today.
pub const BUILT: Built = Built {
    properties: true,
    host_operations: false,
    space_map: true,
    layout_mode: true,
    pins: false,
    open_copy: false,
    trash_rows: true,
    show_original: true,
    visibility_toggles: true,
    copy_to_computer: true,
};

/// The place a menu was opened in. Filled by the caller from `Sources`.
#[derive(Debug, Clone, Copy)]
pub struct SourceFacts {
    /// `SourceKind::Host`: the only place file operations exist.
    pub host: bool,
    /// HFS, HFS+ or HFSX: offers "Mac path with colons".
    pub hfs_family: bool,
}

/// One selected row.
#[derive(Debug, Clone)]
pub struct RowFacts {
    /// True for a folder (or a link to one).
    pub folder: bool,
    /// A disk image or sparse bundle on the host: "Open in Disk Atlas" comes first.
    pub disk_image: bool,
    /// A package shown as one item: Quick Look and "Show package contents" in place of "Open".
    pub package: bool,
    /// A symbolic link or a classic alias: "Show original" (R17.5).
    pub original: bool,
}

/// What a disk heading or a failed whole-disk row offers.
#[derive(Debug, Clone, Copy)]
pub struct DiskFacts {
    /// A disk image: its path is a file, and Explorer can show it.
    pub image: bool,
    /// The disk has something to show (a first volume, or the whole-disk source).
    pub can_show: bool,
    /// Reading it needs administrator rights, and the app can ask for them.
    pub can_ask_permission: bool,
}

/// What a volume row offers.
#[derive(Debug, Clone, Copy)]
pub struct VolumeFacts {
    /// The volume lies in a disk image.
    pub image: bool,
    /// The volume can be browsed (so it can be searched).
    pub browsable: bool,
}

/// What was right-clicked. [`build`] is a pure function of this and a [`MenuState`].
#[derive(Debug, Clone)]
pub enum MenuTarget {
    /// One or more rows of the file list. `rows` holds the selected rows in display order, at
    /// most `MAX_ROW_FACTS` of them: enough to tell one from several and to look at the first.
    Rows {
        source: SourceFacts,
        rows: Vec<RowFacts>,
        in_search: bool,
    },
    /// Empty space in the list.
    Folder {
        source: SourceFacts,
        in_search: bool,
    },
    /// The column header.
    ColumnHeader {
        in_search: bool,
        resource_fork: bool,
    },
    /// A disk heading, or a disk that could not be opened.
    SidebarDisk(DiskFacts),
    /// A volume row (of a disk or of an image).
    SidebarVolume(VolumeFacts),
    /// A place on this computer.
    SidebarPlace,
    /// A breadcrumb segment. The current folder's segment has nowhere to go.
    Crumb { current: bool },
    /// The search field.
    TextField { has_selection: bool },
}

/// The most rows a [`MenuTarget::Rows`] carries.
pub const MAX_ROW_FACTS: usize = 64;

/// The state a menu reads besides its target.
#[derive(Debug, Clone, Copy)]
pub struct MenuState {
    /// What this build has; read by an item once its feature lands.
    #[allow(
        dead_code,
        reason = "read by the first menu item that waits on a feature"
    )]
    pub built: Built,
    /// The clipboard holds text.
    pub can_paste: bool,
    /// The list's current sort.
    pub sort: SortSpec,
    /// "Explorer", "Finder" or "Files".
    pub file_manager: &'static str,
    /// The `show_hidden` setting, for the View menu.
    pub show_hidden: bool,
    /// The `show_system_items` setting, for the View menu.
    pub show_system: bool,
}

/// The name of the host's file manager, for "Show in ...".
pub const fn file_manager_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "Explorer"
    } else if cfg!(target_os = "macos") {
        "Finder"
    } else {
        "Files"
    }
}

/// The menu of `target`. Groups are separated by one separator; an empty group leaves none. An
/// empty result means nothing to show: the owner opens no menu.
pub fn build(target: &MenuTarget, state: &MenuState) -> Vec<MenuItem> {
    let groups = match target {
        MenuTarget::Rows {
            source,
            rows,
            in_search,
        } => rows_menu(source, rows, *in_search, state),
        MenuTarget::Folder { source, in_search } => folder_menu(source, *in_search, state),
        MenuTarget::ColumnHeader {
            in_search,
            resource_fork,
        } => vec![sort_items(state, *in_search, *resource_fork, *in_search)],
        MenuTarget::SidebarDisk(facts) => disk_menu(facts, state),
        MenuTarget::SidebarVolume(facts) => volume_menu(facts, state),
        MenuTarget::SidebarPlace => vec![
            vec![
                MenuItem::action("Open", context::Open),
                MenuItem::action(
                    format!("Show in {}", state.file_manager),
                    context::ShowInFileManager,
                ),
            ],
            properties_group(state, context::Properties),
        ],
        MenuTarget::Crumb { current } => vec![
            if *current {
                Vec::new()
            } else {
                vec![MenuItem::action("Open", context::Open)]
            },
            vec![MenuItem::action("Copy path", context::CopyFolderPath)],
            properties_group(state, context::Properties),
        ],
        MenuTarget::TextField { has_selection } => text_menu(*has_selection, state),
    };
    join(groups)
}

/// The last group of a menu: Properties, with the action its owner handles. Empty when this build
/// has no Properties window.
fn properties_group(state: &MenuState, action: impl gpui::Action) -> Vec<MenuItem> {
    if state.built.properties {
        vec![MenuItem::action("Properties", action)]
    } else {
        Vec::new()
    }
}

/// The groups with one separator between neighbours, none at the ends, none for an empty group.
fn join(groups: Vec<Vec<MenuItem>>) -> Vec<MenuItem> {
    let mut out = Vec::new();
    for group in groups.into_iter().filter(|group| !group.is_empty()) {
        if !out.is_empty() {
            out.push(MenuItem::Separator);
        }
        out.extend(group);
    }
    out
}

fn rows_menu(
    source: &SourceFacts,
    rows: &[RowFacts],
    in_search: bool,
    state: &MenuState,
) -> Vec<Vec<MenuItem>> {
    if rows.is_empty() {
        return Vec::new();
    }
    let single = rows.len() == 1;
    let first = rows.first();
    // A search hit is not a child of the folder shown: its place on this computer is not known
    // without a walk, so the host-only items wait for the hit to be opened in its folder.
    let on_host = source.host && !in_search;

    let mut open = Vec::new();
    if single && let Some(row) = first {
        if on_host && row.disk_image {
            open.push(MenuItem::action(
                "Open in Disk Atlas",
                context::OpenInDiskAtlas,
            ));
        }
        if row.package {
            open.push(MenuItem::action("Quick Look", quick_look::Toggle));
            if !in_search {
                open.push(MenuItem::action(
                    "Show package contents",
                    file_list::ShowPackageContents,
                ));
            }
        } else if row.folder {
            open.push(MenuItem::action("Open", file_list::Open));
        } else if on_host {
            open.push(MenuItem::action("Open", file_list::OpenInApp));
        }
        if !row.folder && !row.package {
            open.push(MenuItem::action("Quick Look", quick_look::Toggle));
        }
        if row.original && !in_search && state.built.show_original {
            open.push(MenuItem::action("Show original", file_list::ShowOriginal));
        }
        if in_search {
            open.push(MenuItem::action(
                "Show in its folder",
                file_list::ShowInFolder,
            ));
        }
    }

    let mut place = Vec::new();
    if single && on_host {
        place.push(MenuItem::action(
            format!("Show in {}", state.file_manager),
            context::ShowInFileManager,
        ));
    }
    // Out of a disk Disk Atlas reads, onto this computer. A search hit on a disk is a row of the
    // volume, so it copies too; on the host the operation is R8's.
    if !source.host && state.built.copy_to_computer {
        place.push(MenuItem::action(
            "Copy to this computer",
            workspace::CopyToComputer,
        ));
    }
    // The map and the disk view show the folder shown: a search hit is not in it.
    if single && !in_search {
        if state.built.space_map {
            place.push(MenuItem::action(
                "Show on the space map",
                workspace::ShowSpaceMap,
            ));
        }
        // Only where Disk Atlas reads the disk itself: a host file's pieces are not known.
        if state.built.layout_mode && !source.host && first.is_some_and(|row| !row.folder) {
            place.push(MenuItem::action(
                "Show where it is on the disk",
                workspace::ShowOnDisk,
            ));
        }
    }

    let (name, path) = if single {
        ("Name", "Path")
    } else {
        ("Names", "Paths")
    };
    let mut copy_as = vec![
        MenuItem::action(name, context::CopyName),
        MenuItem::action(path, context::CopyPath),
    ];
    if source.hfs_family {
        copy_as.push(MenuItem::action(
            if single {
                "Mac path with colons"
            } else {
                "Mac paths with colons"
            },
            context::CopyMacPath,
        ));
    }
    // A search hit's node is a number in the index, not one of the volume's.
    if !source.host && !in_search {
        copy_as.push(MenuItem::action(
            if single { "Node id" } else { "Node ids" },
            context::CopyNodeId,
        ));
    }
    // The file list's menus use the workspace action: it reads the selection itself, which search
    // hits and several rows need.
    vec![
        open,
        place,
        vec![MenuItem::submenu("Copy as", copy_as)],
        properties_group(state, workspace::Properties),
    ]
}

fn folder_menu(source: &SourceFacts, in_search: bool, state: &MenuState) -> Vec<Vec<MenuItem>> {
    let sort = MenuItem::submenu("Sort by", sort_items(state, in_search, false, in_search));
    let mut folder = Vec::new();
    // A search result list is not a folder: there is nothing to read again or to copy a path of.
    if !in_search {
        folder.push(MenuItem::action("Refresh", workspace::Refresh));
        folder.push(MenuItem::action(
            "Copy this folder's path",
            context::CopyFolderPath,
        ));
        if source.host {
            folder.push(MenuItem::action(
                format!("Show in {}", state.file_manager),
                context::ShowInFileManager,
            ));
        }
    }
    let mut first = Vec::new();
    let mut view = Vec::new();
    if state.built.space_map {
        view.push(MenuItem::action("List", workspace::ShowList));
        view.push(MenuItem::action("Space map", workspace::ShowSpaceMap));
    }
    if state.built.visibility_toggles {
        if !view.is_empty() {
            view.push(MenuItem::Separator);
        }
        view.push(
            MenuItem::action("Show hidden items", workspace::ToggleHidden)
                .checked(state.show_hidden),
        );
        view.push(
            MenuItem::action("Show housekeeping files", workspace::ToggleSystemItems)
                .checked(state.show_system),
        );
    }
    if !view.is_empty() {
        first.push(MenuItem::submenu("View", view));
    }
    first.push(sort);
    vec![
        first,
        folder,
        properties_group(state, workspace::Properties),
    ]
}

/// The columns to sort by and the direction, with the current ones checked. `relevance` adds the
/// two columns only search results have.
fn sort_items(
    state: &MenuState,
    relevance: bool,
    resource_fork: bool,
    folder_column: bool,
) -> Vec<MenuItem> {
    let SortSpec { column, descending } = state.sort;
    fn by(label: &'static str, on: bool, action: impl gpui::Action) -> MenuItem {
        MenuItem::action(label, action).checked(on)
    }
    let mut out = Vec::new();
    if relevance {
        out.push(by(
            "Relevance",
            column == SortColumn::Relevance,
            context::SortByRelevance,
        ));
    }
    out.push(by("Name", column == SortColumn::Name, context::SortByName));
    if folder_column {
        out.push(by(
            "Folder",
            column == SortColumn::Folder,
            context::SortByFolder,
        ));
    }
    out.push(by(
        "Modified",
        column == SortColumn::Modified,
        context::SortByModified,
    ));
    out.push(by("Size", column == SortColumn::Size, context::SortBySize));
    if resource_fork {
        out.push(by(
            "Resource fork",
            column == SortColumn::ResourceFork,
            context::SortByResourceFork,
        ));
    }
    out.push(by("Kind", column == SortColumn::Kind, context::SortByKind));
    out.push(MenuItem::Separator);
    out.push(MenuItem::action("Ascending", context::SortAscending).checked(!descending));
    out.push(MenuItem::action("Descending", context::SortDescending).checked(descending));
    out
}

fn disk_menu(facts: &DiskFacts, state: &MenuState) -> Vec<Vec<MenuItem>> {
    let mut group = Vec::new();
    if facts.can_show {
        group.push(MenuItem::action("Show this disk", context::Open));
    }
    if facts.can_ask_permission {
        group.push(MenuItem::action(
            "Allow reading disks",
            context::AllowReadingDisks,
        ));
    }
    if facts.image {
        group.push(MenuItem::action("Copy path", context::CopyDevicePath));
    } else {
        group.push(MenuItem::action(
            "Copy device path",
            context::CopyDevicePath,
        ));
    }
    let mut image = Vec::new();
    if facts.image {
        image.push(MenuItem::action(
            format!("Show the image file in {}", state.file_manager),
            context::ShowInFileManager,
        ));
    }
    vec![group, image, properties_group(state, context::Properties)]
}

fn volume_menu(facts: &VolumeFacts, state: &MenuState) -> Vec<Vec<MenuItem>> {
    let mut group = vec![MenuItem::action("Open", context::Open)];
    if facts.browsable {
        group.push(MenuItem::action(
            "Search this volume",
            context::SearchVolume,
        ));
    }
    let mut image = Vec::new();
    if facts.image {
        image.push(MenuItem::action(
            format!("Show the image file in {}", state.file_manager),
            context::ShowInFileManager,
        ));
    }
    vec![group, image, properties_group(state, context::Properties)]
}

fn text_menu(has_selection: bool, state: &MenuState) -> Vec<Vec<MenuItem>> {
    let needs_selection = |item: MenuItem| {
        if has_selection {
            item
        } else {
            item.disabled("Nothing selected")
        }
    };
    let paste = MenuItem::action("Paste", search_field::Paste);
    let paste = if state.can_paste {
        paste
    } else {
        paste.disabled("Nothing to paste")
    };
    vec![
        vec![
            needs_selection(MenuItem::action("Cut", search_field::Cut)),
            needs_selection(MenuItem::action("Copy", search_field::Copy)),
            paste,
        ],
        vec![MenuItem::action("Select all", search_field::SelectAll)],
    ]
}

#[cfg(test)]
mod tests {
    use crate::components::menu::Enabled;

    use super::*;

    const STATE: MenuState = MenuState {
        built: BUILT,
        can_paste: true,
        sort: SortSpec {
            column: SortColumn::Name,
            descending: false,
        },
        file_manager: "Explorer",
        show_hidden: false,
        show_system: false,
    };

    fn source(host: bool, hfs: bool) -> SourceFacts {
        SourceFacts {
            host,
            hfs_family: hfs,
        }
    }

    fn file(_name: &str) -> RowFacts {
        RowFacts {
            folder: false,
            disk_image: false,
            package: false,
            original: false,
        }
    }

    fn folder(_name: &str) -> RowFacts {
        RowFacts {
            folder: true,
            ..file("")
        }
    }

    /// One line per item: the label, `*` when checked, `(off: reason)` when greyed, `>` and the
    /// nested lines for a submenu, `-` for a separator.
    fn lines(items: &[MenuItem]) -> Vec<String> {
        let mut out = Vec::new();
        for item in items {
            match item {
                MenuItem::Separator => out.push("-".into()),
                MenuItem::Action {
                    label,
                    checked,
                    enabled,
                    ..
                } => {
                    let mut line = label.to_string();
                    if *checked == Some(true) {
                        line.push('*');
                    }
                    if let Enabled::No { reason } = enabled {
                        line.push_str(&format!(" (off: {reason})"));
                    }
                    out.push(line);
                }
                MenuItem::Submenu { label, items, .. } => {
                    out.push(format!("{label} >"));
                    out.extend(lines(items).into_iter().map(|line| format!("  {line}")));
                }
            }
        }
        out
    }

    fn menu(target: &MenuTarget) -> Vec<String> {
        lines(&build(target, &STATE))
    }

    fn rows(source: SourceFacts, rows: Vec<RowFacts>, in_search: bool) -> MenuTarget {
        MenuTarget::Rows {
            source,
            rows,
            in_search,
        }
    }

    #[test]
    fn a_file_on_a_read_only_disk_offers_quick_look_and_copy_as() {
        let got = menu(&rows(source(false, false), vec![file("a.txt")], false));
        assert_eq!(
            got,
            [
                "Quick Look",
                "-",
                "Copy to this computer",
                "Show on the space map",
                "Show where it is on the disk",
                "-",
                "Copy as >",
                "  Name",
                "  Path",
                "  Node id",
                "-",
                "Properties"
            ]
        );
    }

    #[test]
    fn rows_of_a_disk_copy_to_this_computer_and_rows_of_the_host_do_not() {
        let copy = "Copy to this computer".to_owned();
        // One row, several rows, a folder: all of a disk.
        for rows_of in [
            vec![file("a")],
            vec![file("a"), folder("b")],
            vec![folder("b")],
        ] {
            assert!(menu(&rows(source(false, false), rows_of, false)).contains(&copy));
        }
        // A search hit on a disk is a row of the volume.
        assert!(menu(&rows(source(false, false), vec![file("a")], true)).contains(&copy));
        // The host's own files are R8's to copy.
        assert!(!menu(&rows(source(true, false), vec![file("a")], false)).contains(&copy));
        assert!(!menu(&rows(source(true, false), vec![file("a")], true)).contains(&copy));
        // And it goes when the build does not have it.
        let state = MenuState {
            built: Built {
                copy_to_computer: false,
                ..BUILT
            },
            ..STATE
        };
        let off = lines(&build(
            &rows(source(false, false), vec![file("a")], false),
            &state,
        ));
        assert!(!off.contains(&copy));
    }

    #[test]
    fn a_folder_on_a_read_only_disk_opens_and_has_no_quick_look() {
        let got = menu(&rows(source(false, false), vec![folder("Docs")], false));
        assert_eq!(got[0], "Open");
        assert!(!got.contains(&"Quick Look".to_owned()));
        assert!(!got.iter().any(|l| l.contains("Explorer")));
    }

    #[test]
    fn the_hfs_family_offers_the_mac_path() {
        let got = menu(&rows(source(false, true), vec![file("a")], false));
        assert!(got.contains(&"  Mac path with colons".to_owned()));
        let host = menu(&rows(source(true, false), vec![file("a")], false));
        assert!(!host.iter().any(|l| l.contains("Mac path")));
    }

    #[test]
    fn a_host_file_opens_in_its_app_and_shows_in_the_file_manager() {
        let got = menu(&rows(source(true, false), vec![file("a.txt")], false));
        assert_eq!(
            got,
            [
                "Open",
                "Quick Look",
                "-",
                "Show in Explorer",
                "Show on the space map",
                "-",
                "Copy as >",
                "  Name",
                "  Path",
                "-",
                "Properties"
            ]
        );
    }

    #[test]
    fn a_package_offers_quick_look_and_its_contents_not_open() {
        let package = RowFacts {
            package: true,
            ..folder("Thing.app")
        };
        let got = menu(&rows(source(true, false), vec![package], false));
        assert_eq!(&got[..2], ["Quick Look", "Show package contents"]);
        assert!(!got.contains(&"Open".to_string()));
    }

    #[test]
    fn a_disk_image_on_the_host_opens_in_disk_atlas_first() {
        let image = RowFacts {
            disk_image: true,
            ..file("a.dmg")
        };
        let got = menu(&rows(source(true, false), vec![image.clone()], false));
        assert_eq!(got[0], "Open in Disk Atlas");
        // Not on a disk Disk Atlas reads, where the image is not a file of this computer.
        let disk = menu(&rows(source(false, true), vec![image], false));
        assert!(!disk.contains(&"Open in Disk Atlas".to_owned()));
    }

    #[test]
    fn several_rows_have_no_open_and_plural_copy_labels() {
        let got = menu(&rows(
            source(true, true),
            vec![file("a"), folder("b")],
            false,
        ));
        assert_eq!(
            got,
            [
                "Copy as >",
                "  Names",
                "  Paths",
                "  Mac paths with colons",
                "-",
                "Properties"
            ]
        );
    }

    #[test]
    fn a_search_hit_can_show_itself_in_its_folder_and_has_no_host_items() {
        let got = menu(&rows(source(true, false), vec![file("a.txt")], true));
        assert_eq!(
            got,
            [
                "Quick Look",
                "Show in its folder",
                "-",
                "Copy as >",
                "  Name",
                "  Path",
                "-",
                "Properties"
            ]
        );
        let disk = menu(&rows(source(false, false), vec![file("a")], true));
        assert!(!disk.iter().any(|l| l.contains("Node id")));
    }

    #[test]
    fn nothing_is_offered_for_a_feature_that_is_not_built() {
        let all: Vec<String> = [
            rows(source(true, true), vec![file("a")], false),
            MenuTarget::Folder {
                source: source(true, true),
                in_search: false,
            },
            MenuTarget::SidebarPlace,
        ]
        .iter()
        .flat_map(menu)
        .collect();
        for word in [
            "Rename",
            "Paste",
            "Recycle",
            "Pin",
            "Show original",
            "Open a copy",
            "Cut",
            "Duplicate",
        ] {
            assert!(
                !all.iter().any(|line| line.contains(word)),
                "{word} must be left out"
            );
        }
    }

    #[test]
    fn empty_space_in_a_folder_sorts_refreshes_and_copies_its_path() {
        let got = menu(&MenuTarget::Folder {
            source: source(true, false),
            in_search: false,
        });
        assert_eq!(
            got,
            [
                "View >",
                "  List",
                "  Space map",
                "  -",
                "  Show hidden items",
                "  Show housekeeping files",
                "Sort by >",
                "  Name*",
                "  Modified",
                "  Size",
                "  Kind",
                "  -",
                "  Ascending*",
                "  Descending",
                "-",
                "Refresh",
                "Copy this folder's path",
                "Show in Explorer",
                "-",
                "Properties"
            ]
        );
        let disk = menu(&MenuTarget::Folder {
            source: source(false, true),
            in_search: false,
        });
        assert!(!disk.iter().any(|l| l.contains("Explorer")));
    }

    #[test]
    fn the_view_menu_checks_the_switches_that_are_on() {
        let mut state = STATE;
        state.show_hidden = true;
        let got = lines(&build(
            &MenuTarget::Folder {
                source: source(false, true),
                in_search: false,
            },
            &state,
        ));
        assert_eq!(
            &got[..6],
            [
                "View >",
                "  List",
                "  Space map",
                "  -",
                "  Show hidden items*",
                "  Show housekeeping files"
            ]
        );
    }

    #[test]
    fn empty_space_in_results_only_sorts() {
        let got = menu(&MenuTarget::Folder {
            source: source(true, false),
            in_search: true,
        });
        assert_eq!(got[0], "View >");
        assert_eq!(got[6], "Sort by >");
        assert_eq!(got[7], "  Relevance");
        assert!(!got.contains(&"Refresh".to_owned()));
        assert!(got.contains(&"  Folder".to_owned()));
    }

    #[test]
    fn the_current_sort_is_checked() {
        let mut state = STATE;
        state.sort = SortSpec {
            column: SortColumn::Size,
            descending: true,
        };
        let got = lines(&build(
            &MenuTarget::ColumnHeader {
                in_search: false,
                resource_fork: true,
            },
            &state,
        ));
        assert_eq!(
            got,
            [
                "Name",
                "Modified",
                "Size*",
                "Resource fork",
                "Kind",
                "-",
                "Ascending",
                "Descending*"
            ]
        );
    }

    #[test]
    fn disks_volumes_and_places() {
        let disk = menu(&MenuTarget::SidebarDisk(DiskFacts {
            image: false,
            can_show: true,
            can_ask_permission: true,
        }));
        assert_eq!(
            disk,
            [
                "Show this disk",
                "Allow reading disks",
                "Copy device path",
                "-",
                "Properties"
            ]
        );
        let image = menu(&MenuTarget::SidebarDisk(DiskFacts {
            image: true,
            can_show: true,
            can_ask_permission: false,
        }));
        assert_eq!(
            image,
            [
                "Show this disk",
                "Copy path",
                "-",
                "Show the image file in Explorer",
                "-",
                "Properties"
            ]
        );
        let volume = menu(&MenuTarget::SidebarVolume(VolumeFacts {
            image: false,
            browsable: true,
        }));
        assert_eq!(volume, ["Open", "Search this volume", "-", "Properties"]);
        let unreadable = menu(&MenuTarget::SidebarVolume(VolumeFacts {
            image: true,
            browsable: false,
        }));
        assert_eq!(
            unreadable,
            [
                "Open",
                "-",
                "Show the image file in Explorer",
                "-",
                "Properties"
            ]
        );
        assert_eq!(
            menu(&MenuTarget::SidebarPlace),
            ["Open", "Show in Explorer", "-", "Properties"]
        );
    }

    #[test]
    fn a_crumb_offers_open_unless_it_is_the_current_folder() {
        assert_eq!(
            menu(&MenuTarget::Crumb { current: false }),
            ["Open", "-", "Copy path", "-", "Properties"]
        );
        assert_eq!(
            menu(&MenuTarget::Crumb { current: true }),
            ["Copy path", "-", "Properties"]
        );
    }

    #[test]
    fn every_menu_ends_with_properties_but_the_column_header_and_the_text_field() {
        let disk = |image, can_show, can_ask_permission| {
            MenuTarget::SidebarDisk(DiskFacts {
                image,
                can_show,
                can_ask_permission,
            })
        };
        let volume = |image, browsable| MenuTarget::SidebarVolume(VolumeFacts { image, browsable });
        let targets = [
            rows(source(false, false), vec![file("a")], false),
            rows(source(false, true), vec![folder("a"), file("b")], false),
            rows(source(true, false), vec![file("a")], true),
            MenuTarget::Folder {
                source: source(true, false),
                in_search: false,
            },
            MenuTarget::Folder {
                source: source(false, true),
                in_search: true,
            },
            disk(false, true, true),
            disk(true, false, false),
            volume(false, true),
            // An "other partition": a volume the app cannot browse.
            volume(false, false),
            MenuTarget::SidebarPlace,
            MenuTarget::Crumb { current: false },
            MenuTarget::Crumb { current: true },
        ];
        for target in &targets {
            let got = menu(target);
            assert_eq!(
                got.last().map(String::as_str),
                Some("Properties"),
                "{target:?}"
            );
            assert_eq!(got[got.len() - 2], "-", "{target:?}");
        }
        for target in [
            MenuTarget::ColumnHeader {
                in_search: false,
                resource_fork: false,
            },
            MenuTarget::TextField {
                has_selection: true,
            },
        ] {
            assert!(!menu(&target).iter().any(|line| line == "Properties"));
        }
    }

    #[test]
    fn no_rows_means_no_menu_and_a_build_without_properties_leaves_it_out() {
        assert!(menu(&rows(source(true, false), Vec::new(), false)).is_empty());
        let mut state = STATE;
        state.built.properties = false;
        let got = lines(&build(&MenuTarget::Crumb { current: true }, &state));
        assert_eq!(got, ["Copy path"]);
    }

    #[test]
    fn a_text_field_greys_what_does_not_apply_and_says_why() {
        let mut state = STATE;
        state.can_paste = false;
        let got = lines(&build(
            &MenuTarget::TextField {
                has_selection: false,
            },
            &state,
        ));
        assert_eq!(
            got,
            [
                "Cut (off: Nothing selected)",
                "Copy (off: Nothing selected)",
                "Paste (off: Nothing to paste)",
                "-",
                "Select all"
            ]
        );
        let got = menu(&MenuTarget::TextField {
            has_selection: true,
        });
        assert_eq!(got[..3], ["Cut", "Copy", "Paste"]);
    }

    #[test]
    fn no_empty_menu_has_a_separator_and_none_starts_or_ends_with_one() {
        let targets = [
            MenuTarget::SidebarPlace,
            MenuTarget::Crumb { current: true },
            MenuTarget::SidebarDisk(DiskFacts {
                image: false,
                can_show: false,
                can_ask_permission: false,
            }),
            rows(source(false, false), Vec::new(), false),
        ];
        for target in &targets {
            let got = menu(target);
            assert_ne!(got.first().map(String::as_str), Some("-"));
            assert_ne!(got.last().map(String::as_str), Some("-"));
        }
    }
}
