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

//! The table of housekeeping items: names that operating systems and tools
//! leave on volumes and that the user normally does not want to see, and the
//! root `.hidden` list of old Mac OS X.
//! Source: `.claude/docs/design/r10-everyday.md` section 5.1.
//!
//! Everything here is pure except [`read_hidden_list`], which reads one
//! small file from a volume.

use crate::fs::{FileSystem, NodeKind, Stream};

/// Most bytes of a root `.hidden` file that are read; the rest is ignored.
pub const MAX_HIDDEN_LIST: usize = 64 * 1024;

/// Most names taken from a `.hidden` file.
pub const MAX_HIDDEN_NAMES: usize = 1024;

/// Longest line of a `.hidden` file taken as a name, in bytes.
pub const MAX_HIDDEN_NAME: usize = 255;

/// What kind of housekeeping item an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SystemItem {
    /// `.DS_Store`.
    DsStore,
    /// `._name` resource-fork sidecar files.
    AppleDouble,
    /// `Icon\r`, a custom folder icon.
    FolderIcon,
    /// `.localized`.
    Localized,
    /// `__MACOSX`, left by archive tools.
    MacOsxArchiveFolder,
    /// `.Spotlight-V100`.
    SpotlightStore,
    /// `.fseventsd`.
    FsEvents,
    /// `.Trashes`, `.Trash`, and the classic Trash folders (hidden only).
    Trash,
    /// `.TemporaryItems`. Also used for `.PKInstallSandboxManager` and
    /// `.PKInstallSandboxManager-SystemSoftware` (installer scratch space).
    TemporaryItems,
    /// `.DocumentRevisions-V100`.
    DocumentRevisions,
    /// `.VolumeIcon.icns`.
    VolumeIcon,
    /// `.hidden`.
    HiddenList,
    /// `.metadata_never_index` and its variant.
    IndexingMarker,
    /// Time Machine marker files. Also used for `.MobileBackups` (local
    /// Time Machine snapshots).
    TimeMachineMarker,
    /// `.apdisk`.
    AfpShare,
    /// HFS+ private stores (the `HFS+ Private Data` folder and the
    /// `.HFS+ Private Directory Data\r` folder). Also used for `.vol`, the
    /// volume-by-ID bookkeeping folder.
    HfsPrivate,
    /// HFS+ journal and hot-file files: `.journal`, `.journal_info_block`,
    /// `.hotfiles.btree`.
    HfsJournal,
    /// Classic `Desktop DB` and `Desktop DF`.
    ClassicDesktopDb,
    /// Classic `Desktop Folder`.
    ClassicDesktopFolder,
    /// Other classic Mac OS housekeeping names.
    ClassicHousekeeping,
    /// `Thumbs.db`.
    WindowsThumbs,
    /// `desktop.ini`.
    WindowsDesktopIni,
    /// `$RECYCLE.BIN`.
    WindowsRecycleBin,
    /// `System Volume Information`.
    WindowsVolumeInfo,
    /// `lost+found`.
    LinuxLostFound,
    /// `.Trash-<uid>`.
    LinuxTrash,
}

/// Every variant, for iteration.
pub const ALL: [SystemItem; 26] = [
    SystemItem::DsStore,
    SystemItem::AppleDouble,
    SystemItem::FolderIcon,
    SystemItem::Localized,
    SystemItem::MacOsxArchiveFolder,
    SystemItem::SpotlightStore,
    SystemItem::FsEvents,
    SystemItem::Trash,
    SystemItem::TemporaryItems,
    SystemItem::DocumentRevisions,
    SystemItem::VolumeIcon,
    SystemItem::HiddenList,
    SystemItem::IndexingMarker,
    SystemItem::TimeMachineMarker,
    SystemItem::AfpShare,
    SystemItem::HfsPrivate,
    SystemItem::HfsJournal,
    SystemItem::ClassicDesktopDb,
    SystemItem::ClassicDesktopFolder,
    SystemItem::ClassicHousekeeping,
    SystemItem::WindowsThumbs,
    SystemItem::WindowsDesktopIni,
    SystemItem::WindowsRecycleBin,
    SystemItem::WindowsVolumeInfo,
    SystemItem::LinuxLostFound,
    SystemItem::LinuxTrash,
];

const HFS_PRIVATE_DATA: &str = "HFS+ Private Data";

/// True when `name` is `HFS+ Private Data` preceded by four copies of `lead`.
fn is_private_data(name: &str, lead: char) -> bool {
    name.strip_prefix(lead)
        .and_then(|s| s.strip_prefix(lead))
        .and_then(|s| s.strip_prefix(lead))
        .and_then(|s| s.strip_prefix(lead))
        .is_some_and(|rest| rest == HFS_PRIVATE_DATA)
}

/// Classify an entry. `hidden` is true when the entry already carries
/// `EntryFlags::HIDDEN`; `at_root` when it is a direct child of the volume
/// root. Names match exactly, except the Windows rows, which ignore ASCII
/// case.
pub fn system_item(name: &str, kind: NodeKind, at_root: bool, hidden: bool) -> Option<SystemItem> {
    use SystemItem as S;

    // Anywhere.
    match name {
        ".DS_Store" => return Some(S::DsStore),
        "Icon\r" => return Some(S::FolderIcon),
        ".localized" => return Some(S::Localized),
        "__MACOSX" if kind == NodeKind::Directory => return Some(S::MacOsxArchiveFolder),
        _ => {}
    }
    if kind == NodeKind::File && name.strip_prefix("._").is_some_and(|r| !r.is_empty()) {
        return Some(S::AppleDouble);
    }
    if name.eq_ignore_ascii_case("Thumbs.db") {
        return Some(S::WindowsThumbs);
    }
    if name.eq_ignore_ascii_case("desktop.ini") {
        return Some(S::WindowsDesktopIni);
    }

    if !at_root {
        return None;
    }

    // Root only.
    let found = match name {
        ".Spotlight-V100" => Some(S::SpotlightStore),
        ".fseventsd" => Some(S::FsEvents),
        ".TemporaryItems"
        | ".PKInstallSandboxManager"
        | ".PKInstallSandboxManager-SystemSoftware" => Some(S::TemporaryItems),
        ".DocumentRevisions-V100" => Some(S::DocumentRevisions),
        ".MobileBackups"
        | ".com.apple.timemachine.donotpresent"
        | ".com.apple.timemachine.supported" => Some(S::TimeMachineMarker),
        ".vol" | ".HFS+ Private Directory Data\r" => Some(S::HfsPrivate),
        ".Trashes" | ".Trash" => Some(S::Trash),
        ".VolumeIcon.icns" => Some(S::VolumeIcon),
        ".hidden" => Some(S::HiddenList),
        ".metadata_never_index" | ".metadata_never_index_unless_rootfs" => Some(S::IndexingMarker),
        ".apdisk" => Some(S::AfpShare),
        ".journal" | ".journal_info_block" | ".hotfiles.btree" => Some(S::HfsJournal),
        "lost+found" if kind == NodeKind::Directory => Some(S::LinuxLostFound),
        _ => None,
    };
    if found.is_some() {
        return found;
    }
    if is_private_data(name, '\0') || is_private_data(name, '\u{1A}') {
        return Some(S::HfsPrivate);
    }
    if name.eq_ignore_ascii_case("$RECYCLE.BIN") {
        return Some(S::WindowsRecycleBin);
    }
    if name.eq_ignore_ascii_case("System Volume Information") {
        return Some(S::WindowsVolumeInfo);
    }
    if name
        .strip_prefix(".Trash-")
        .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
    {
        return Some(S::LinuxTrash);
    }

    // Classic Mac OS names count only when the Finder itself hid them.
    if !hidden {
        return None;
    }
    match name {
        "Desktop DB" | "Desktop DF" => Some(S::ClassicDesktopDb),
        "Desktop Folder" => Some(S::ClassicDesktopFolder),
        "Trash" | "Network Trash Folder" => Some(S::Trash),
        "Temporary Items"
        | "TheVolumeSettingsFolder"
        | "TheFindByContentFolder"
        | "Cleanup At Startup"
        | "Shutdown Check"
        | "AppleShare PDS"
        | "Move&Rename"
        | "VM Storage"
        | "OpenFolderListDF\r" => Some(S::ClassicHousekeeping),
        _ => None,
    }
}

/// The names a root `.hidden` file (Mac OS X 10.0 to 10.3) asks to hide.
///
/// Lines end in LF, CR or CRLF; nothing but the line ending is trimmed.
/// Empty lines, lines longer than [`MAX_HIDDEN_NAME`] bytes, lines holding
/// NUL or `/`, and lines that are not UTF-8 are skipped. At most the first
/// [`MAX_HIDDEN_LIST`] bytes are read (a line cut off at that limit is
/// dropped) and at most [`MAX_HIDDEN_NAMES`] names are returned.
pub fn parse_hidden_list(bytes: &[u8]) -> Vec<String> {
    let is_end = |b: &u8| *b == b'\n' || *b == b'\r';
    let mut head = bytes.get(..MAX_HIDDEN_LIST).unwrap_or(bytes);
    // Cut at the limit in the middle of a line: that line is not whole.
    if bytes.get(MAX_HIDDEN_LIST).is_some_and(|b| !is_end(b)) {
        head = match head.iter().rposition(is_end) {
            Some(end) => head.get(..end).unwrap_or(&[]),
            None => &[],
        };
    }
    let mut names = Vec::new();
    // CRLF splits into a line and an empty one, which is skipped.
    for line in head.split(is_end) {
        if names.len() >= MAX_HIDDEN_NAMES {
            break;
        }
        if line.is_empty()
            || line.len() > MAX_HIDDEN_NAME
            || line.iter().any(|&b| b == 0 || b == b'/')
        {
            continue;
        }
        if let Ok(name) = std::str::from_utf8(line) {
            names.push(name.to_owned());
        }
    }
    names
}

/// The names the volume root's `.hidden` file asks to hide, read through
/// [`parse_hidden_list`].
///
/// Advisory: an empty list when there is no `.hidden` file in the root, when
/// it is not a file, or when anything about reading it fails. Reads at most
/// [`MAX_HIDDEN_LIST`] bytes, plus one to know whether a line was cut off.
pub fn read_hidden_list(fs: &dyn FileSystem) -> Vec<String> {
    let Ok(Some(node)) = fs.lookup(fs.root(), ".hidden") else {
        return Vec::new();
    };
    match fs.metadata(node) {
        Ok(meta) if meta.kind == NodeKind::File => {}
        _ => return Vec::new(),
    }
    let Ok(source) = fs.open(node, Stream::Data) else {
        return Vec::new();
    };
    // One byte past the limit tells `parse_hidden_list` whether the last
    // line read is whole. `MAX_HIDDEN_LIST + 1` fits in u64 and usize.
    let want = source.len().min((MAX_HIDDEN_LIST as u64).saturating_add(1));
    let Ok(want) = usize::try_from(want) else {
        return Vec::new();
    };
    let mut bytes = vec![0u8; want];
    if source.read_at(0, &mut bytes).is_err() {
        return Vec::new();
    }
    parse_hidden_list(&bytes)
}

impl SystemItem {
    /// Short human name, shown in the Kind column.
    pub const fn label(self) -> &'static str {
        match self {
            Self::DsStore => "Finder view settings",
            Self::AppleDouble => "Mac file metadata",
            Self::FolderIcon => "Custom folder icon",
            Self::Localized => "Finder name translations",
            Self::MacOsxArchiveFolder => "Archive leftovers",
            Self::SpotlightStore => "Spotlight index",
            Self::FsEvents => "File change log",
            Self::Trash => "Trash",
            Self::TemporaryItems => "Temporary items",
            Self::DocumentRevisions => "Document versions",
            Self::VolumeIcon => "Volume icon",
            Self::HiddenList => "Hidden names list",
            Self::IndexingMarker => "Indexing opt-out marker",
            Self::TimeMachineMarker => "Time Machine data",
            Self::AfpShare => "File sharing marker",
            Self::HfsPrivate => "HFS+ private data",
            Self::HfsJournal => "HFS+ journal",
            Self::ClassicDesktopDb => "Classic Desktop database",
            Self::ClassicDesktopFolder => "Classic Desktop folder",
            Self::ClassicHousekeeping => "Classic Mac OS housekeeping",
            Self::WindowsThumbs => "Windows thumbnail cache",
            Self::WindowsDesktopIni => "Windows folder settings",
            Self::WindowsRecycleBin => "Windows Recycle Bin",
            Self::WindowsVolumeInfo => "Windows volume data",
            Self::LinuxLostFound => "Recovered file fragments",
            Self::LinuxTrash => "Linux Trash",
        }
    }

    /// One plain sentence: what the item is for, and that it is safe to
    /// ignore.
    pub const fn explains(self) -> &'static str {
        match self {
            Self::DsStore => {
                "The Finder stores window and icon layout for this folder here; safe to ignore."
            }
            Self::AppleDouble => {
                "Holds extra Mac metadata for the file it is named after on a disk that cannot store it natively; safe to ignore."
            }
            Self::FolderIcon => "Holds the custom icon of the folder it sits in; safe to ignore.",
            Self::Localized => {
                "Tells the Finder to show a translated name for this folder; safe to ignore."
            }
            Self::MacOsxArchiveFolder => {
                "Added by archive tools to keep Mac metadata inside a zip file; safe to ignore."
            }
            Self::SpotlightStore => {
                "The macOS Spotlight search index for this volume; safe to ignore."
            }
            Self::FsEvents => {
                "A log macOS keeps of recent file changes on this volume; safe to ignore."
            }
            Self::Trash => {
                "Files deleted from this volume wait here until the Trash is emptied; safe to ignore."
            }
            Self::TemporaryItems => {
                "Scratch space used by apps and installers while they work; safe to ignore."
            }
            Self::DocumentRevisions => {
                "Earlier versions of documents saved by macOS; safe to ignore."
            }
            Self::VolumeIcon => "The icon macOS shows for this volume; safe to ignore.",
            Self::HiddenList => {
                "A list of names the Finder should hide, used by old Mac OS X versions; safe to ignore."
            }
            Self::IndexingMarker => {
                "An empty marker that asks Spotlight not to index this volume; safe to ignore."
            }
            Self::TimeMachineMarker => {
                "Marks or holds Time Machine backup data for this volume; safe to ignore."
            }
            Self::AfpShare => {
                "Left by Apple file sharing to remember share settings; safe to ignore."
            }
            Self::HfsPrivate => {
                "Bookkeeping the HFS+ filesystem uses for hard links and open deleted files; safe to ignore."
            }
            Self::HfsJournal => {
                "Bookkeeping the HFS+ filesystem uses to recover after a crash or to speed up access; safe to ignore."
            }
            Self::ClassicDesktopDb => {
                "The Desktop database that classic Mac OS used to remember icons and file types; safe to ignore."
            }
            Self::ClassicDesktopFolder => {
                "Holds items that were on the Desktop in classic Mac OS; safe to ignore."
            }
            Self::ClassicHousekeeping => {
                "A hidden file or folder that classic Mac OS used for its own bookkeeping; safe to ignore."
            }
            Self::WindowsThumbs => {
                "A cache of picture thumbnails that Windows made for this folder; safe to ignore."
            }
            Self::WindowsDesktopIni => {
                "Holds how Windows should show this folder, such as its icon; safe to ignore."
            }
            Self::WindowsRecycleBin => {
                "Files deleted on Windows wait here until the Recycle Bin is emptied; safe to ignore."
            }
            Self::WindowsVolumeInfo => {
                "Windows keeps restore points and indexing data for this volume here; safe to ignore."
            }
            Self::LinuxLostFound => {
                "Where Linux file system checks put file fragments they recover; safe to ignore."
            }
            Self::LinuxTrash => {
                "Files deleted on Linux wait here until the Trash is emptied; safe to ignore."
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use NodeKind::{Directory as D, File as F};

    fn root(name: &str, kind: NodeKind) -> Option<SystemItem> {
        system_item(name, kind, true, false)
    }

    #[test]
    fn anywhere_rows() {
        for at_root in [true, false] {
            assert_eq!(
                system_item(".DS_Store", F, at_root, false),
                Some(SystemItem::DsStore)
            );
            assert_eq!(
                system_item("Icon\r", F, at_root, false),
                Some(SystemItem::FolderIcon)
            );
            assert_eq!(
                system_item(".localized", F, at_root, false),
                Some(SystemItem::Localized)
            );
            assert_eq!(
                system_item("__MACOSX", D, at_root, false),
                Some(SystemItem::MacOsxArchiveFolder)
            );
            assert_eq!(
                system_item("._x", F, at_root, false),
                Some(SystemItem::AppleDouble)
            );
        }
        assert_eq!(root("Icon", F), None);
        assert_eq!(root(".localised", F), None);
        assert_eq!(root("__MACOSX", F), None);
        assert_eq!(root("__MACOSX2", D), None);
    }

    #[test]
    fn appledouble_rules() {
        assert_eq!(root("._", F), None);
        assert_eq!(root("._x", D), None);
        assert_eq!(root("._x", F), Some(SystemItem::AppleDouble));
        assert_eq!(root(".x", F), None);
    }

    #[test]
    fn mac_names_are_case_sensitive() {
        assert_eq!(root(".ds_store", F), None);
        assert_eq!(root(".spotlight-v100", D), None);
    }

    #[test]
    fn root_only_rows() {
        let rows: &[(&str, SystemItem)] = &[
            (".Spotlight-V100", SystemItem::SpotlightStore),
            (".fseventsd", SystemItem::FsEvents),
            (".TemporaryItems", SystemItem::TemporaryItems),
            (".DocumentRevisions-V100", SystemItem::DocumentRevisions),
            (".PKInstallSandboxManager", SystemItem::TemporaryItems),
            (
                ".PKInstallSandboxManager-SystemSoftware",
                SystemItem::TemporaryItems,
            ),
            (".MobileBackups", SystemItem::TimeMachineMarker),
            (".vol", SystemItem::HfsPrivate),
            (".Trashes", SystemItem::Trash),
            (".Trash", SystemItem::Trash),
            (".VolumeIcon.icns", SystemItem::VolumeIcon),
            (".hidden", SystemItem::HiddenList),
            (".metadata_never_index", SystemItem::IndexingMarker),
            (
                ".metadata_never_index_unless_rootfs",
                SystemItem::IndexingMarker,
            ),
            (
                ".com.apple.timemachine.donotpresent",
                SystemItem::TimeMachineMarker,
            ),
            (
                ".com.apple.timemachine.supported",
                SystemItem::TimeMachineMarker,
            ),
            (".apdisk", SystemItem::AfpShare),
            (".HFS+ Private Directory Data\r", SystemItem::HfsPrivate),
            (".journal", SystemItem::HfsJournal),
            (".journal_info_block", SystemItem::HfsJournal),
            (".hotfiles.btree", SystemItem::HfsJournal),
            ("$RECYCLE.BIN", SystemItem::WindowsRecycleBin),
            ("System Volume Information", SystemItem::WindowsVolumeInfo),
            ("lost+found", SystemItem::LinuxLostFound),
        ];
        for (name, item) in rows {
            let kind = if *name == "lost+found" || name.starts_with('.') {
                D
            } else {
                F
            };
            assert_eq!(
                system_item(name, kind, true, false),
                Some(*item),
                "{name:?}"
            );
            assert_eq!(
                system_item(name, kind, false, false),
                None,
                "{name:?} below root"
            );
            // Near miss: a trailing character.
            let near = format!("{name}x");
            assert_eq!(system_item(&near, kind, true, false), None, "{near:?}");
        }
    }

    #[test]
    fn lost_found_is_a_folder() {
        assert_eq!(root("lost+found", F), None);
    }

    #[test]
    fn private_data_spellings() {
        let nul = "\0\0\0\0HFS+ Private Data";
        let sub = "\u{1A}\u{1A}\u{1A}\u{1A}HFS+ Private Data";
        assert_eq!(root(nul, D), Some(SystemItem::HfsPrivate));
        assert_eq!(root(sub, D), Some(SystemItem::HfsPrivate));
        assert_eq!(system_item(nul, D, false, false), None);
        assert_eq!(root("HFS+ Private Data", D), None);
        assert_eq!(root("\0\0\0HFS+ Private Data", D), None);
        assert_eq!(root("\0\0\0\0\0HFS+ Private Data", D), None);
        assert_eq!(root("\0\0\u{1A}\u{1A}HFS+ Private Data", D), None);
    }

    #[test]
    fn classic_names_need_root_and_hidden() {
        let hidden_rows: &[(&str, SystemItem)] = &[
            ("Desktop DB", SystemItem::ClassicDesktopDb),
            ("Desktop DF", SystemItem::ClassicDesktopDb),
            ("Desktop Folder", SystemItem::ClassicDesktopFolder),
            ("Trash", SystemItem::Trash),
            ("Network Trash Folder", SystemItem::Trash),
            ("Temporary Items", SystemItem::ClassicHousekeeping),
            ("TheVolumeSettingsFolder", SystemItem::ClassicHousekeeping),
            ("TheFindByContentFolder", SystemItem::ClassicHousekeeping),
            ("Cleanup At Startup", SystemItem::ClassicHousekeeping),
            ("Shutdown Check", SystemItem::ClassicHousekeeping),
            ("AppleShare PDS", SystemItem::ClassicHousekeeping),
            ("Move&Rename", SystemItem::ClassicHousekeeping),
            ("VM Storage", SystemItem::ClassicHousekeeping),
            ("OpenFolderListDF\r", SystemItem::ClassicHousekeeping),
        ];
        for (name, item) in hidden_rows {
            assert_eq!(system_item(name, D, true, true), Some(*item), "{name:?}");
            assert_eq!(
                system_item(name, D, true, false),
                None,
                "{name:?} not hidden"
            );
            assert_eq!(
                system_item(name, D, false, true),
                None,
                "{name:?} in sub-folder"
            );
            assert_eq!(
                system_item(&format!("{name}x"), D, true, true),
                None,
                "{name:?} near miss"
            );
        }
    }

    #[test]
    fn windows_rows_ignore_case() {
        for name in ["Thumbs.db", "THUMBS.DB", "thumbs.db"] {
            assert_eq!(
                system_item(name, F, false, false),
                Some(SystemItem::WindowsThumbs)
            );
        }
        for name in ["desktop.ini", "Desktop.INI"] {
            assert_eq!(
                system_item(name, F, false, false),
                Some(SystemItem::WindowsDesktopIni)
            );
        }
        for name in ["$RECYCLE.BIN", "$recycle.bin"] {
            assert_eq!(root(name, D), Some(SystemItem::WindowsRecycleBin));
            assert_eq!(system_item(name, D, false, false), None);
        }
        for name in ["System Volume Information", "SYSTEM VOLUME INFORMATION"] {
            assert_eq!(root(name, D), Some(SystemItem::WindowsVolumeInfo));
        }
        assert_eq!(root("Thumbs.dbx", F), None);
    }

    #[test]
    fn linux_trash_needs_digits() {
        assert_eq!(root(".Trash-1000", D), Some(SystemItem::LinuxTrash));
        assert_eq!(root(".Trash-0", D), Some(SystemItem::LinuxTrash));
        assert_eq!(root(".Trash-", D), None);
        assert_eq!(root(".Trash-abc", D), None);
        assert_eq!(root(".Trash-10a", D), None);
        assert_eq!(system_item(".Trash-1000", D, false, false), None);
    }

    // ---- .hidden -------------------------------------------------------

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn hidden_list_line_endings() {
        assert_eq!(parse_hidden_list(b"a\nb\nc"), names(&["a", "b", "c"]));
        assert_eq!(parse_hidden_list(b"a\rb\rc\r"), names(&["a", "b", "c"]));
        assert_eq!(
            parse_hidden_list(b"a\r\nb\r\nc\r\n"),
            names(&["a", "b", "c"])
        );
        assert_eq!(parse_hidden_list(b"a\n\r\n\rb\n\n"), names(&["a", "b"]));
        // Only the line ending goes: spaces and tabs are part of the name.
        assert_eq!(
            parse_hidden_list(b" bin \n\tusr\n"),
            names(&[" bin ", "\tusr"])
        );
    }

    #[test]
    fn hidden_list_empty_inputs() {
        assert!(parse_hidden_list(b"").is_empty());
        assert!(parse_hidden_list(b"\n").is_empty());
        assert!(parse_hidden_list(b"\r\n\r\n\n\r").is_empty());
    }

    #[test]
    fn hidden_list_skips_bad_lines() {
        let long = "x".repeat(300);
        let longest = "y".repeat(MAX_HIDDEN_NAME);
        let mut input = Vec::new();
        let lines: [&[u8]; 9] = [
            long.as_bytes(),
            longest.as_bytes(),
            b"a\0b",
            b"etc/passwd",
            b"/",
            b"bad \xFF utf8",
            b"\xC3",
            "caf\u{e9}".as_bytes(),
            b"ok",
        ];
        for line in lines {
            input.extend_from_slice(line);
            input.push(b'\n');
        }
        assert_eq!(
            parse_hidden_list(&input),
            vec![longest, "caf\u{e9}".to_owned(), "ok".to_owned()]
        );
    }

    #[test]
    fn hidden_list_stops_at_the_name_limit() {
        let input: String = (0..2000).map(|i| format!("n{i}\n")).collect();
        let got = parse_hidden_list(input.as_bytes());
        assert_eq!(got.len(), MAX_HIDDEN_NAMES);
        assert_eq!(got.first().map(String::as_str), Some("n0"));
        assert_eq!(got.last().map(String::as_str), Some("n1023"));
        // Skipped lines do not count towards the limit.
        let mixed: String = (0..2000)
            .map(|i| {
                if i % 2 == 0 {
                    "\0\n".to_owned()
                } else {
                    format!("m{i}\n")
                }
            })
            .collect();
        let got = parse_hidden_list(mixed.as_bytes());
        assert_eq!(got.len(), 1000);
    }

    #[test]
    fn hidden_list_reads_only_the_first_64_kib() {
        // 1 MiB of 8-byte lines: only lines inside the first 64 KiB count,
        // and the name limit cuts it shorter still.
        let line = b"abcdefg\n";
        let mut input = Vec::with_capacity(1 << 20);
        while input.len() < 1 << 20 {
            input.extend_from_slice(line);
        }
        assert_eq!(parse_hidden_list(&input).len(), MAX_HIDDEN_NAMES);

        // Few long lines: the byte limit, not the name limit, ends it.
        let name = "z".repeat(200);
        let mut big = Vec::new();
        while big.len() < 1 << 20 {
            big.extend_from_slice(name.as_bytes());
            big.push(b'\n');
        }
        let got = parse_hidden_list(&big);
        assert_eq!(got.len(), MAX_HIDDEN_LIST / 201);
        assert!(got.iter().all(|n| *n == name));

        // A line cut by the limit is dropped; one that ends at it is kept.
        let mut cut = vec![b'a'; MAX_HIDDEN_LIST - 3];
        cut[0] = b'\n';
        cut.extend_from_slice(b"\nlongname\n");
        assert!(parse_hidden_list(&cut).is_empty());
        let mut whole = vec![b'\n'; MAX_HIDDEN_LIST - 3];
        whole.extend_from_slice(b"abc\nnext\n");
        assert_eq!(parse_hidden_list(&whole), names(&["abc"]));
        // Exactly the limit, no line ending at all: one name.
        let mut exact = vec![b'\n'; MAX_HIDDEN_LIST - 4];
        exact.extend_from_slice(b"last");
        assert_eq!(parse_hidden_list(&exact), names(&["last"]));
    }

    mod volume {
        use super::super::*;
        use crate::fs::hfsplus::test_volume::{self, FileSpec, ROOT, VolumeBuilder};
        use crate::fs::hfsplus::{self, HfsPlus};

        fn mounted(v: &VolumeBuilder) -> HfsPlus {
            hfsplus::mount(test_volume::source(v.build())).unwrap()
        }

        fn with_content(bytes: &[u8]) -> FileSpec {
            FileSpec {
                content: Some(bytes.to_vec()),
                ..FileSpec::regular(0)
            }
        }

        #[test]
        fn read_from_the_root() {
            let mut v = VolumeBuilder::new("Old");
            v.file(ROOT, ".hidden", 16, with_content(b"bin\r\nusr\r\n\0x\r\n"))
                .folder(ROOT, "bin", 17);
            let fs = mounted(&v);
            assert_eq!(read_hidden_list(&fs), vec!["bin", "usr"]);
        }

        #[test]
        fn missing_or_not_a_file_is_empty() {
            let mut v = VolumeBuilder::new("None");
            // Only the root's own `.hidden` counts.
            v.folder(ROOT, "Docs", 16)
                .file(16, ".hidden", 17, with_content(b"Docs\n"));
            assert!(read_hidden_list(&mounted(&v)).is_empty());

            let mut v = VolumeBuilder::new("Folder");
            v.folder(ROOT, ".hidden", 16);
            assert!(read_hidden_list(&mounted(&v)).is_empty());

            let mut v = VolumeBuilder::new("Plain");
            v.folder(ROOT, "Docs", 16);
            assert!(read_hidden_list(&mounted(&v)).is_empty());
        }

        #[test]
        fn a_long_file_is_read_up_to_the_limit() {
            let mut content = Vec::new();
            for i in 0..20_000 {
                content.extend_from_slice(format!("name {i}\n").as_bytes());
            }
            let mut v = VolumeBuilder::new("Long");
            v.file(ROOT, ".hidden", 16, with_content(&content));
            let got = read_hidden_list(&mounted(&v));
            assert_eq!(got.len(), MAX_HIDDEN_NAMES);
            assert_eq!(got[0], "name 0");
        }
    }

    #[test]
    fn labels_and_explanations_are_filled_in() {
        for item in ALL {
            assert!(!item.label().is_empty(), "{item:?}");
            assert!(!item.explains().is_empty(), "{item:?}");
        }
        assert_eq!(SystemItem::DsStore.label(), "Finder view settings");
    }
}
