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

//! File operations on the host filesystem (`r1-redesign.md` 4): new folder, rename, copy, move,
//! Recycle Bin and their undo. Everything here is blocking; the caller runs it on the
//! background executor.
//!
//! Rule 1 allows writes on the host only. [`HostPath`] is the type-level guard: it can be made
//! only from a row of a listing whose source is the host, so no operation here can be handed a
//! node of a disk or image Disk Atlas reads itself.
//!
//! There is no permanent delete.

use std::io;
use std::path::{Path, PathBuf};

pub mod undo;

pub use undo::{Step, UndoStack};

/// Longest name a host filesystem takes, in bytes (the common limit).
const MAX_NAME_BYTES: usize = 255;

/// A path on the host filesystem. Made only by [`HostPath::of`] from a listing row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPath(PathBuf);

impl HostPath {
    /// The path of row `position` of `listing`; `None` for anything that is not a plain row of a
    /// host folder (a disk, an image, a search hit).
    pub fn of(
        listing: &crate::state::listing::Listing,
        position: usize,
        cx: &gpui::App,
    ) -> Option<Self> {
        listing.host_path_of(position, cx).map(Self)
    }

    /// The folder shown, when it is a host folder.
    pub fn folder_of(listing: &crate::state::listing::Listing, cx: &gpui::App) -> Option<Self> {
        listing.host_folder_path(cx).map(Self)
    }

    /// The path.
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// For undo, which only ever holds paths this module made or was given.
    pub(in crate::ops) fn new_unchecked(path: PathBuf) -> Self {
        Self(path)
    }

    #[cfg(test)]
    pub fn for_test(path: PathBuf) -> Self {
        Self(path)
    }
}

/// What to do when the target of a copy or move exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conflict {
    /// Put the existing one in the Recycle Bin and write the new one.
    Replace,
    /// Write the new one under another name: "name (2)".
    KeepBoth,
    /// Leave this one out.
    Skip,
}

/// Why a name cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameError {
    Empty,
    /// Holds a character a host filesystem refuses.
    BadCharacter(char),
    /// "." or "..", or ends in a dot or space.
    BadEnd,
    /// A device name Windows reserves.
    Reserved,
    TooLong,
}

impl NameError {
    /// The sentence for the user.
    pub fn message(self) -> String {
        match self {
            Self::Empty => "A name cannot be empty.".to_owned(),
            Self::BadCharacter(c) => format!("A name cannot contain \u{201c}{c}\u{201d}."),
            Self::BadEnd => "A name cannot end with a dot or a space.".to_owned(),
            Self::Reserved => "Windows keeps that name for a device.".to_owned(),
            Self::TooLong => "That name is too long.".to_owned(),
        }
    }
}

/// Checks `name` against what every host filesystem Disk Atlas runs on accepts, so a name that
/// works here also works on the other systems.
pub fn check_name(name: &str) -> Result<(), NameError> {
    if name.trim().is_empty() {
        return Err(NameError::Empty);
    }
    if let Some(c) = name.chars().find(|c| {
        c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
    }) {
        return Err(NameError::BadCharacter(c));
    }
    if name == "." || name == ".." || name.ends_with('.') || name.ends_with(' ') {
        return Err(NameError::BadEnd);
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(NameError::TooLong);
    }
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.ends_with(|c: char| c.is_ascii_digit() && c != '0'));
    if reserved {
        return Err(NameError::Reserved);
    }
    Ok(())
}

/// Where the rename field's selection ends: the stem, not the extension.
pub fn stem_len(name: &str) -> usize {
    match name.rfind('.') {
        Some(0) | None => name.len(),
        Some(dot) => dot,
    }
}

/// `name` made unique inside `dir`: "name", then "name (2)", "name (3)" and so on, the number
/// before the extension.
pub fn unique_name(dir: &Path, name: &str) -> String {
    if !exists(&dir.join(name)) {
        return name.to_owned();
    }
    let stem = stem_len(name);
    let (base, extension) = name.split_at(stem);
    (2u32..)
        .map(|n| format!("{base} ({n}){extension}"))
        .find(|candidate| !exists(&dir.join(candidate)))
        .unwrap_or_else(|| name.to_owned())
}

/// True when something is at `path`, a link included.
fn exists(path: &Path) -> bool {
    path.symlink_metadata().is_ok()
}

/// Makes "New folder" (or "New folder (2)") in `parent`.
pub fn new_folder(parent: &HostPath) -> io::Result<PathBuf> {
    let parent = parent.path();
    // `create_dir` fails when the name exists, so two makers cannot pick the same name.
    for _ in 0..1000 {
        let path = parent.join(unique_name(parent, "New folder"));
        match std::fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free name for a new folder",
    ))
}

/// Renames the item at `from` to `new_name` in the same folder. Refuses a name that is taken,
/// except for a change of case only.
pub fn rename(from: &HostPath, new_name: &str) -> io::Result<PathBuf> {
    check_name(new_name)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.message()))?;
    let from = from.path();
    let to = from.with_file_name(new_name);
    let same_item_other_case = from
        .file_name()
        .is_some_and(|old| old.to_string_lossy().to_lowercase() == new_name.to_lowercase());
    if exists(&to) && !same_item_other_case {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("\u{201c}{new_name}\u{201d} already exists here."),
        ));
    }
    std::fs::rename(from, &to)?;
    Ok(to)
}

/// The first of `items` that is, or holds, a disk or image Disk Atlas has open. A host
/// operation never targets one (rule 1).
pub fn open_conflict<'a>(items: &'a [HostPath], open: &[PathBuf]) -> Option<&'a HostPath> {
    items
        .iter()
        .find(|item| open.iter().any(|path| path.starts_with(item.path())))
}

/// How many of `items` already have a namesake in `dir`.
pub fn count_conflicts(items: &[HostPath], dir: &HostPath) -> usize {
    items
        .iter()
        .filter_map(|item| item.path().file_name())
        .filter(|name| exists(&dir.path().join(name)))
        .count()
}

/// Moves `paths` to the Recycle Bin (Trash on macOS and Linux).
pub fn trash(paths: &[HostPath]) -> Result<(), String> {
    trash::delete_all(paths.iter().map(HostPath::path)).map_err(|error| error.to_string())
}

/// Copies the file or folder `from` into the folder `dir`, applying `conflict` when the name is
/// taken. Links are not followed and not copied. Returns the new path, or `None` when skipped.
pub fn copy_into(
    from: &HostPath,
    dir: &HostPath,
    conflict: Conflict,
) -> io::Result<Option<PathBuf>> {
    let (from, dir) = (from.path(), dir.path());
    let name = from
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no name"))?
        .to_string_lossy()
        .into_owned();
    if from.is_dir() && dir.starts_with(from) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "A folder cannot be copied into itself.",
        ));
    }
    let Some(target) = settle_target(dir, &name, conflict)? else {
        return Ok(None);
    };
    copy_tree(from, &target)?;
    Ok(Some(target))
}

/// Moves the item `from` into the folder `dir`. Within a volume it is a rename; across volumes
/// it is a copy, a check of the copy, then removing the original.
pub fn move_into(
    from: &HostPath,
    dir: &HostPath,
    conflict: Conflict,
) -> io::Result<Option<PathBuf>> {
    let (source, folder) = (from.path(), dir.path());
    let name = source
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no name"))?
        .to_string_lossy()
        .into_owned();
    if source.parent() == Some(folder) {
        // Already there.
        return Ok(None);
    }
    if source.is_dir() && folder.starts_with(source) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "A folder cannot be moved into itself.",
        ));
    }
    let Some(target) = settle_target(folder, &name, conflict)? else {
        return Ok(None);
    };
    match std::fs::rename(source, &target) {
        Ok(()) => Ok(Some(target)),
        Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {
            copy_tree(source, &target)?;
            if tree_size(source)? != tree_size(&target)? {
                return Err(io::Error::other(
                    "The copy is not the same size as the original, which was left in place.",
                ));
            }
            if source.is_dir() {
                std::fs::remove_dir_all(source)?;
            } else {
                std::fs::remove_file(source)?;
            }
            Ok(Some(target))
        }
        Err(error) => Err(error),
    }
}

/// The path to write `name` to inside `dir`, or `None` to skip it.
fn settle_target(dir: &Path, name: &str, conflict: Conflict) -> io::Result<Option<PathBuf>> {
    let target = dir.join(name);
    if !exists(&target) {
        return Ok(Some(target));
    }
    match conflict {
        Conflict::Skip => Ok(None),
        Conflict::KeepBoth => Ok(Some(dir.join(unique_name(dir, name)))),
        Conflict::Replace => {
            trash::delete(&target).map_err(|error| io::Error::other(error.to_string()))?;
            Ok(Some(target))
        }
    }
}

/// Copies a file or a folder tree. Links inside are left out.
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    let metadata = from.symlink_metadata()?;
    if metadata.is_dir() {
        std::fs::create_dir(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else if metadata.is_file() {
        std::fs::copy(from, to).map(|_| ())
    } else {
        Ok(())
    }
}

/// Total size of the files under `path`, for checking a copy.
fn tree_size(path: &Path) -> io::Result<u64> {
    let metadata = path.symlink_metadata()?;
    if metadata.is_dir() {
        let mut total = 0u64;
        for entry in std::fs::read_dir(path)? {
            total = total.saturating_add(tree_size(&entry?.path())?);
        }
        Ok(total)
    } else if metadata.is_file() {
        Ok(metadata.len())
    } else {
        Ok(0)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// A fresh empty folder under the system's temp folder, removed on drop.
    pub struct Temp(pub PathBuf);

    impl Temp {
        pub fn new(label: &str) -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "disk-atlas-ops-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("creates the folder");
            Self(path)
        }

        pub fn host(&self) -> HostPath {
            HostPath::for_test(self.0.clone())
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn names_the_filesystems_refuse_are_refused() {
        assert_eq!(check_name(""), Err(NameError::Empty));
        assert_eq!(check_name("  "), Err(NameError::Empty));
        assert_eq!(check_name("a/b"), Err(NameError::BadCharacter('/')));
        assert_eq!(check_name("a:b"), Err(NameError::BadCharacter(':')));
        assert_eq!(check_name("a\0b"), Err(NameError::BadCharacter('\0')));
        assert_eq!(check_name(".."), Err(NameError::BadEnd));
        assert_eq!(check_name("name."), Err(NameError::BadEnd));
        assert_eq!(check_name("con.txt"), Err(NameError::Reserved));
        assert_eq!(check_name("LPT3"), Err(NameError::Reserved));
        assert_eq!(check_name(&"x".repeat(256)), Err(NameError::TooLong));
        assert_eq!(check_name("report 2.txt"), Ok(()));
        assert_eq!(check_name(".gitignore"), Ok(()));
        assert_eq!(check_name("com0"), Ok(()));
    }

    #[test]
    fn open_images_and_their_folders_are_protected() {
        let image = PathBuf::from("/disks/os9.dmg");
        let open = vec![image];
        let holder = [HostPath::for_test(PathBuf::from("/disks"))];
        let itself = [HostPath::for_test(PathBuf::from("/disks/os9.dmg"))];
        let other = [HostPath::for_test(PathBuf::from("/disks/other.dmg"))];
        assert!(open_conflict(&holder, &open).is_some());
        assert!(open_conflict(&itself, &open).is_some());
        assert!(open_conflict(&other, &open).is_none());
    }

    #[test]
    fn the_stem_stops_before_the_last_dot() {
        assert_eq!(stem_len("photo.jpg"), 5);
        assert_eq!(stem_len("a.tar.gz"), 5);
        assert_eq!(stem_len(".gitignore"), 10);
        assert_eq!(stem_len("README"), 6);
    }

    #[test]
    fn a_taken_name_gets_a_number_before_the_extension() {
        let dir = Temp::new("unique");
        assert_eq!(unique_name(&dir.0, "a.txt"), "a.txt");
        std::fs::write(dir.0.join("a.txt"), b"x").unwrap();
        assert_eq!(unique_name(&dir.0, "a.txt"), "a (2).txt");
        std::fs::write(dir.0.join("a (2).txt"), b"x").unwrap();
        assert_eq!(unique_name(&dir.0, "a.txt"), "a (3).txt");
    }

    #[test]
    fn new_folders_never_share_a_name() {
        let dir = Temp::new("newfolder");
        let first = new_folder(&dir.host()).unwrap();
        let second = new_folder(&dir.host()).unwrap();
        assert_eq!(first.file_name().unwrap(), "New folder");
        assert_eq!(second.file_name().unwrap(), "New folder (2)");
    }

    #[test]
    fn rename_refuses_a_taken_name_and_a_bad_one() {
        let dir = Temp::new("rename");
        std::fs::write(dir.0.join("a.txt"), b"a").unwrap();
        std::fs::write(dir.0.join("b.txt"), b"b").unwrap();
        let a = HostPath::for_test(dir.0.join("a.txt"));
        assert_eq!(
            rename(&a, "b.txt").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            rename(&a, "x/y").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let renamed = rename(&a, "c.txt").unwrap();
        assert!(renamed.exists() && !dir.0.join("a.txt").exists());
    }

    #[test]
    fn copy_and_move_apply_the_conflict_choice() {
        let dir = Temp::new("copymove");
        let (src, dst) = (dir.0.join("src"), dir.0.join("dst"));
        std::fs::create_dir_all(src.join("tree/inner")).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("tree/inner/f.txt"), b"data").unwrap();
        std::fs::write(src.join("a.txt"), b"new").unwrap();
        std::fs::write(dst.join("a.txt"), b"old").unwrap();
        let dst_host = HostPath::for_test(dst.clone());

        let a = HostPath::for_test(src.join("a.txt"));
        assert_eq!(copy_into(&a, &dst_host, Conflict::Skip).unwrap(), None);
        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"old");
        let kept = copy_into(&a, &dst_host, Conflict::KeepBoth)
            .unwrap()
            .unwrap();
        assert_eq!(kept.file_name().unwrap(), "a (2).txt");
        assert_eq!(std::fs::read(&kept).unwrap(), b"new");

        let tree = HostPath::for_test(src.join("tree"));
        let copied = copy_into(&tree, &dst_host, Conflict::Skip)
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read(copied.join("inner/f.txt")).unwrap(), b"data");
        assert!(src.join("tree").exists());

        let moved = move_into(
            &a,
            &HostPath::for_test(dir.0.join("elsewhere")),
            Conflict::Skip,
        );
        assert!(moved.is_err(), "the target folder does not exist");
        // Moving into its own folder changes nothing; into itself is refused.
        assert_eq!(
            move_into(&a, &HostPath::for_test(src.clone()), Conflict::Skip).unwrap(),
            None
        );
        let inside = HostPath::for_test(src.join("tree/inner"));
        assert!(move_into(&tree, &inside, Conflict::Skip).is_err());
        assert!(copy_into(&tree, &inside, Conflict::Skip).is_err());
    }
}
