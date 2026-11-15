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

//! Reader for AppleDouble sidecar files (`._name`) and AppleSingle files:
//! the resource fork and Finder information of a file on a volume that has
//! no forks. The writer is `extract::appledouble`. Layout: RFC 1740, every
//! integer big-endian. Source: `r10-everyday.md` section 5.3.

use std::sync::Arc;

use crate::error::{CorruptKind, Result};
use crate::fs::{FileSystem, NodeId, NodeKind, Stream};
use crate::io::block::{BlockSource, Slice};

use super::util::{be16, be32, corrupt, len64, to_usize, truncated};

const WHAT: &str = "AppleDouble file";

/// AppleDouble magic.
pub const MAGIC_DOUBLE: u32 = 0x0005_1607;
/// AppleSingle magic: the same header with the data fork inside it.
pub const MAGIC_SINGLE: u32 = 0x0005_1600;
/// The only version read.
pub const VERSION: u32 = 0x0002_0000;
/// Most entries a header may declare.
pub const MAX_AD_ENTRIES: usize = 64;
/// Most bytes read from a sidecar to find its entries.
pub const MAX_AD_HEADER: usize = 4096;

const ENTRY_RESOURCE_FORK: u32 = 2;
const ENTRY_FINDER_INFO: u32 = 9;
/// Magic, version, filler, entry count.
const FIXED: usize = 26;
const DESCRIPTOR: usize = 12;

/// What a sidecar holds that Disk Atlas uses.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sidecar {
    /// The 32 bytes of Finder information, when the sidecar has them inside
    /// the bytes read.
    pub finder_info: Option<[u8; 32]>,
    /// The resource fork as (offset, length) in the sidecar.
    pub resource: Option<(u64, u64)>,
}

/// Whether `head` starts like an AppleDouble or AppleSingle file of
/// version 2. A file that does not is an ordinary file that happens to be
/// named `._x`.
pub fn looks_like_sidecar(head: &[u8]) -> bool {
    matches!(be32(head, 0), Some(MAGIC_DOUBLE | MAGIC_SINGLE)) && be32(head, 4) == Some(VERSION)
}

/// Parses the header in `head` (the first bytes of a file of `file_len`
/// bytes). Every entry must lie inside the file.
pub fn parse(head: &[u8], file_len: u64) -> Result<Sidecar> {
    if !looks_like_sidecar(head) {
        return Err(corrupt(
            WHAT,
            0,
            CorruptKind::BadSignature {
                expected: u64::from(MAGIC_DOUBLE),
                found: u64::from(be32(head, 0).unwrap_or(0)),
            },
        ));
    }
    let Some(count) = be16(head, 24) else {
        return Err(truncated(WHAT, 24, 2, len64(head)));
    };
    let count = usize::from(count);
    if count > MAX_AD_ENTRIES {
        return Err(corrupt(
            WHAT,
            24,
            CorruptKind::OutOfRange {
                field: "entry count",
                value: count as u64,
                limit_name: "most entries",
                limit: MAX_AD_ENTRIES as u64,
            },
        ));
    }
    let mut sidecar = Sidecar::default();
    for index in 0..count {
        // Cannot overflow: count <= 64.
        let at = FIXED.saturating_add(index.saturating_mul(DESCRIPTOR));
        let (Some(id), Some(offset), Some(len)) = (
            be32(head, at),
            be32(head, at.saturating_add(4)),
            be32(head, at.saturating_add(8)),
        ) else {
            return Err(truncated(
                WHAT,
                at as u64,
                DESCRIPTOR as u64,
                len64(head).saturating_sub(at as u64),
            ));
        };
        let (offset, len) = (u64::from(offset), u64::from(len));
        let end = offset.checked_add(len).ok_or(corrupt(
            WHAT,
            at as u64,
            CorruptKind::Overflow { field: "entry end" },
        ))?;
        if end > file_len {
            return Err(truncated(WHAT, at as u64, end, file_len));
        }
        match id {
            ENTRY_RESOURCE_FORK if sidecar.resource.is_none() => {
                sidecar.resource = Some((offset, len));
            }
            ENTRY_FINDER_INFO if sidecar.finder_info.is_none() && len >= 32 => {
                // The Finder information is advisory: outside the bytes
                // read it is left out rather than read from elsewhere.
                sidecar.finder_info = head
                    .get(to_usize(offset)..)
                    .and_then(<[u8]>::first_chunk::<32>)
                    .copied();
            }
            _ => {}
        }
    }
    Ok(sidecar)
}

/// Looks up `._name` in `dir` and reads its header (at most
/// [`MAX_AD_HEADER`] bytes). `Ok(None)` when there is no such file, it is
/// not a file, or it is not an AppleDouble file; `Err` when it is one and
/// its header is damaged.
pub fn read_sidecar(
    fs: &dyn FileSystem,
    dir: NodeId,
    name: &str,
) -> Result<Option<(NodeId, Sidecar)>> {
    let Some(node) = fs.lookup(dir, &format!("._{name}"))? else {
        return Ok(None);
    };
    if fs.metadata(node)?.kind != NodeKind::File {
        return Ok(None);
    }
    let source = fs.open(node, Stream::Data)?;
    let file_len = source.len();
    let want = to_usize(file_len.min(MAX_AD_HEADER as u64));
    let mut head = vec![0u8; want];
    source.read_at(0, &mut head)?;
    if !looks_like_sidecar(&head) {
        return Ok(None);
    }
    Ok(Some((node, parse(&head, file_len)?)))
}

/// The resource fork as a window on the sidecar's data stream.
pub fn open_resource(
    fs: &dyn FileSystem,
    sidecar: NodeId,
    at: (u64, u64),
) -> Result<Arc<dyn BlockSource>> {
    let source = fs.open(sidecar, Stream::Data)?;
    Ok(Arc::new(Slice::new(source, at.0, at.1)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::appledouble::header;

    fn sidecar_bytes(finder: Option<&[u8; 32]>, fork: &[u8]) -> Vec<u8> {
        let mut bytes = header(finder, fork.len() as u64).unwrap();
        bytes.extend_from_slice(fork);
        bytes
    }

    #[test]
    fn written_sidecar_reads_back() {
        let finder = *b"TEXTttxtabcdefghijklmnopqrstuvwx";
        let bytes = sidecar_bytes(Some(&finder), b"resource fork bytes");
        let parsed = parse(&bytes[..bytes.len().min(MAX_AD_HEADER)], bytes.len() as u64).unwrap();
        assert_eq!(parsed.finder_info, Some(finder));
        let (offset, len) = parsed.resource.unwrap();
        assert_eq!(&bytes[offset as usize..(offset + len) as usize], b"resource fork bytes");
    }

    #[test]
    fn applesingle_is_accepted_and_without_a_fork_has_none() {
        let mut bytes = sidecar_bytes(None, b"");
        bytes[..4].copy_from_slice(&MAGIC_SINGLE.to_be_bytes());
        let parsed = parse(&bytes, bytes.len() as u64).unwrap();
        assert_eq!(parsed.resource, None);
    }

    #[test]
    fn hostile_headers_are_errors_not_panics() {
        let good = sidecar_bytes(None, b"fork");
        let len = good.len() as u64;
        // An entry that ends past the file.
        let mut past = good.clone();
        past[34..38].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(parse(&past, len).is_err());
        // Offset + length overflowing u32 stays inside u64 and is refused by the file length.
        let mut wrap = good.clone();
        wrap[26 + 4..26 + 8].copy_from_slice(&u32::MAX.to_be_bytes());
        wrap[26 + 8..26 + 12].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(parse(&wrap, len).is_err());
        // 65 entries.
        let mut many = good.clone();
        many[24..26].copy_from_slice(&65u16.to_be_bytes());
        assert!(parse(&many, len).is_err());
        // Entries claimed but the header is cut short.
        assert!(parse(&good[..30], len).is_err());
        // Wrong magic is not a sidecar at all.
        assert!(!looks_like_sidecar(b"not apple double"));
        assert!(parse(&[], 0).is_err());
    }
}

#[cfg(test)]
mod fuzz_seeds {
    use super::*;
    use crate::extract::appledouble::header;

    /// Writes seeds for the `appledouble` fuzz target into `DISK_ATLAS_FUZZ_SEEDS/appledouble`.
    #[test]
    #[ignore = "writes files; run by the fuzz job"]
    fn write_fuzz_seeds() {
        let Some(root) = std::env::var_os("DISK_ATLAS_FUZZ_SEEDS") else {
            return;
        };
        let dir = std::path::Path::new(&root).join("appledouble");
        std::fs::create_dir_all(&dir).expect("create seed directory");
        let mut with_fork = header(Some(&[7; 32]), 8).expect("header");
        with_fork.extend_from_slice(b"forkdata");
        std::fs::write(dir.join("with_fork"), &with_fork).expect("write seed");
        let mut single = with_fork.clone();
        single[..4].copy_from_slice(&MAGIC_SINGLE.to_be_bytes());
        std::fs::write(dir.join("single"), &single).expect("write seed");
        std::fs::write(dir.join("finder_only"), header(None, 0).expect("header"))
            .expect("write seed");
    }
}
