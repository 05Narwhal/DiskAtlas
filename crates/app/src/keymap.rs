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

//! Actions and key bindings.
//!
//! Each view declares a key context on its root element and handles its actions there; this file
//! binds keys to those actions. "secondary" is Ctrl on Windows and Linux and Cmd on macOS.

use gpui::{App, KeyBinding};

/// Key context of the file list (`views/explorer.rs`).
pub const FILE_LIST: &str = "FileList";

/// Key context of the frame every auxiliary window shares (`views/aux_window.rs`). It sits
/// above the window's own context in the stack, so a deeper context's Escape wins.
pub const AUX_WINDOW: &str = "AuxWindow";

/// Key context of the Settings window's body (`views/settings.rs`). It sits below the frame's
/// `AuxWindow`, so its Escape (clear the search, then close) wins.
pub const SETTINGS_WINDOW: &str = "SettingsWindow";

/// Key context of the Settings window's list of sections: Up and Down move through it.
pub const SETTINGS_SECTIONS: &str = "SettingsSections";

/// Key context of a segmented control in the Settings window: Left and Right move it.
pub const SETTINGS_SEGMENTS: &str = "SettingsSegments";

/// Key context of the Properties window's body (`views/properties.rs`). It sits below the
/// frame's `AuxWindow`, so Escape and Ctrl+W close the window from anywhere in it.
pub const PROPERTIES: &str = "Properties";

/// Key context of the space map (`views/space_map.rs`).
pub const SPACE_MAP: &str = "SpaceMap";

/// Key context of the Quick Look panel (`views/preview.rs`).
pub const QUICK_LOOK: &str = "QuickLook";

/// Key context of a button inside the Quick Look panel. Only Enter presses it: Space belongs to
/// the panel (it closes), so it must reach the panel's context from a focused button.
pub const QUICK_LOOK_BUTTON: &str = "QuickLookButton";

/// Key context of the bytes grid of the Quick Look panel (`views/bytes.rs`): the arrow keys move
/// its selection instead of the file list's.
pub const BYTES_GRID: &str = "BytesGrid";

/// Key context of the bytes view's "Go to byte" field. Space, the arrows and the other keys the
/// panel binds are bound to nothing here, so the field can take typing.
pub const BYTES_GO_TO: &str = "BytesGoTo";

/// Key context of a focusable button.
pub const BUTTON: &str = "Button";

/// Key context of the strip that asks about resource forks above the footer.
pub const COPY_STRIP: &str = "CopyStrip";

/// Key context of the panel with the details of a copy.
pub const COPY_DETAILS: &str = "CopyDetails";

/// Key context of the search field (`components/search_field.rs`).
pub const SEARCH_FIELD: &str = "SearchField";

/// Key context of an open menu (`components/menu.rs`).
pub const MENU: &str = "Menu";

/// Key context of the sidebar (`components/sidebar.rs`). It holds the focus after a right click
/// there, so the Menu key and Shift+F10 open the menu of the current place.
pub const SIDEBAR: &str = "Sidebar";

/// Actions of an open menu, and the one that opens a context menu.
pub mod menu {
    gpui::actions!(
        menu,
        [
            /// Open the context menu of the focused item, at the cursor row.
            Open,
            /// Cursor up one row.
            SelectPrevious,
            /// Cursor down one row.
            SelectNext,
            /// Cursor to the first row.
            SelectFirst,
            /// Cursor to the last row.
            SelectLast,
            /// Open the submenu of the row under the cursor.
            OpenSubmenu,
            /// Close the open submenu.
            CloseSubmenu,
            /// Choose the row under the cursor (open its submenu, for a submenu row).
            Confirm,
            /// Close one level: the submenu, or else the menu, returning to where it opened.
            Close,
            /// Close the menu and move focus to the next control.
            FocusNext,
            /// Close the menu and move focus to the previous control.
            FocusPrev,
        ]
    );
}

/// Actions the context menus (`state/menus.rs`) dispatch that no key binds. The element that
/// opened the menu handles them.
pub mod context {
    gpui::actions!(
        context,
        [
            /// Open the place the menu was opened on (sidebar, breadcrumb).
            Open,
            /// Open the disk image under the cursor in Disk Atlas.
            OpenInDiskAtlas,
            /// Copy the names of the selected rows.
            CopyName,
            /// Copy the paths of the selected rows.
            CopyPath,
            /// Copy the paths of the selected rows with colons, as the classic Mac OS wrote them.
            CopyMacPath,
            /// Copy the node ids of the selected rows.
            CopyNodeId,
            /// Copy the path of the folder shown, or of the breadcrumb segment.
            CopyFolderPath,
            /// Copy the device path of a disk, or the file of an image.
            CopyDevicePath,
            /// Open the Properties window of the place the menu was opened on (sidebar,
            /// breadcrumb).
            Properties,
            /// Show the item in the host's file manager.
            ShowInFileManager,
            /// Open a volume and start a search in it.
            SearchVolume,
            /// Restart with the rights to read whole disks.
            AllowReadingDisks,
            /// Sort by name.
            SortByName,
            /// Sort by the date modified.
            SortByModified,
            /// Sort by size.
            SortBySize,
            /// Sort by the resource fork's size.
            SortByResourceFork,
            /// Sort by kind.
            SortByKind,
            /// Sort search results by their folder.
            SortByFolder,
            /// Sort search results by relevance.
            SortByRelevance,
            /// Sort ascending.
            SortAscending,
            /// Sort descending.
            SortDescending,
        ]
    );
}

/// Actions of the search field.
pub mod search_field {
    gpui::actions!(
        search_field,
        [
            /// Caret one character left.
            Left,
            /// Caret one character right.
            Right,
            /// Extend the selection one character left.
            SelectLeft,
            /// Extend the selection one character right.
            SelectRight,
            /// Caret to the start of the text.
            Home,
            /// Caret to the end of the text.
            End,
            /// Extend the selection to the start of the text.
            SelectHome,
            /// Extend the selection to the end of the text.
            SelectEnd,
            /// Delete the selection or the character before the caret.
            Backspace,
            /// Delete the selection or the character after the caret.
            Delete,
            /// Select all the text.
            SelectAll,
            /// Copy the selection.
            Copy,
            /// Cut the selection.
            Cut,
            /// Paste at the caret.
            Paste,
            /// Clear the field and leave search, or return to the list when it is empty.
            Escape,
            /// Move focus to the first result in the list.
            ToList,
        ]
    );
}

/// Actions of the file list.
pub mod file_list {
    gpui::actions!(
        file_list,
        [
            /// Cursor up one row.
            MoveUp,
            /// Cursor down one row.
            MoveDown,
            /// Cursor up one screen.
            PageUp,
            /// Cursor down one screen.
            PageDown,
            /// Cursor to the first row.
            MoveToFirst,
            /// Cursor to the last row.
            MoveToLast,
            /// Extend the selection up one row.
            SelectUp,
            /// Extend the selection down one row.
            SelectDown,
            /// Extend the selection up one screen.
            SelectPageUp,
            /// Extend the selection down one screen.
            SelectPageDown,
            /// Extend the selection to the first row.
            SelectToFirst,
            /// Extend the selection to the last row.
            SelectToLast,
            /// Open the folder under the cursor.
            Open,
            /// Select every row.
            SelectAll,
            /// In search results, show the file under the cursor in its folder.
            ShowInFolder,
            /// Open the file under the cursor in its own app (files of this computer).
            OpenInApp,
        ]
    );
}

/// Actions of Quick Look. The panel handles them in its own window (the arrow keys, Close, the
/// tabs); the workspace handles Toggle and Open in the main window, where the list is.
pub mod quick_look {
    gpui::actions!(
        quick_look,
        [
            /// Open the panel on the selected file, or close it (Space in the list).
            Toggle,
            /// Close the panel and return to the list (Escape).
            Close,
            /// Show the next tab.
            NextTab,
            /// Show the previous tab.
            PreviousTab,
            /// Go to a byte offset (Bytes tab).
            GoToByte,
            /// Select the file above the current one while the panel has focus.
            PreviousFile,
            /// Select the file below the current one while the panel has focus.
            NextFile,
            /// Select the first file while the panel has focus.
            FirstFile,
            /// Select the last file while the panel has focus.
            LastFile,
            /// Scroll the preview up one page.
            ScrollPageUp,
            /// Scroll the preview down one page.
            ScrollPageDown,
            /// Scroll the preview left.
            ScrollLeft,
            /// Scroll the preview right.
            ScrollRight,
        ]
    );
}

/// Actions of the Quick Look bytes view (`views/bytes.rs`).
pub mod bytes {
    gpui::actions!(
        bytes,
        [
            /// Move the selection one byte left.
            Left,
            /// Move the selection one byte right.
            Right,
            /// Move the selection one row up.
            Up,
            /// Move the selection one row down.
            Down,
            /// Extend the selection one byte left.
            ExtendLeft,
            /// Extend the selection one byte right.
            ExtendRight,
            /// Extend the selection one row up.
            ExtendUp,
            /// Extend the selection one row down.
            ExtendDown,
            /// Move the selection a page up.
            PageUp,
            /// Move the selection a page down.
            PageDown,
            /// Select the first byte.
            Start,
            /// Select the last byte.
            End,
            /// Copy the selected bytes as hex text.
            Copy,
            /// Go to the offset typed in the "Go to byte" field.
            GoToConfirm,
            /// Close the "Go to byte" field.
            GoToClose,
            /// A key the "Go to byte" field swallows so the panel does not act on it.
            Ignore,
        ]
    );
}

/// Actions of auxiliary windows (Quick Look, Properties, Settings).
pub mod aux {
    gpui::actions!(
        aux,
        [
            /// Close the auxiliary window (Escape, Ctrl+W).
            Close,
        ]
    );
}

/// Actions of the Settings window.
pub mod settings {
    gpui::actions!(
        settings,
        [
            /// Focus the "Find a setting" field (Ctrl+F).
            FocusSearch,
            /// Clear the search; with nothing to clear, close the window (Escape).
            Escape,
            /// The section above the current one.
            SectionPrevious,
            /// The section below the current one.
            SectionNext,
            /// The previous option of the focused segmented control.
            Previous,
            /// The next option of the focused segmented control.
            Next,
        ]
    );
}

/// Actions of the space map.
pub mod space_map {
    gpui::actions!(
        space_map,
        [
            /// Go out to the enclosing folder (Backspace).
            ZoomOut,
            /// Open the region under the pointer, or the folder, in the list (Enter).
            OpenInList,
        ]
    );
}

/// Actions of the whole window: history, enclosing folder, panes.
pub mod workspace {
    gpui::actions!(
        workspace,
        [
            /// Step back in the history.
            GoBack,
            /// Step forward in the history.
            GoForward,
            /// Go to the enclosing folder.
            GoUp,
            /// Show or hide the details pane.
            ToggleDetails,
            /// Choose a disk image and add it to the sidebar.
            OpenImage,
            /// Choose a disk image that is a folder (a sparse bundle) and add it to the sidebar.
            OpenImageFolder,
            /// Move focus to the next control.
            FocusNext,
            /// Move focus to the previous control.
            FocusPrev,
            /// Focus the search field, select its text and start a search.
            Search,
            /// Leave search from the file list (Escape).
            LeaveSearch,
            /// Read the current folder again.
            Refresh,
            /// Show or hide the items the disk marks hidden (the `show_hidden` setting).
            ToggleHidden,
            /// Show or hide housekeeping files (the `show_system_items` setting).
            ToggleSystemItems,
            /// Open the Settings window, or bring the one that is open to the front (Ctrl+,).
            OpenSettings,
            /// Open the Properties window of the selection, or of the folder shown when nothing
            /// is selected (Alt+Enter, Ctrl+I).
            Properties,
            /// Copy the selected rows of a disk to a folder of this computer (Ctrl+Shift+C, the
            /// details pane's button, the file list's menu).
            CopyToComputer,
            /// Show the folder as a list (Ctrl+1).
            ShowList,
            /// Show the folder as the space map (Ctrl+3).
            ShowSpaceMap,
            /// Open the disk view in Layout mode: where the selected file lies on the disk.
            ShowOnDisk,
        ]
    );
}

/// Actions of the copy to this computer's footer, strip and panel (`components/statusbar.rs`,
/// `components/copy_ui.rs`).
pub mod transfers {
    gpui::actions!(
        transfers,
        [
            /// Stop the copy that is running.
            Cancel,
            /// Close the fork question without copying (Escape).
            CancelAsk,
            /// Close the panel with the details of a copy (Escape).
            CloseDetails,
            /// Flip "Keep resource forks and Finder information" in the copy menu.
            ToggleForks,
            /// Flip "Also copy housekeeping files" in the copy menu.
            ToggleSystemItems,
        ]
    );
}

/// Actions of focusable controls.
pub mod controls {
    gpui::actions!(
        controls,
        [
            /// Activate the focused button.
            Press,
        ]
    );
}

/// Installs every key binding. Call once at startup.
pub fn init(cx: &mut App) {
    use controls::*;
    use file_list::*;
    use workspace::*;

    let list = Some(FILE_LIST);
    cx.bind_keys([
        KeyBinding::new("up", MoveUp, list),
        KeyBinding::new("down", MoveDown, list),
        KeyBinding::new("pageup", PageUp, list),
        KeyBinding::new("pagedown", PageDown, list),
        KeyBinding::new("home", MoveToFirst, list),
        KeyBinding::new("end", MoveToLast, list),
        KeyBinding::new("shift-up", SelectUp, list),
        KeyBinding::new("shift-down", SelectDown, list),
        KeyBinding::new("shift-pageup", SelectPageUp, list),
        KeyBinding::new("shift-pagedown", SelectPageDown, list),
        KeyBinding::new("shift-home", SelectToFirst, list),
        KeyBinding::new("shift-end", SelectToLast, list),
        KeyBinding::new("enter", Open, list),
        KeyBinding::new("secondary-a", SelectAll, list),
        // Backspace is the file list's own, so a text field can keep it.
        KeyBinding::new("backspace", GoUp, list),
        KeyBinding::new("secondary-enter", ShowInFolder, list),
        KeyBinding::new("shift-enter", OpenInApp, list),
        KeyBinding::new("secondary-shift-c", CopyToComputer, list),
        // The list hands Space back when it continues a name being typed.
        KeyBinding::new("space", quick_look::Toggle, list),
        KeyBinding::new("menu", menu::Open, list),
        KeyBinding::new("shift-f10", menu::Open, list),
    ]);
    cx.bind_keys([
        KeyBinding::new("menu", menu::Open, Some(SIDEBAR)),
        KeyBinding::new("shift-f10", menu::Open, Some(SIDEBAR)),
    ]);
    cx.bind_keys([
        KeyBinding::new("alt-left", GoBack, None),
        KeyBinding::new("alt-right", GoForward, None),
        KeyBinding::new("alt-up", GoUp, None),
        // Alt+Enter and Ctrl+I (Cmd+I, "Get Info", on macOS) open Properties; the details pane's
        // toggle moved to Ctrl+Shift+I (decision 36).
        KeyBinding::new("alt-enter", Properties, None),
        KeyBinding::new("secondary-i", Properties, None),
        KeyBinding::new("secondary-shift-i", ToggleDetails, None),
        KeyBinding::new("secondary-o", OpenImage, None),
        KeyBinding::new("secondary-shift-o", OpenImageFolder, None),
        KeyBinding::new("tab", FocusNext, None),
        KeyBinding::new("shift-tab", FocusPrev, None),
        KeyBinding::new("secondary-f", Search, None),
        KeyBinding::new("f5", Refresh, None),
        KeyBinding::new("secondary-shift-.", ToggleHidden, None),
        KeyBinding::new("secondary-,", OpenSettings, None),
        KeyBinding::new("secondary-1", ShowList, None),
        KeyBinding::new("secondary-3", ShowSpaceMap, None),
        // Ctrl on every platform, as the design says: Cmd+Tab is the system's.
        KeyBinding::new("ctrl-tab", quick_look::NextTab, None),
        KeyBinding::new("ctrl-shift-tab", quick_look::PreviousTab, None),
        KeyBinding::new("secondary-g", quick_look::GoToByte, None),
    ]);
    // Escape and Ctrl+W close any auxiliary window. Quick Look's own Escape and Space (below) are in
    // a deeper context, and GPUI tries the deepest matching context first, so they win there.
    let aux_window = Some(AUX_WINDOW);
    cx.bind_keys([
        KeyBinding::new("escape", aux::Close, aux_window),
        KeyBinding::new("secondary-w", aux::Close, aux_window),
    ]);
    // The Settings window's own keys; its Escape is deeper than the frame's and wins.
    cx.bind_keys([
        KeyBinding::new("escape", settings::Escape, Some(SETTINGS_WINDOW)),
        KeyBinding::new("secondary-f", settings::FocusSearch, Some(SETTINGS_WINDOW)),
        KeyBinding::new("up", settings::SectionPrevious, Some(SETTINGS_SECTIONS)),
        KeyBinding::new("down", settings::SectionNext, Some(SETTINGS_SECTIONS)),
        KeyBinding::new("left", settings::Previous, Some(SETTINGS_SEGMENTS)),
        KeyBinding::new("right", settings::Next, Some(SETTINGS_SEGMENTS)),
    ]);
    // Inside the panel the arrow keys still move the selection of the list.
    cx.bind_keys([
        KeyBinding::new("backspace", space_map::ZoomOut, Some(SPACE_MAP)),
        KeyBinding::new("enter", space_map::OpenInList, Some(SPACE_MAP)),
    ]);
    let panel = Some(QUICK_LOOK);
    cx.bind_keys([
        KeyBinding::new("escape", quick_look::Close, panel),
        // Space closes from anywhere in the panel, a focused button included. A text field inside
        // the panel (the bytes view's "Go to byte") must bind Space in its own, deeper context.
        KeyBinding::new("space", quick_look::Close, panel),
        KeyBinding::new("up", quick_look::PreviousFile, panel),
        KeyBinding::new("down", quick_look::NextFile, panel),
        KeyBinding::new("home", quick_look::FirstFile, panel),
        KeyBinding::new("end", quick_look::LastFile, panel),
        KeyBinding::new("pageup", quick_look::ScrollPageUp, panel),
        KeyBinding::new("pagedown", quick_look::ScrollPageDown, panel),
        KeyBinding::new("left", quick_look::ScrollLeft, panel),
        KeyBinding::new("right", quick_look::ScrollRight, panel),
    ]);
    // The bytes grid: deeper than the panel, so its arrows win there.
    let grid = Some(BYTES_GRID);
    cx.bind_keys([
        KeyBinding::new("left", bytes::Left, grid),
        KeyBinding::new("right", bytes::Right, grid),
        KeyBinding::new("up", bytes::Up, grid),
        KeyBinding::new("down", bytes::Down, grid),
        KeyBinding::new("shift-left", bytes::ExtendLeft, grid),
        KeyBinding::new("shift-right", bytes::ExtendRight, grid),
        KeyBinding::new("shift-up", bytes::ExtendUp, grid),
        KeyBinding::new("shift-down", bytes::ExtendDown, grid),
        KeyBinding::new("pageup", bytes::PageUp, grid),
        KeyBinding::new("pagedown", bytes::PageDown, grid),
        KeyBinding::new("home", bytes::Start, grid),
        KeyBinding::new("end", bytes::End, grid),
        KeyBinding::new("secondary-c", bytes::Copy, grid),
    ]);
    let go_to = Some(BYTES_GO_TO);
    cx.bind_keys([
        KeyBinding::new("enter", bytes::GoToConfirm, go_to),
        KeyBinding::new("escape", bytes::GoToClose, go_to),
        KeyBinding::new("space", bytes::Ignore, go_to),
        KeyBinding::new("left", bytes::Ignore, go_to),
        KeyBinding::new("right", bytes::Ignore, go_to),
        KeyBinding::new("up", bytes::Ignore, go_to),
        KeyBinding::new("down", bytes::Ignore, go_to),
        KeyBinding::new("home", bytes::Ignore, go_to),
        KeyBinding::new("end", bytes::Ignore, go_to),
        KeyBinding::new("pageup", bytes::Ignore, go_to),
        KeyBinding::new("pagedown", bytes::Ignore, go_to),
    ]);
    // Escape in the list leaves a search. The field has its own Escape in its own context; the
    // two never meet because only one of them holds focus.
    cx.bind_keys([KeyBinding::new("escape", LeaveSearch, list)]);
    use search_field as field;
    let field_context = Some(SEARCH_FIELD);
    cx.bind_keys([
        KeyBinding::new("left", field::Left, field_context),
        KeyBinding::new("right", field::Right, field_context),
        KeyBinding::new("shift-left", field::SelectLeft, field_context),
        KeyBinding::new("shift-right", field::SelectRight, field_context),
        KeyBinding::new("home", field::Home, field_context),
        KeyBinding::new("end", field::End, field_context),
        KeyBinding::new("shift-home", field::SelectHome, field_context),
        KeyBinding::new("shift-end", field::SelectEnd, field_context),
        KeyBinding::new("backspace", field::Backspace, field_context),
        KeyBinding::new("delete", field::Delete, field_context),
        KeyBinding::new("secondary-a", field::SelectAll, field_context),
        KeyBinding::new("secondary-c", field::Copy, field_context),
        KeyBinding::new("secondary-x", field::Cut, field_context),
        KeyBinding::new("secondary-v", field::Paste, field_context),
        KeyBinding::new("escape", field::Escape, field_context),
        KeyBinding::new("down", field::ToList, field_context),
        KeyBinding::new("enter", field::ToList, field_context),
        KeyBinding::new("menu", menu::Open, field_context),
        KeyBinding::new("shift-f10", menu::Open, field_context),
    ]);
    let menu_context = Some(MENU);
    cx.bind_keys([
        KeyBinding::new("up", menu::SelectPrevious, menu_context),
        KeyBinding::new("down", menu::SelectNext, menu_context),
        KeyBinding::new("home", menu::SelectFirst, menu_context),
        KeyBinding::new("end", menu::SelectLast, menu_context),
        KeyBinding::new("left", menu::CloseSubmenu, menu_context),
        KeyBinding::new("right", menu::OpenSubmenu, menu_context),
        KeyBinding::new("enter", menu::Confirm, menu_context),
        KeyBinding::new("space", menu::Confirm, menu_context),
        KeyBinding::new("escape", menu::Close, menu_context),
        KeyBinding::new("tab", menu::FocusNext, menu_context),
        KeyBinding::new("shift-tab", menu::FocusPrev, menu_context),
    ]);
    // The fork question above the footer and the panel of a copy's details close on Escape.
    cx.bind_keys([
        KeyBinding::new("escape", transfers::CancelAsk, Some(COPY_STRIP)),
        KeyBinding::new("escape", transfers::CloseDetails, Some(COPY_DETAILS)),
    ]);
    cx.bind_keys([KeyBinding::new("enter", Press, Some(QUICK_LOOK_BUTTON))]);
    let button = Some(BUTTON);
    cx.bind_keys([
        KeyBinding::new("enter", Press, button),
        KeyBinding::new("space", Press, button),
    ]);
}
