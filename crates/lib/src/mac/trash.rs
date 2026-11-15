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

//! What operating systems keep of things thrown away: where they were, and
//! when. Windows Recycle Bin `$I` files, Linux `.trashinfo` files, and the
//! Trash folders of a volume. Source: `r10-everyday.md` sections 5.2 and 5.3
//! (`trash.rs`). Disk Atlas never empties a trash.
//!
//! Windows (little-endian, from the libyal format notes): version u64 at 0
//! (1 or 2), original size u64 at 8, deletion time u64 at 16 (100 ns steps
//! since 1601, UTC); version 1 has the path at 24 as UTF-16LE ending at a
//! zero unit, at most 260 units; version 2 has a u32 unit count at 24 and the
//! path at 28, at most 32,767 units.

use crate::error::Result;
use crate::fs::{DirBatch, DirProgress, FileSystem, NodeId, NodeKind, Timestamp};

use super::util::{corrupt, corrupt_text, truncated};

const WHAT: &str = "Recycle Bin record";

/// Most bytes read of a `$I` or `.trashinfo` file.
pub const MAX_TRASH_INFO: usize = 4096;
/// Most UTF-16 units of a version 1 path.
const MAX_V1_UNITS: usize = 260;
/// Most UTF-16 units of a version 2 path.
const MAX_V2_UNITS: usize = 32_767;
/// Seconds from 1601-01-01 to 1970-01-01.
const WINDOWS_TO_UNIX: u64 = 11_644_473_600;
/// Most entries looked at when asking whether a trash folder holds anything.
const PEEK_ENTRIES: usize = 64;

/// What a Windows `$I` file says about the item beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecycleRecord {
    /// Where the item was before it was deleted.
    pub original_path: String,
    /// Its size in bytes.
    pub size: u64,
    /// When it was deleted.
    pub deleted: Timestamp,
}

/// Parses the bytes of a `$I` file.
pub fn parse_recycle_record(bytes: &[u8]) -> Result<RecycleRecord> {
    let word = |at: usize| {
        bytes
            .get(at..)
            .and_then(<[u8]>::first_chunk::<8>)
            .map(|chunk| u64::from_le_bytes(*chunk))
    };
    let (Some(version), Some(size), Some(time)) = (word(0), word(8), word(16)) else {
        return Err(truncated(WHAT, 0, 24, bytes.len() as u64));
    };
    let (start, limit) = match version {
        1 => (24usize, MAX_V1_UNITS),
        2 => {
            let count = bytes
                .get(24..)
                .and_then(<[u8]>::first_chunk::<4>)
                .map(|chunk| u32::from_le_bytes(*chunk))
                .ok_or_else(|| truncated(WHAT, 24, 4, bytes.len() as u64))?;
            // The notes do not say whether the count includes the closing zero:
            // the first zero unit or the count ends the path, whichever is first.
            (28, usize::try_from(count).unwrap_or(usize::MAX).min(MAX_V2_UNITS))
        }
        other => {
            return Err(corrupt(
                WHAT,
                0,
                crate::error::CorruptKind::InvalidValue {
                    field: "version",
                    value: other,
                },
            ));
        }
    };
    let units: Vec<u16> = bytes
        .get(start..)
        .unwrap_or_default()
        .chunks_exact(2)
        .take(limit)
        .filter_map(|pair| <[u8; 2]>::try_from(pair).ok())
        .map(u16::from_le_bytes)
        .take_while(|&unit| unit != 0)
        .collect();
    if units.is_empty() {
        return Err(corrupt_text(WHAT, "the original path is empty"));
    }
    let seconds = (time / 10_000_000).saturating_sub(WINDOWS_TO_UNIX);
    Ok(RecycleRecord {
        original_path: char::decode_utf16(units)
            .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect(),
        size,
        deleted: Timestamp::from_unix(i64::try_from(seconds).unwrap_or(i64::MAX), 0),
    })
}

/// The `$R` name a `$I` name describes (`$IABC123.txt` gives `$RABC123.txt`), or `None` when
/// `name` is not a `$I` name.
pub fn recycle_data_name(name: &str) -> Option<String> {
    let rest = name.strip_prefix("$I")?;
    (!rest.is_empty()).then(|| format!("$R{rest}"))
}

/// What a Linux `info/<name>.trashinfo` file says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashInfo {
    /// Where the item was, percent-decoded.
    pub original_path: String,
    /// When it was deleted, as written (local time, no zone), read as UTC.
    pub deleted: Option<Timestamp>,
}

/// Parses the text of a `.trashinfo` file. `None` when it has no `Path=`.
pub fn parse_trashinfo(text: &str) -> Option<TrashInfo> {
    let mut path = None;
    let mut deleted = None;
    for line in text.lines().take(32) {
        if let Some(value) = line.strip_prefix("Path=") {
            path.get_or_insert_with(|| percent_decode(value.trim_end()));
        } else if let Some(value) = line.strip_prefix("DeletionDate=") {
            deleted = deleted.or_else(|| parse_local_date(value.trim()));
        }
    }
    path.filter(|path| !path.is_empty())
        .map(|original_path| TrashInfo {
            original_path,
            deleted,
        })
}

/// Decodes `%XX` escapes; a bad escape stays as written.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        let escaped = (byte == b'%')
            .then(|| bytes.get(index.saturating_add(1)..index.saturating_add(3)))
            .flatten()
            .and_then(|pair| std::str::from_utf8(pair).ok())
            .and_then(|pair| u8::from_str_radix(pair, 16).ok());
        match escaped {
            Some(value) => {
                out.push(value);
                index = index.saturating_add(3);
            }
            None => {
                out.push(byte);
                index = index.saturating_add(1);
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// "2026-11-15T10:11:12" as a timestamp (read as UTC).
fn parse_local_date(text: &str) -> Option<Timestamp> {
    let (date, time) = text.split_once('T')?;
    let mut d = date.split('-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (d.next()??, d.next()??, d.next()??);
    let mut t = time.split(':').map(|part| part.parse::<i64>().ok());
    let (hour, minute, second) = (t.next()??, t.next()??, t.next()??);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..24).contains(&hour)
        || !(0..60).contains(&minute)
        || !(0..61).contains(&second)
        || !(1..=9999).contains(&year)
    {
        return None;
    }
    Some(Timestamp::from_unix(
        days_from_civil(year, month, day)
            .saturating_mul(86_400)
            .saturating_add(hour * 3600 + minute * 60 + second),
        0,
    ))
}

/// Days since 1970-01-01 of a civil date (Howard Hinnant). Inputs are range
/// checked by the caller, so no step can overflow.
#[allow(clippy::arithmetic_side_effects)]
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Which kind of folder a [`TrashFolder`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrashKind {
    /// A Mac Trash (`.Trashes/<uid>`, `.Trash`, classic `Trash`).
    Mac,
    /// A Windows Recycle Bin (`$RECYCLE.BIN/<SID>`).
    Windows,
    /// A Linux trash (`.Trash-<uid>`).
    Linux,
    /// The classic `Desktop Folder`: not a trash, but the user's files.
    Desktop,
}

/// A folder to list as a row under a volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrashFolder {
    /// The row's text: "Trash", "Recycle Bin", "Desktop", or "User 501".
    pub label: String,
    /// The folder.
    pub node: NodeId,
    /// What it is.
    pub kind: TrashKind,
}

/// Names that do not count as something being in a trash.
fn is_noise(name: &str) -> bool {
    matches!(name, ".DS_Store" | "desktop.ini" | "Thumbs.db")
}

/// Up to [`PEEK_ENTRIES`] entries of `dir`: (node, name, is folder). Errors give what was read.
fn peek(fs: &dyn FileSystem, dir: NodeId) -> Vec<(NodeId, String, bool)> {
    let Ok(mut stream) = fs.read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut batch = DirBatch::new();
    // A bounded number of steps: a listing that keeps saying "more" with nothing is cut off.
    for _ in 0..PEEK_ENTRIES {
        batch.clear();
        let progress = stream.next_batch(&mut batch, PEEK_ENTRIES);
        for entry in batch.iter() {
            out.push((
                entry.node,
                entry.name.to_owned(),
                matches!(entry.kind, NodeKind::Directory),
            ));
        }
        if out.len() >= PEEK_ENTRIES || !matches!(progress, Ok(DirProgress::More)) {
            break;
        }
    }
    out
}

fn holds_something(fs: &dyn FileSystem, dir: NodeId) -> bool {
    peek(fs, dir).iter().any(|(_, name, _)| !is_noise(name))
}

/// The trash folders and the Desktop of a volume that hold something, found from its root.
///
/// One user's trash is shown straight in as "Trash" (or "Recycle Bin"); several are one row each,
/// "User 501". An empty trash gives no row. Blocking: a few lookups and one listing page each.
pub fn find_trash(fs: &dyn FileSystem) -> Vec<TrashFolder> {
    let root = fs.root();
    let find = |name: &str| fs.lookup(root, name).ok().flatten();
    let mut out = Vec::new();

    // Per-user folders inside one container.
    let mut users = |container: Option<NodeId>, kind: TrashKind, label: &str, prefix: &str| {
        let Some(container) = container else { return };
        let mut found: Vec<(String, NodeId)> = peek(fs, container)
            .into_iter()
            .filter(|(_, name, folder)| *folder && !is_noise(name) && !name.starts_with('.'))
            .filter(|(node, _, _)| holds_something(fs, *node))
            .map(|(node, name, _)| (name, node))
            .collect();
        found.sort();
        match found.as_slice() {
            [] => {}
            [(_, node)] => out.push(TrashFolder {
                label: label.to_owned(),
                node: *node,
                kind,
            }),
            many => out.extend(many.iter().map(|(name, node)| TrashFolder {
                label: format!("{prefix} {name}"),
                node: *node,
                kind,
            })),
        }
    };
    users(find(".Trashes"), TrashKind::Mac, "Trash", "User");
    users(find("$RECYCLE.BIN"), TrashKind::Windows, "Recycle Bin", "User");

    // A trash folder that is itself the trash.
    for (name, kind, label) in [
        (".Trash", TrashKind::Mac, "Trash"),
        ("Trash", TrashKind::Mac, "Trash"),
        (".Trash-1000", TrashKind::Linux, "Trash"),
    ] {
        if let Some(node) = find(name)
            && holds_something(fs, node)
        {
            out.push(TrashFolder {
                label: label.to_owned(),
                node,
                kind,
            });
        }
    }
    if let Some(node) = find("Desktop Folder")
        && holds_something(fs, node)
    {
        out.push(TrashFolder {
            label: "Desktop".to_owned(),
            node,
            kind: TrashKind::Desktop,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16le(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    fn record(version: u64, path: &str, count: Option<u32>) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&version.to_le_bytes());
        out.extend_from_slice(&1234u64.to_le_bytes());
        // 2026-01-01 00:00:00 UTC in 100 ns steps since 1601.
        let secs = 1_767_225_600u64 + WINDOWS_TO_UNIX;
        out.extend_from_slice(&(secs * 10_000_000).to_le_bytes());
        if version == 2 {
            let units = count.unwrap_or(path.encode_utf16().count() as u32 + 1);
            out.extend_from_slice(&units.to_le_bytes());
        }
        out.extend(utf16le(path));
        out.extend_from_slice(&[0, 0]);
        out
    }

    #[test]
    fn recycle_records_of_both_versions() {
        for version in [1, 2] {
            let parsed = parse_recycle_record(&record(version, r"C:\Users\a\Résumé.txt", None)).unwrap();
            assert_eq!(parsed.original_path, r"C:\Users\a\Résumé.txt");
            assert_eq!(parsed.size, 1234);
            assert_eq!(parsed.deleted, Timestamp::from_unix(1_767_225_600, 0));
        }
        assert_eq!(recycle_data_name("$IABC123.txt").as_deref(), Some("$RABC123.txt"));
        assert_eq!(recycle_data_name("$RABC123.txt"), None);
    }

    #[test]
    fn hostile_recycle_records() {
        // A count larger than the file, and a path with no closing zero, still end at the data.
        let mut big = record(2, "abc", Some(u32::MAX));
        big.truncate(big.len() - 2);
        assert_eq!(parse_recycle_record(&big).unwrap().original_path, "abc");
        // A count smaller than the text cuts it.
        assert_eq!(
            parse_recycle_record(&record(2, "abcdef", Some(3))).unwrap().original_path,
            "abc"
        );
        // Version 3, truncated headers, an empty path.
        assert!(parse_recycle_record(&record(3, "abc", None)).is_err());
        assert!(parse_recycle_record(&[0; 10]).is_err());
        assert!(parse_recycle_record(&record(1, "", None)).is_err());
        // A version 1 path is cut at 260 units.
        let long = "x".repeat(400);
        assert_eq!(parse_recycle_record(&record(1, &long, None)).unwrap().original_path.len(), 260);
    }

    /// A folder tree: (parent, node, name, is folder). Node 1 is the root.
    struct Tree(Vec<(u64, u64, &'static str, bool)>);

    impl FileSystem for Tree {
        fn info(&self) -> crate::fs::VolumeInfo {
            crate::fs::VolumeInfo::new("tree")
        }
        fn root(&self) -> NodeId {
            NodeId(1)
        }
        fn metadata(&self, node: NodeId) -> Result<crate::fs::Metadata> {
            Err(crate::error::Error::NotFound { node: node.0 })
        }
        fn lookup(&self, dir: NodeId, name: &str) -> Result<Option<NodeId>> {
            Ok(self
                .0
                .iter()
                .find(|(parent, _, n, _)| *parent == dir.0 && *n == name)
                .map(|(_, node, _, _)| NodeId(*node)))
        }
        fn read_dir(&self, dir: NodeId) -> Result<Box<dyn crate::fs::DirStream>> {
            struct One(Option<Vec<(u64, &'static str, bool)>>);
            impl crate::fs::DirStream for One {
                fn next_batch(&mut self, out: &mut DirBatch, _max: usize) -> Result<DirProgress> {
                    for (node, name, folder) in self.0.take().unwrap_or_default() {
                        out.push(crate::fs::DirEntry {
                            node: NodeId(node),
                            name,
                            kind: if folder { NodeKind::Directory } else { NodeKind::File },
                            size: None,
                            resource_size: None,
                            modified: None,
                            flags: crate::fs::EntryFlags::EMPTY,
                        });
                    }
                    Ok(DirProgress::Done)
                }
            }
            Ok(Box::new(One(Some(
                self.0
                    .iter()
                    .filter(|(parent, ..)| *parent == dir.0)
                    .map(|(_, node, name, folder)| (*node, *name, *folder))
                    .collect(),
            ))))
        }
    }

    #[test]
    fn trash_rows_one_user_two_users_and_empty() {
        let one = Tree(vec![
            (1, 2, ".Trashes", true),
            (2, 3, "501", true),
            (3, 4, "old.txt", false),
            (1, 5, "Desktop Folder", true),
            (5, 6, "Notes", false),
        ]);
        let rows = find_trash(&one);
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].label.as_str(), rows[0].node), ("Trash", NodeId(3)));
        assert_eq!(rows[1].label, "Desktop");
        let two = Tree(vec![
            (1, 2, ".Trashes", true),
            (2, 3, "501", true),
            (3, 4, "a", false),
            (2, 5, "502", true),
            (5, 6, "b", false),
        ]);
        let labels: Vec<_> = find_trash(&two).into_iter().map(|r| r.label).collect();
        assert_eq!(labels, ["User 501", "User 502"]);
        // An empty user folder, and a Trash with only a .DS_Store, give no row.
        let empty = Tree(vec![
            (1, 2, ".Trashes", true),
            (2, 3, "501", true),
            (1, 4, ".Trash", true),
            (4, 5, ".DS_Store", false),
        ]);
        assert!(find_trash(&empty).is_empty());
    }

    #[test]
    fn trashinfo_with_percent_encoding_and_without_a_path() {
        let info = parse_trashinfo(
            "[Trash Info]\nPath=/home/a/My%20File%C3%A9.txt\nDeletionDate=2026-01-01T00:00:00\n",
        )
        .unwrap();
        assert_eq!(info.original_path, "/home/a/My Fileé.txt");
        assert_eq!(info.deleted, Some(Timestamp::from_unix(1_767_225_600, 0)));
        assert!(parse_trashinfo("[Trash Info]\nDeletionDate=2026-01-01T00:00:00\n").is_none());
        // A bad escape stays as written and a bad date is left out.
        let odd = parse_trashinfo("Path=/a%zz\nDeletionDate=yesterday").unwrap();
        assert_eq!(odd.original_path, "/a%zz");
        assert_eq!(odd.deleted, None);
    }
}

#[cfg(test)]
mod fuzz_seeds {
    use super::*;

    /// Writes seeds for the `trash_info` fuzz target into `DISK_ATLAS_FUZZ_SEEDS/trash_info`.
    #[test]
    #[ignore = "writes files; run by the fuzz job"]
    fn write_fuzz_seeds() {
        let Some(root) = std::env::var_os("DISK_ATLAS_FUZZ_SEEDS") else {
            return;
        };
        let dir = std::path::Path::new(&root).join("trash_info");
        std::fs::create_dir_all(&dir).expect("create seed directory");
        for (version, name) in [(1u64, "recycle_v1"), (2, "recycle_v2")] {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&version.to_le_bytes());
            bytes.extend_from_slice(&5u64.to_le_bytes());
            bytes.extend_from_slice(&132_000_000_000_000_000u64.to_le_bytes());
            if version == 2 {
                bytes.extend_from_slice(&8u32.to_le_bytes());
            }
            bytes.extend("C:\\a.txt\0".encode_utf16().flat_map(u16::to_le_bytes));
            assert!(parse_recycle_record(&bytes).is_ok(), "the seed is valid");
            std::fs::write(dir.join(name), bytes).expect("write seed");
        }
        std::fs::write(
            dir.join("trashinfo"),
            "[Trash Info]\nPath=/home/a/My%20File.txt\nDeletionDate=2026-01-01T00:00:00\n",
        )
        .expect("write seed");
    }
}
