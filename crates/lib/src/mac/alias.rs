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

//! The classic alias record (resource `alis`, id 0, of a file with the alias
//! flag): who the alias points at. Version 2 only; version 3 and modern
//! bookmark files are reported as a newer format. Source: `r10-everyday.md`
//! section 5.3, from the published layout of the alias record. Tags 14, 15,
//! 18 and 19 are from memory and have not been checked against a file made
//! by Mac OS X.
//!
//! The version 2 fixed part is 150 bytes, big-endian: creator code 0,
//! record size 4, version 6, kind 8 (0 file, 1 folder), volume name 10
//! (length byte and 27 bytes), volume creation date 38 (seconds since 1904),
//! volume signature 42, volume type 44, **parent folder id** 46, target name
//! 50 (length byte and 63 bytes), **target file number** 114, target creation
//! date 118, type and creator 122, levels up and down 130, volume attributes
//! 134, filesystem id 138. Tagged data follows until the record size: tag
//! i16, length u16, the bytes, a pad byte when the length is odd; tag -1 ends.

use crate::error::{CorruptKind, Error, Result};
use crate::fs::hfs::encoding::Encoding;
use crate::fs::{NodeId, Timestamp};

use super::util::{be16, be32, corrupt, corrupt_text, len64, truncated};

const WHAT: &str = "alias record";

/// Longest record read.
pub const MAX_ALIAS: usize = 64 * 1024;
/// Most tags read.
pub const MAX_TAGS: usize = 64;
/// Longest tag read.
pub const MAX_TAG: usize = 4096;
/// Seconds from 1904-01-01 to 1970-01-01.
const MAC_TO_UNIX: i64 = 2_082_844_800;
const FIXED: usize = 150;

/// What an alias points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasTarget {
    /// The target is a folder.
    pub is_folder: bool,
    /// The target's volume.
    pub volume_name: String,
    /// When the volume was created, as stored (local time on a classic Mac).
    pub volume_created: Option<Timestamp>,
    /// The folder that held the target.
    pub parent: NodeId,
    /// The target's file or folder number.
    pub node: NodeId,
    /// The target's name.
    pub name: String,
    /// Tag 18 (POSIX path), else tag 2 (colon path) with colons turned into slashes.
    pub path: Option<String>,
    /// The target is on another volume than the alias.
    pub other_volume: bool,
}

/// The sentence for a record this reader does not understand.
pub const NEWER_FORMAT: &str = "This alias uses a newer format";

fn pascal(bytes: &[u8], at: usize, max: usize) -> String {
    let len = usize::from(bytes.get(at).copied().unwrap_or(0)).min(max);
    let text = bytes
        .get(at.saturating_add(1)..)
        .and_then(|rest| rest.get(..len))
        .unwrap_or_default();
    let mut out = String::new();
    Encoding::Roman.decode_into(text, &mut out);
    out
}

/// A tag-14 or tag-15 string: a u16 count of UTF-16BE units, then the units.
fn utf16_tag(data: &[u8]) -> Option<String> {
    let count = usize::from(be16(data, 0)?);
    let units = data.get(2..)?.get(..count.checked_mul(2)?)?;
    Some(
        char::decode_utf16(
            units
                .as_chunks::<2>()
                .0
                .iter()
                .copied()
                .map(u16::from_be_bytes),
        )
        .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect(),
    )
}

/// Parses an alias record. Names are read as Mac OS Roman; tags 14 and 15, when present, win
/// over the Pascal strings.
pub fn parse_alias(record: &[u8]) -> Result<AliasTarget> {
    if record.len() > MAX_ALIAS {
        return Err(corrupt(
            WHAT,
            0,
            CorruptKind::OutOfRange {
                field: "record size",
                value: len64(record),
                limit_name: "longest record",
                limit: MAX_ALIAS as u64,
            },
        ));
    }
    // A bookmark file starts with "book".
    if record.starts_with(b"book") {
        return Err(Error::unsupported(NEWER_FORMAT));
    }
    let (Some(size), Some(version)) = (be16(record, 4), be16(record, 6)) else {
        return Err(truncated(WHAT, 0, FIXED as u64, len64(record)));
    };
    match version {
        2 => {}
        3 => return Err(Error::unsupported(NEWER_FORMAT)),
        other => {
            return Err(corrupt(
                WHAT,
                6,
                CorruptKind::InvalidValue {
                    field: "version",
                    value: u64::from(other),
                },
            ));
        }
    }
    let size = usize::from(size);
    if size < FIXED || size > record.len() {
        return Err(truncated(WHAT, 4, size.max(FIXED) as u64, len64(record)));
    }
    let record = record.get(..size).unwrap_or(record);
    let created = be32(record, 38)
        .filter(|&seconds| seconds != 0)
        .map(|seconds| Timestamp::from_unix(i64::from(seconds).saturating_sub(MAC_TO_UNIX), 0));
    let mut target = AliasTarget {
        is_folder: be16(record, 8) == Some(1),
        volume_name: pascal(record, 10, 27),
        volume_created: created,
        parent: NodeId(u64::from(be32(record, 46).unwrap_or(0))),
        node: NodeId(u64::from(be32(record, 114).unwrap_or(0))),
        name: pascal(record, 50, 63),
        path: None,
        // Levels up to the common folder: -1 means another volume.
        other_volume: be16(record, 130) == Some(0xFFFF),
    };
    let (mut colon_path, mut posix_path) = (None, None);
    let mut at = FIXED;
    for _ in 0..MAX_TAGS {
        let (Some(tag), Some(len)) = (be16(record, at), be16(record, at.saturating_add(2))) else {
            break;
        };
        if tag == 0xFFFF {
            break;
        }
        let len = usize::from(len);
        if len > MAX_TAG {
            return Err(corrupt(
                WHAT,
                at as u64,
                CorruptKind::OutOfRange {
                    field: "tag length",
                    value: len as u64,
                    limit_name: "longest tag",
                    limit: MAX_TAG as u64,
                },
            ));
        }
        let start = at.saturating_add(4);
        let Some(data) = record.get(start..).and_then(|rest| rest.get(..len)) else {
            return Err(truncated(WHAT, at as u64, len as u64, len64(record)));
        };
        match tag {
            2 => {
                let mut text = String::new();
                Encoding::Roman.decode_into(data, &mut text);
                colon_path = Some(text);
            }
            14 => {
                if let Some(name) = utf16_tag(data) {
                    target.name = name;
                }
            }
            15 => {
                if let Some(name) = utf16_tag(data) {
                    target.volume_name = name;
                }
            }
            18 => posix_path = Some(String::from_utf8_lossy(data).into_owned()),
            _ => {}
        }
        // The next tag: past the data and its pad byte.
        at = start.saturating_add(len).saturating_add(len & 1);
    }
    target.path = posix_path.or_else(|| colon_path.map(|path| path.replace(':', "/")));
    if target.name.is_empty() && target.node == NodeId(0) {
        return Err(corrupt_text(WHAT, "the alias names no target"));
    }
    Ok(target)
}

/// Builds version 2 records for tests and fuzz seeds.
#[cfg(test)]
pub(crate) fn build_record(
    name: &str,
    volume: &str,
    parent: u32,
    node: u32,
    tags: &[(i16, &[u8])],
) -> Vec<u8> {
    let mut out = vec![0u8; FIXED];
    out[6..8].copy_from_slice(&2u16.to_be_bytes());
    out[10] = volume.len() as u8;
    out[11..11 + volume.len()].copy_from_slice(volume.as_bytes());
    out[38..42].copy_from_slice(&3_000_000_000u32.to_be_bytes());
    out[46..50].copy_from_slice(&parent.to_be_bytes());
    out[50] = name.len() as u8;
    out[51..51 + name.len()].copy_from_slice(name.as_bytes());
    out[114..118].copy_from_slice(&node.to_be_bytes());
    for (tag, data) in tags {
        out.extend_from_slice(&tag.to_be_bytes());
        out.extend_from_slice(&(data.len() as u16).to_be_bytes());
        out.extend_from_slice(data);
        if data.len() % 2 == 1 {
            out.push(0);
        }
    }
    out.extend_from_slice(&(-1i16).to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    let size = out.len() as u16;
    out[4..6].copy_from_slice(&size.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fixed_part_field_by_field() {
        let record = build_record("Report", "Macintosh HD", 1234, 5678, &[]);
        let target = parse_alias(&record).unwrap();
        assert_eq!(target.name, "Report");
        assert_eq!(target.volume_name, "Macintosh HD");
        assert_eq!(target.parent, NodeId(1234));
        assert_eq!(target.node, NodeId(5678));
        assert!(!target.is_folder && !target.other_volume);
        assert_eq!(
            target.volume_created,
            Some(Timestamp::from_unix(3_000_000_000 - MAC_TO_UNIX, 0))
        );
        assert_eq!(target.path, None);
    }

    #[test]
    fn tags_with_odd_lengths_and_their_pad_byte() {
        // Tag 0 is odd (3 bytes, one pad), then tag 2 the colon path, then tag 14 in UTF-16.
        let wide: Vec<u8> = [0u16, 4]
            .into_iter()
            .skip(1)
            .flat_map(u16::to_be_bytes)
            .chain("Résumé".encode_utf16().flat_map(u16::to_be_bytes))
            .collect();
        let record = build_record(
            "Resume",
            "Disk",
            2,
            3,
            &[(0, b"Doc"), (2, b"Disk:Docs:Resume"), (14, &wide)],
        );
        // The count in the tag is wrong on purpose (4 for 6 units): the name is cut to what it says.
        let target = parse_alias(&record).unwrap();
        assert_eq!(target.path.as_deref(), Some("Disk/Docs/Resume"));
        assert_eq!(target.name, "Résu");
        // Tag 18 wins over tag 2.
        let record = build_record(
            "x",
            "Disk",
            2,
            3,
            &[(2, b"Disk:x"), (18, b"/Volumes/Disk/x")],
        );
        assert_eq!(
            parse_alias(&record).unwrap().path.as_deref(),
            Some("/Volumes/Disk/x")
        );
    }

    #[test]
    fn another_volume_and_newer_formats() {
        let mut record = build_record("x", "Disk", 2, 3, &[]);
        record[130..132].copy_from_slice(&(-1i16).to_be_bytes());
        assert!(parse_alias(&record).unwrap().other_volume);
        let mut v3 = record.clone();
        v3[6..8].copy_from_slice(&3u16.to_be_bytes());
        assert!(matches!(parse_alias(&v3), Err(Error::Unsupported(text)) if text == NEWER_FORMAT));
        assert!(matches!(
            parse_alias(b"book\0\0\0\0"),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn hostile_records() {
        let good = build_record("x", "Disk", 2, 3, &[(2, b"Disk:x")]);
        // A missing end tag is fine; a tag that runs past the record is not.
        let mut cut = good.clone();
        let size = cut.len() - 4;
        cut.truncate(size);
        cut[4..6].copy_from_slice(&(size as u16).to_be_bytes());
        assert!(parse_alias(&cut).is_ok());
        let mut long = good.clone();
        long[FIXED + 2..FIXED + 4].copy_from_slice(&4000u16.to_be_bytes());
        assert!(parse_alias(&long).is_err());
        let mut huge = good.clone();
        huge[FIXED + 2..FIXED + 4].copy_from_slice(&5000u16.to_be_bytes());
        assert!(parse_alias(&huge).is_err());
        // A size larger than the bytes, shorter than the fixed part, an odd version, nothing.
        let mut size_lie = good.clone();
        size_lie[4..6].copy_from_slice(&60_000u16.to_be_bytes());
        assert!(parse_alias(&size_lie).is_err());
        let mut small = good.clone();
        small[4..6].copy_from_slice(&10u16.to_be_bytes());
        assert!(parse_alias(&small).is_err());
        let mut odd = good;
        odd[6..8].copy_from_slice(&9u16.to_be_bytes());
        assert!(parse_alias(&odd).is_err());
        assert!(parse_alias(&[]).is_err());
        assert!(parse_alias(&vec![0u8; MAX_ALIAS + 1]).is_err());
    }

    #[test]
    fn random_bytes_never_panic() {
        let mut state = 0xDEAD_BEEF_CAFE_F00Du64;
        let base = build_record("Name", "Vol", 7, 9, &[(2, b"Vol:Name"), (18, b"/a/b")]);
        for _ in 0..4000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let mut bytes = base.clone();
            for k in 0..3 {
                let at = ((state >> (k * 8)) % bytes.len() as u64) as usize;
                bytes[at] = (state >> (k * 5 + 3)) as u8;
            }
            let _ = parse_alias(&bytes);
        }
    }
}

#[cfg(test)]
mod fuzz_seeds {
    use super::*;

    /// Writes seeds for the `alias_record` fuzz target into `DISK_ATLAS_FUZZ_SEEDS/alias_record`.
    #[test]
    #[ignore = "writes files; run by the fuzz job"]
    fn write_fuzz_seeds() {
        let Some(root) = std::env::var_os("DISK_ATLAS_FUZZ_SEEDS") else {
            return;
        };
        let dir = std::path::Path::new(&root).join("alias_record");
        std::fs::create_dir_all(&dir).expect("create seed directory");
        let tagged = build_record("Name", "Vol", 7, 9, &[(2, b"Vol:Name"), (18, b"/a/b")]);
        assert!(parse_alias(&tagged).is_ok());
        std::fs::write(dir.join("tagged"), tagged).expect("write seed");
        std::fs::write(dir.join("plain"), build_record("x", "Disk", 2, 3, &[]))
            .expect("write seed");
    }
}
