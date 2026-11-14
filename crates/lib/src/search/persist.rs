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

//! The saved index file: writing and checked loading.
//!
//! [`save`] writes an [`Index`] as one flat little-endian file; [`load`]
//! reads it back. The file lives in a user-writable cache folder, so `load`
//! treats it as hostile: it can be truncated, stale, from another volume or
//! crafted.
//!
//! # Format
//!
//! One 80-byte header, then the sections in this fixed order, with no
//! padding. `n` is the number of entries, the root included.
//!
//! | offset | bytes | field |
//! |-------:|------:|-------|
//! | 0      | 8     | magic `"DATLSIDX"` ([`MAGIC`]) |
//! | 8      | 4     | version, [`VERSION`] |
//! | 12     | 4     | `n`, entries (u32) |
//! | 16     | 8     | `names_len` (u64) |
//! | 24     | 16    | identity `id` |
//! | 40     | 16    | identity `change` |
//! | 56     | 8     | summary `entries` |
//! | 64     | 8     | summary `skipped` |
//! | 72     | 8     | `file_len`, the length of the whole file |
//! | 80     | `names_len` | names, each followed by one 0 byte |
//! |        | (n + 1) × 4 | offsets (u32) |
//! |        | n × 4 | parent (u32) |
//! |        | n × 8 | node (u64) |
//! |        | n × 8 | size (u64) |
//! |        | n × 8 | modified (i64) |
//! |        | n × 4 | rsize (u32) |
//! |        | n × 2 | flags (u16, [`IndexFlags`](crate::search::IndexFlags)) |
//!
//! So `file_len = 80 + names_len + n × 38 + 4`.
//!
//! # Loading a hostile file
//!
//! `Ok(None)` means "not ours or not current": fewer than 8 bytes, another
//! magic, another version, another volume `id` or another `change` marker.
//! The caller rebuilds. Every other defect of a file that claims to match is
//! `Err(Error::Corrupt)` with structure [`STRUCTURE`] and the byte offset of
//! the offending field. The rules, in the order they are checked:
//!
//! - The header is complete (80 bytes), and its `file_len` equals the real
//!   length the caller passes.
//! - `1 <= n <= MAX_ENTRIES` and `names_len <= u32::MAX` (offsets are u32).
//! - The real length equals the formula above, computed with checked
//!   arithmetic, before anything is allocated. Allocation is therefore
//!   bounded by the real file size, which the caller must take from the file
//!   system, not from the file.
//! - Every section is read in full; a short read is `Truncated` (the input
//!   was shorter than `file_len`), any other read failure is `Error::Io`.
//! - `offsets[0] == 0`, offsets strictly ascending (an empty name still has
//!   its separator), `offsets[n] == names_len`.
//! - The 0 bytes of `names` are exactly the bytes before each offset but the
//!   first: every name ends in a separator and holds no 0 byte.
//! - `names` is valid UTF-8 (one pass over the whole buffer).
//! - Every flag word has its reserved bit (15) clear, a valid [`KindClass`]
//!   in bits 8–11, and the package bit (7) only on a folder; the root is a
//!   folder.
//! - `parent[0] == 0`; for every other entry `parent[i] < i` and the parent
//!   is a folder, and when the parent has the housekeeping bit (6) the entry
//!   has it too.
//!
//! Version 1 (before the housekeeping, package and label bits) is another
//! version: `Ok(None)`, and the caller rebuilds once.
//!
//! `node`, `size`, `modified` and `rsize` may hold any value (the sentinels
//! are valid). `folded` is not stored; [`Index::from_parts`] recomputes it in
//! parallel. Last, [`Index::verify`] runs in every build as a final check.
//!
//! Peak memory while loading is the index plus one section's bytes.
//!
//! # Replacing a saved file
//!
//! This module writes to any [`Write`] and never touches paths. The app
//! writes to `<name>.tmp` and renames it over `<name>`, so a crash never
//! leaves a half-written file under the real name.

use std::io::{Read, Write};
use std::path::PathBuf;

use crate::error::{CorruptKind, Corruption, Error, IoContext, Location, Result};
use crate::fs::{ScanSummary, VolumeIdentity};
use crate::search::index::{Index, IndexFlags, IndexParts, MAX_ENTRIES};
use crate::search::kind::KindClass;

/// The first 8 bytes of a saved index.
pub const MAGIC: [u8; 8] = *b"DATLSIDX";
/// The format version this module writes and reads. 2 gave bits 6, 7 and
/// 12–14 of the flag words their meanings; no array changed.
pub const VERSION: u32 = 2;
/// Length of the header in bytes.
pub const HEADER_LEN: u64 = 80;
/// The `structure` of every [`Corruption`] this module returns.
pub const STRUCTURE: &str = "saved search index";

/// Array bytes per entry: `offsets` 4 (plus 4 once), `parent` 4, `node` 8,
/// `size` 8, `modified` 8, `rsize` 4, `flags` 2.
const BYTES_PER_ENTRY: u64 = 38;
/// Flag bits that must be 0: 15.
const RESERVED_FLAGS: u16 = IndexFlags::RESERVED;
/// Bytes per write while saving the arrays.
const WRITE_CHUNK: usize = 64 * 1024;

// Header field offsets.
const AT_VERSION: u64 = 8;
const AT_ENTRIES: u64 = 12;
const AT_NAMES_LEN: u64 = 16;
const AT_FILE_LEN: u64 = 72;

/// Byte offsets of every section, from the two header counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Layout {
    pub(crate) names: u64,
    pub(crate) offsets: u64,
    pub(crate) parent: u64,
    pub(crate) node: u64,
    pub(crate) size: u64,
    pub(crate) modified: u64,
    pub(crate) rsize: u64,
    pub(crate) flags: u64,
    /// The file length.
    pub(crate) end: u64,
}

impl Layout {
    /// The layout of a file of `entries` entries and `names_len` name
    /// bytes; `None` when it overflows a `u64`.
    pub(crate) fn new(entries: u32, names_len: u64) -> Option<Self> {
        let n = u64::from(entries);
        let names = HEADER_LEN;
        let offsets = names.checked_add(names_len)?;
        let parent = offsets.checked_add(n.checked_add(1)?.checked_mul(4)?)?;
        let node = parent.checked_add(n.checked_mul(4)?)?;
        let size = node.checked_add(n.checked_mul(8)?)?;
        let modified = size.checked_add(n.checked_mul(8)?)?;
        let rsize = modified.checked_add(n.checked_mul(8)?)?;
        let flags = rsize.checked_add(n.checked_mul(4)?)?;
        let end = flags.checked_add(n.checked_mul(2)?)?;
        debug_assert_eq!(
            Some(end),
            n.checked_mul(BYTES_PER_ENTRY)
                .and_then(|a| a.checked_add(4))
                .and_then(|a| a.checked_add(names_len))
                .and_then(|a| a.checked_add(HEADER_LEN))
        );
        Some(Self {
            names,
            offsets,
            parent,
            node,
            size,
            modified,
            rsize,
            flags,
            end,
        })
    }
}

/// Writes `index` to `out` in the format of the module docs.
///
/// Refuses (`Error::Unsupported`, nothing written) an index without a
/// volume identity: it could never be matched to its volume again. Write
/// failures are `Error::Io`.
pub fn save(index: &Index, out: &mut dyn Write) -> Result<()> {
    let Some(identity) = index.identity() else {
        return Err(Error::unsupported(
            "saving a search index without a volume identity",
        ));
    };
    let names = index.names_bytes();
    let entries = index.len();
    let names_len = u64::try_from(names.len()).unwrap_or(u64::MAX);
    let layout = Layout::new(entries, names_len)
        .ok_or_else(|| Error::unsupported("search index too large to save"))?;
    let summary = index.summary();

    let mut header = Vec::with_capacity(80);
    header.extend_from_slice(&MAGIC);
    header.extend_from_slice(&VERSION.to_le_bytes());
    header.extend_from_slice(&entries.to_le_bytes());
    header.extend_from_slice(&names_len.to_le_bytes());
    header.extend_from_slice(&identity.id);
    header.extend_from_slice(&identity.change);
    header.extend_from_slice(&summary.entries.to_le_bytes());
    header.extend_from_slice(&summary.skipped.to_le_bytes());
    header.extend_from_slice(&layout.end.to_le_bytes());
    debug_assert_eq!(header.len(), 80);

    write_all(out, &header)?;
    write_all(out, names)?;
    let mut buf = Vec::with_capacity(WRITE_CHUNK);
    write_section(out, &mut buf, index.offsets(), u32::to_le_bytes)?;
    write_section(out, &mut buf, index.parents(), u32::to_le_bytes)?;
    write_section(out, &mut buf, index.nodes(), u64::to_le_bytes)?;
    write_section(out, &mut buf, index.sizes(), u64::to_le_bytes)?;
    write_section(out, &mut buf, index.modified_times(), i64::to_le_bytes)?;
    write_section(out, &mut buf, index.rsizes(), u32::to_le_bytes)?;
    write_section(out, &mut buf, index.flag_words(), u16::to_le_bytes)?;
    out.flush().map_err(write_error)
}

/// Reads a saved index of `file_len` bytes (the real length, from the file
/// system) from `input`, checking every rule of the module docs.
///
/// `Ok(None)` when the file is not a saved index, or is one of another
/// version, volume or volume state than `expect`. `Err(Error::Corrupt)` when
/// it claims to match and is malformed; `Err(Error::Io)` when reading fails.
pub fn load(input: &mut dyn Read, file_len: u64, expect: VolumeIdentity) -> Result<Option<Index>> {
    // Header, and whether the file is ours.
    if file_len < 8 {
        return Ok(None);
    }
    let header = read_section(input, 0, file_len.min(HEADER_LEN))?;
    if header.get(..8) != Some(&MAGIC[..]) {
        return Ok(None);
    }
    if file_len < HEADER_LEN {
        return Err(corrupt(
            file_len,
            CorruptKind::Truncated {
                needed: HEADER_LEN,
                available: file_len,
            },
        ));
    }
    let Some(header) = header.first_chunk::<80>() else {
        return Err(corrupt(0, CorruptKind::Other("header")));
    };
    let field = Header(header);
    if field.u32(AT_VERSION) != VERSION {
        return Ok(None);
    }
    let identity = VolumeIdentity {
        id: field.bytes16(24),
        change: field.bytes16(40),
    };
    if identity != expect {
        return Ok(None);
    }

    // Lengths, before anything is allocated.
    let entries = field.u32(AT_ENTRIES);
    let names_len = field.u64(AT_NAMES_LEN);
    let claimed_len = field.u64(AT_FILE_LEN);
    let summary = ScanSummary {
        entries: field.u64(56),
        skipped: field.u64(64),
    };
    if claimed_len != file_len {
        let kind = if claimed_len > file_len {
            CorruptKind::Truncated {
                needed: claimed_len,
                available: file_len,
            }
        } else {
            CorruptKind::InvalidValue {
                field: "file_len",
                value: claimed_len,
            }
        };
        return Err(corrupt(AT_FILE_LEN, kind));
    }
    if entries == 0 {
        return Err(corrupt(
            AT_ENTRIES,
            CorruptKind::InvalidValue {
                field: "entries",
                value: 0,
            },
        ));
    }
    if entries > MAX_ENTRIES {
        return Err(corrupt(
            AT_ENTRIES,
            CorruptKind::OutOfRange {
                field: "entries",
                value: u64::from(entries),
                limit_name: "MAX_ENTRIES",
                limit: u64::from(MAX_ENTRIES),
            },
        ));
    }
    if names_len > u64::from(u32::MAX) {
        return Err(corrupt(
            AT_NAMES_LEN,
            CorruptKind::OutOfRange {
                field: "names_len",
                value: names_len,
                limit_name: "u32::MAX",
                limit: u64::from(u32::MAX),
            },
        ));
    }
    let Some(layout) = Layout::new(entries, names_len) else {
        return Err(corrupt(
            AT_ENTRIES,
            CorruptKind::Overflow {
                field: "file length from entries and names_len",
            },
        ));
    };
    if layout.end != file_len {
        let kind = if layout.end > file_len {
            CorruptKind::Truncated {
                needed: layout.end,
                available: file_len,
            }
        } else {
            CorruptKind::OutOfRange {
                field: "file length",
                value: file_len,
                limit_name: "length from entries and names_len",
                limit: layout.end,
            }
        };
        return Err(corrupt(layout.end.min(file_len), kind));
    }

    // Names and offsets.
    let names = read_section(input, layout.names, names_len)?;
    let offsets: Box<[u32]> = read_array(
        input,
        layout.offsets,
        u64::from(entries).saturating_add(1),
        u32::from_le_bytes,
    )?;
    check_offsets(&offsets, names_len, layout.offsets)?;
    check_separators(&names, &offsets, layout.names)?;
    let names = match String::from_utf8(names) {
        Ok(names) => names.into_boxed_str(),
        Err(err) => {
            let at = err.utf8_error().valid_up_to();
            let value = err.as_bytes().get(at).copied().unwrap_or(0);
            return Err(corrupt(
                at_index(layout.names, at, 1),
                CorruptKind::InvalidValue {
                    field: "name byte (not UTF-8)",
                    value: u64::from(value),
                },
            ));
        }
    };

    // The value arrays.
    let n = u64::from(entries);
    let parent: Box<[u32]> = read_array(input, layout.parent, n, u32::from_le_bytes)?;
    let node: Box<[u64]> = read_array(input, layout.node, n, u64::from_le_bytes)?;
    let size: Box<[u64]> = read_array(input, layout.size, n, u64::from_le_bytes)?;
    let modified: Box<[i64]> = read_array(input, layout.modified, n, i64::from_le_bytes)?;
    let rsize: Box<[u32]> = read_array(input, layout.rsize, n, u32::from_le_bytes)?;
    let flags: Box<[u16]> = read_array(input, layout.flags, n, u16::from_le_bytes)?;
    check_flags(&flags, layout.flags)?;
    check_parents(&parent, &flags, layout.parent, layout.flags)?;

    let index = Index::from_parts(IndexParts {
        names,
        offsets,
        parent,
        node,
        size,
        modified,
        rsize,
        flags,
        identity: Some(identity),
        summary,
    });
    index.verify().map_err(|what| {
        Error::Corrupt(Corruption::new(
            STRUCTURE,
            Location::Unspecified,
            CorruptKind::Other(what),
        ))
    })?;
    Ok(Some(index))
}

/// The fixed-size header, read through little-endian field views.
struct Header<'a>(&'a [u8; 80]);

impl Header<'_> {
    fn array<const N: usize>(&self, at: u64) -> [u8; N] {
        usize::try_from(at)
            .ok()
            .and_then(|at| self.0.get(at..)?.first_chunk::<N>())
            .copied()
            .unwrap_or([0; N])
    }

    fn u32(&self, at: u64) -> u32 {
        u32::from_le_bytes(self.array(at))
    }

    fn u64(&self, at: u64) -> u64 {
        u64::from_le_bytes(self.array(at))
    }

    fn bytes16(&self, at: u64) -> [u8; 16] {
        self.array(at)
    }
}

/// `offsets[0] == 0`, strictly ascending, last equals `names_len`.
fn check_offsets(offsets: &[u32], names_len: u64, at: u64) -> Result<()> {
    if let Some(&first) = offsets.first()
        && first != 0
    {
        return Err(corrupt(
            at,
            CorruptKind::InvalidValue {
                field: "first name offset",
                value: u64::from(first),
            },
        ));
    }
    for (k, pair) in offsets.windows(2).enumerate() {
        if let &[start, end] = pair
            && end <= start
        {
            return Err(corrupt(
                at_index(at, k.saturating_add(1), 4),
                CorruptKind::InvalidValue {
                    field: "name offset (not ascending)",
                    value: u64::from(end),
                },
            ));
        }
    }
    let last = offsets.last().map_or(0, |&o| u64::from(o));
    if last != names_len {
        return Err(corrupt(
            at_index(at, offsets.len().saturating_sub(1), 4),
            CorruptKind::InvalidValue {
                field: "last name offset (not names_len)",
                value: last,
            },
        ));
    }
    Ok(())
}

/// The 0 bytes of `names` are exactly the separators, the bytes before each
/// offset but the first. One pass over the zeros, in step with `offsets`.
fn check_separators(names: &[u8], offsets: &[u32], at: u64) -> Result<()> {
    let mut zeros = memchr::memchr_iter(0, names);
    for &end in offsets.iter().skip(1) {
        let Some(separator) = (end as usize).checked_sub(1) else {
            return Err(corrupt(
                at,
                CorruptKind::Other("name offset 0 after the first"),
            ));
        };
        match zeros.next() {
            Some(zero) if zero == separator => {}
            Some(zero) if zero < separator => return Err(zero_in_name(at, zero)),
            _ => {
                let value = names.get(separator).copied().unwrap_or(0);
                return Err(corrupt(
                    at_index(at, separator, 1),
                    CorruptKind::InvalidValue {
                        field: "name separator",
                        value: u64::from(value),
                    },
                ));
            }
        }
    }
    match zeros.next() {
        Some(zero) => Err(zero_in_name(at, zero)),
        None => Ok(()),
    }
}

fn zero_in_name(at: u64, zero: usize) -> Error {
    corrupt(
        at_index(at, zero, 1),
        CorruptKind::InvalidValue {
            field: "byte inside a name",
            value: 0,
        },
    )
}

/// Reserved bits clear, a valid kind class, the package bit on folders
/// only, and a folder at the root.
fn check_flags(flags: &[u16], at: u64) -> Result<()> {
    for (i, &word) in flags.iter().enumerate() {
        if word & RESERVED_FLAGS != 0 {
            return Err(corrupt(
                at_index(at, i, 2),
                CorruptKind::InvalidValue {
                    field: "flags (reserved bits)",
                    value: u64::from(word),
                },
            ));
        }
        let class = (word & IndexFlags::CLASS_MASK) >> IndexFlags::CLASS_SHIFT;
        if KindClass::from_bits(class).bits() != class {
            return Err(corrupt(
                at_index(at, i, 2),
                CorruptKind::InvalidValue {
                    field: "flags (kind class)",
                    value: u64::from(word),
                },
            ));
        }
        let parsed = IndexFlags::from_bits(word);
        if parsed.is_package() && !parsed.is_folder() {
            return Err(corrupt(
                at_index(at, i, 2),
                CorruptKind::InvalidValue {
                    field: "flags (package that is not a folder)",
                    value: u64::from(word),
                },
            ));
        }
    }
    if let Some(&root) = flags.first()
        && !IndexFlags::from_bits(root).is_folder()
    {
        return Err(corrupt(
            at,
            CorruptKind::InvalidValue {
                field: "root flags (not a folder)",
                value: u64::from(root),
            },
        ));
    }
    Ok(())
}

/// `parent[0] == 0`; every other parent comes before its child and is a
/// folder; the housekeeping bit of a parent is on its child too. `at` is the
/// offset of `parent`, `flags_at` that of `flags`.
fn check_parents(parent: &[u32], flags: &[u16], at: u64, flags_at: u64) -> Result<()> {
    if let Some(&root) = parent.first()
        && root != 0
    {
        return Err(corrupt(
            at,
            CorruptKind::InvalidValue {
                field: "root parent",
                value: u64::from(root),
            },
        ));
    }
    for (i, &p) in parent.iter().enumerate().skip(1) {
        let place = at_index(at, i, 4);
        if p as usize >= i {
            return Err(corrupt(
                place,
                CorruptKind::OutOfRange {
                    field: "parent",
                    value: u64::from(p),
                    limit_name: "entry number",
                    limit: u64::try_from(i).unwrap_or(u64::MAX),
                },
            ));
        }
        let parent_flags = flags.get(p as usize).map(|&w| IndexFlags::from_bits(w));
        if !parent_flags.is_some_and(IndexFlags::is_folder) {
            return Err(corrupt(
                place,
                CorruptKind::InvalidValue {
                    field: "parent (not a folder)",
                    value: u64::from(p),
                },
            ));
        }
        let own = flags.get(i).map(|&w| IndexFlags::from_bits(w));
        if parent_flags.is_some_and(IndexFlags::is_system_item)
            && !own.is_some_and(IndexFlags::is_system_item)
        {
            return Err(corrupt(
                at_index(flags_at, i, 2),
                CorruptKind::InvalidValue {
                    field: "flags (inside a housekeeping item, without its bit)",
                    value: own.map_or(0, |f| u64::from(f.bits())),
                },
            ));
        }
    }
    Ok(())
}

/// Reads exactly `len` bytes that start at file offset `at`. A short read
/// is `Truncated`; the buffer is allocated once at its final size.
fn read_section(input: &mut dyn Read, at: u64, len: u64) -> Result<Vec<u8>> {
    let Ok(capacity) = usize::try_from(len) else {
        return Err(corrupt(
            at,
            CorruptKind::LimitExceeded {
                what: "section length",
                limit: u64::try_from(usize::MAX).unwrap_or(u64::MAX),
            },
        ));
    };
    let mut buf = Vec::with_capacity(capacity);
    let got = Read::take(&mut *input, len)
        .read_to_end(&mut buf)
        .map_err(|source| {
            Error::io(
                IoContext::Path {
                    op: "reading the saved search index",
                    path: PathBuf::new(),
                },
                source,
            )
        })?;
    if got != capacity {
        let got = u64::try_from(got).unwrap_or(u64::MAX);
        let end = at.saturating_add(got);
        return Err(corrupt(
            end,
            CorruptKind::Truncated {
                needed: at.saturating_add(len),
                available: end,
            },
        ));
    }
    Ok(buf)
}

/// Reads `count` little-endian values of `N` bytes at file offset `at`. The
/// byte buffer is dropped as soon as the values are collected.
fn read_array<T, const N: usize>(
    input: &mut dyn Read,
    at: u64,
    count: u64,
    from: fn([u8; N]) -> T,
) -> Result<Box<[T]>> {
    let width = u64::try_from(N).unwrap_or(u64::MAX);
    let Some(len) = count.checked_mul(width) else {
        return Err(corrupt(
            at,
            CorruptKind::Overflow {
                field: "section length",
            },
        ));
    };
    let bytes = read_section(input, at, len)?;
    let (values, rest) = bytes.as_chunks::<N>();
    if !rest.is_empty() {
        return Err(corrupt(at, CorruptKind::Other("section length")));
    }
    Ok(values.iter().map(|&b| from(b)).collect())
}

/// Writes `values` as little-endian bytes, `WRITE_CHUNK` bytes at a time.
fn write_section<T: Copy, const N: usize>(
    out: &mut dyn Write,
    buf: &mut Vec<u8>,
    values: &[T],
    to: fn(T) -> [u8; N],
) -> Result<()> {
    let per_chunk = WRITE_CHUNK.checked_div(N).unwrap_or(WRITE_CHUNK).max(1);
    for chunk in values.chunks(per_chunk) {
        buf.clear();
        for &value in chunk {
            buf.extend_from_slice(&to(value));
        }
        write_all(out, buf)?;
    }
    Ok(())
}

fn write_all(out: &mut dyn Write, bytes: &[u8]) -> Result<()> {
    out.write_all(bytes).map_err(write_error)
}

fn write_error(source: std::io::Error) -> Error {
    Error::io(
        IoContext::Path {
            op: "writing the saved search index",
            path: PathBuf::new(),
        },
        source,
    )
}

/// File offset of element `i` of `width` bytes in a section at `at`; for
/// messages only, so it saturates.
fn at_index(at: u64, i: usize, width: u64) -> u64 {
    at.saturating_add(u64::try_from(i).unwrap_or(u64::MAX).saturating_mul(width))
}

fn corrupt(at: u64, kind: CorruptKind) -> Error {
    Error::Corrupt(Corruption::new(STRUCTURE, Location::Byte(at), kind))
}

#[cfg(test)]
mod tests {
    use std::io::{self, Cursor};
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::fs::NodeKind;
    use crate::search::Query;
    use crate::search::index::{MAX_RSIZE, NO_RSIZE};
    use crate::search::kind::KindSet;
    use crate::search::query::fixtures::{self, Rng, Spec, dir, file, link, root};

    const ID: VolumeIdentity = VolumeIdentity {
        id: [0x11; 16],
        change: [0x22; 16],
    };
    const SUMMARY: ScanSummary = ScanSummary {
        entries: 123_456,
        skipped: 7,
    };

    /// A copy of `index` with [`ID`] and [`SUMMARY`].
    fn with_identity(index: &Index) -> Index {
        Index::from_parts(IndexParts {
            names: std::str::from_utf8(index.names_bytes()).unwrap().into(),
            offsets: index.offsets().into(),
            parent: index.parents().into(),
            node: index.nodes().into(),
            size: index.sizes().into(),
            modified: index.modified_times().into(),
            rsize: index.rsizes().into(),
            flags: index.flag_words().into(),
            identity: Some(ID),
            summary: SUMMARY,
        })
    }

    /// Fifty entries: folders, files, a link, an "other", hidden entries,
    /// an empty name, a NUL in a name, non-ASCII names and the sentinel
    /// neighbours.
    fn small_specs() -> Vec<Spec> {
        let mut specs = vec![
            root("Macintosh HD"),
            dir("Applications", 0),
            dir("Mail.app", 1),
            file("Info.plist", 2)
                .size(1234)
                .time(1_600_000_000_000_000_000),
            file("", 1),
            dir("Users", 0),
            dir("julian", 5),
            file("caf\u{e9} notes.txt", 6).hidden(),
            file(
                "\u{3a3}\u{3b7}\u{3bc}\u{3b5}\u{3af}\u{3c9}\u{3c3}\u{3b7}.pdf",
                6,
            )
            .rsize(4096),
            link("link to Users", 6).links_to_folder(),
            file("zero\0name.bin", 6),
            file("big.dmg", 6).size(u64::MAX - 1).time(i64::MIN + 1),
            file("negative time.txt", 6).time(-1),
            file("no size", 6).no_size().rsize(MAX_RSIZE),
            Spec {
                kind: NodeKind::Other,
                ..file("fifo", 6)
            },
        ];
        let mut folder = 5u32;
        for k in 0..35u32 {
            let entry = specs.len() as u32;
            if k % 6 == 0 {
                specs.push(dir(&format!("Folder {k}"), 5).time(i64::from(k)));
                folder = entry;
            } else {
                let ext = fixtures::EXTS[k as usize % fixtures::EXTS.len()];
                let mut spec = file(&format!("photo {k}.{ext}"), folder).size(u64::from(k) * 1000);
                if k % 5 == 0 {
                    spec = spec.hidden();
                }
                specs.push(spec);
            }
        }
        assert_eq!(specs.len(), 50);
        specs
    }

    fn small_index() -> Index {
        with_identity(&fixtures::index(&small_specs()))
    }

    fn saved(index: &Index) -> Vec<u8> {
        let mut out = Vec::new();
        save(index, &mut out).unwrap();
        out
    }

    fn load_bytes(bytes: &[u8]) -> Result<Option<Index>> {
        load(&mut Cursor::new(bytes), bytes.len() as u64, ID)
    }

    fn get_u32(bytes: &[u8], at: u64) -> u32 {
        let at = at as usize;
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    fn set_u32(bytes: &mut [u8], at: u64, value: u32) {
        let at = at as usize;
        bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn set_u64(bytes: &mut [u8], at: u64, value: u64) {
        let at = at as usize;
        bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn layout_of(bytes: &[u8]) -> Layout {
        let entries = get_u32(bytes, AT_ENTRIES);
        let names_len = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
        Layout::new(entries, names_len).unwrap()
    }

    fn assert_corrupt(result: Result<Option<Index>>) -> Corruption {
        match result {
            Err(Error::Corrupt(c)) => {
                assert_eq!(c.structure, STRUCTURE);
                c
            }
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    fn assert_corrupt_at(result: Result<Option<Index>>, at: u64) -> CorruptKind {
        let c = assert_corrupt(result);
        assert_eq!(c.location, Location::Byte(at), "{c}");
        c.kind
    }

    fn queries() -> Vec<Query> {
        let text = |t: &str| Query {
            text: t.to_owned(),
            ..Query::default()
        };
        vec![
            text("a"),
            text("photo"),
            text("caf"),
            text("\u{3c3}\u{3b7}\u{3bc}"),
            text("txt"),
            text(fixtures::RARE),
            text("photo 1"),
            Query {
                include_hidden: true,
                ..text("o")
            },
            Query {
                size: Some(1000..=1 << 31),
                ..Query::default()
            },
            Query {
                modified: Some(0..=i64::MAX),
                kinds: KindSet::default().with(KindClass::Folder),
                ..Query::default()
            },
        ]
    }

    fn assert_same(a: &Index, b: &Index) {
        assert_eq!(a.len(), b.len());
        assert_eq!(a.names_bytes(), b.names_bytes());
        assert_eq!(a.folded_bytes(), b.folded_bytes());
        assert_eq!(a.offsets(), b.offsets());
        assert_eq!(a.parents(), b.parents());
        assert_eq!(a.nodes(), b.nodes());
        assert_eq!(a.sizes(), b.sizes());
        assert_eq!(a.modified_times(), b.modified_times());
        assert_eq!(a.rsizes(), b.rsizes());
        assert_eq!(a.flag_words(), b.flag_words());
        assert_eq!(a.identity(), b.identity());
        assert_eq!(a.summary(), b.summary());
        for i in 0..a.len() {
            assert_eq!(a.name(i), b.name(i));
            assert_eq!(a.entry(i), b.entry(i));
            assert_eq!(a.flags(i), b.flags(i));
            assert_eq!(a.parent(i), b.parent(i));
            assert_eq!(a.subtree(i), b.subtree(i));
        }
        let cancel = AtomicBool::new(false);
        for query in queries() {
            let ha = a.search(&query, None, &cancel).unwrap();
            let hb = b.search(&query, None, &cancel).unwrap();
            assert_eq!(ha.entries_in_order(), hb.entries_in_order(), "{query:?}");
        }
    }

    #[test]
    fn round_trip_small() {
        let index = small_index();
        let bytes = saved(&index);
        assert_eq!(bytes.len() as u64, layout_of(&bytes).end);
        assert_eq!(&bytes[..8], b"DATLSIDX");
        let loaded = load_bytes(&bytes).unwrap().unwrap();
        assert_same(&index, &loaded);
        assert_eq!(loaded.name(4), "");
        assert_eq!(loaded.name(10), "zero\u{1a}name.bin");
        assert_eq!(loaded.sizes()[11], u64::MAX - 1);
        assert_eq!(loaded.rsizes()[13], MAX_RSIZE);
        assert_eq!(loaded.rsizes()[3], NO_RSIZE);
        assert_eq!(loaded.identity(), Some(ID));
        assert_eq!(loaded.summary(), SUMMARY);
        // Saving the loaded index gives the same bytes.
        assert_eq!(saved(&loaded), bytes);
    }

    #[test]
    fn round_trip_generated() {
        let index = with_identity(&fixtures::index(&fixtures::generated(10_000, 0xD15C)));
        let bytes = saved(&index);
        let loaded = load_bytes(&bytes).unwrap().unwrap();
        assert_same(&index, &loaded);
    }

    #[test]
    fn round_trip_new_bits() {
        // Housekeeping bit on the Users subtree, a package, every label.
        let base = small_index();
        let users = base.subtree(5);
        let mut flags: Vec<u16> = base.flag_words().to_vec();
        for (i, w) in flags.iter_mut().enumerate() {
            let i = i as u32;
            if users.contains(&i) {
                *w |= IndexFlags::SYSTEM_ITEM;
            }
            *w |= ((i % 8) as u16) << IndexFlags::LABEL_SHIFT;
        }
        flags[2] |= IndexFlags::PACKAGE;
        let index = Index::from_parts(IndexParts {
            names: std::str::from_utf8(base.names_bytes()).unwrap().into(),
            offsets: base.offsets().into(),
            parent: base.parents().into(),
            node: base.nodes().into(),
            size: base.sizes().into(),
            modified: base.modified_times().into(),
            rsize: base.rsizes().into(),
            flags: flags.into(),
            identity: Some(ID),
            summary: SUMMARY,
        });
        let bytes = saved(&index);
        let loaded = load_bytes(&bytes).unwrap().unwrap();
        assert_same(&index, &loaded);
        assert!(loaded.flags(2).unwrap().is_package());
        assert!(loaded.flags(49).unwrap().is_system_item());
        assert!(!loaded.flags(1).unwrap().is_system_item());
        assert_eq!(loaded.flags(7).unwrap().label(), 7);
        assert_eq!(loaded.entry(7).unwrap().flags.label(), 7);
    }

    #[test]
    fn version_1_is_refused() {
        // A saved index of the previous format: the same bytes with the
        // version field written as 1. Refused, not an error: the caller
        // rebuilds once.
        let mut bytes = saved(&small_index());
        set_u32(&mut bytes, AT_VERSION, 1);
        assert_eq!(&bytes[8..12], &[1, 0, 0, 0]);
        assert!(load_bytes(&bytes).unwrap().is_none());
        // Even when the rest of the file would be wrong for version 2.
        let mut truncated = bytes.clone();
        truncated.truncate(100);
        assert!(load_bytes(&truncated).unwrap().is_none());
        let mut header_only = bytes[..80].to_vec();
        header_only[79] ^= 0xFF;
        assert!(load_bytes(&header_only).unwrap().is_none());
    }

    #[test]
    fn root_only_round_trip() {
        let index = with_identity(&fixtures::index(&[root("")]));
        let bytes = saved(&index);
        assert_eq!(bytes.len(), 80 + 1 + 38 + 4);
        let loaded = load_bytes(&bytes).unwrap().unwrap();
        assert_same(&index, &loaded);
    }

    #[test]
    fn not_ours_is_none() {
        let bytes = saved(&small_index());
        let mut magic = bytes.clone();
        magic[7] = b'Y';
        assert!(load_bytes(&magic).unwrap().is_none());
        assert_eq!(get_u32(&bytes, AT_VERSION), 2);
        for version in [0, 1, 3, u32::MAX] {
            let mut b = bytes.clone();
            set_u32(&mut b, AT_VERSION, version);
            assert!(load_bytes(&b).unwrap().is_none());
        }
        let mut other = ID;
        other.id[15] ^= 1;
        assert!(
            load(&mut Cursor::new(&bytes), bytes.len() as u64, other)
                .unwrap()
                .is_none()
        );
        let mut other = ID;
        other.change[0] ^= 1;
        assert!(
            load(&mut Cursor::new(&bytes), bytes.len() as u64, other)
                .unwrap()
                .is_none()
        );
        for len in 0..8 {
            assert!(load_bytes(&bytes[..len]).unwrap().is_none());
            assert!(load_bytes(&vec![0xAB; len]).unwrap().is_none());
        }
        // Another magic is None whatever the length.
        assert!(load_bytes(&[0u8; 40]).unwrap().is_none());
    }

    #[test]
    fn header_cut_after_magic() {
        let bytes = saved(&small_index());
        for len in 8..80 {
            let kind = assert_corrupt_at(load_bytes(&bytes[..len]), len as u64);
            assert!(matches!(kind, CorruptKind::Truncated { needed: 80, .. }));
        }
    }

    #[test]
    fn file_len_field_must_match_real_length() {
        let bytes = saved(&small_index());
        let real = bytes.len() as u64;
        for claimed in [real + 1, real - 1, 0, u64::MAX] {
            let mut b = bytes.clone();
            set_u64(&mut b, AT_FILE_LEN, claimed);
            assert_corrupt_at(load_bytes(&b), AT_FILE_LEN);
        }
        // A caller length that disagrees, however large, allocates nothing.
        assert_corrupt_at(load(&mut Cursor::new(&bytes), u64::MAX, ID), AT_FILE_LEN);
    }

    #[test]
    fn real_length_must_match_formula() {
        let bytes = saved(&small_index());
        let real = bytes.len() as u64;
        // An extra trailing byte, with the header agreeing.
        let mut longer = bytes.clone();
        longer.push(0);
        set_u64(&mut longer, AT_FILE_LEN, real + 1);
        let kind = assert_corrupt_at(load_bytes(&longer), real);
        assert!(matches!(kind, CorruptKind::OutOfRange { .. }), "{kind:?}");
        // A missing byte.
        let mut shorter = bytes.clone();
        shorter.pop();
        set_u64(&mut shorter, AT_FILE_LEN, real - 1);
        let kind = assert_corrupt_at(load_bytes(&shorter), real - 1);
        assert!(matches!(kind, CorruptKind::Truncated { .. }), "{kind:?}");
    }

    #[test]
    fn entry_count_rules() {
        let bytes = saved(&small_index());
        let real = bytes.len() as u64;
        let mut zero = bytes.clone();
        set_u32(&mut zero, AT_ENTRIES, 0);
        assert_corrupt_at(load_bytes(&zero), AT_ENTRIES);

        let mut one_more = bytes.clone();
        set_u32(&mut one_more, AT_ENTRIES, 51);
        let kind = assert_corrupt_at(load_bytes(&one_more), real);
        assert!(matches!(kind, CorruptKind::Truncated { .. }));

        let mut too_many = bytes.clone();
        set_u32(&mut too_many, AT_ENTRIES, u32::MAX);
        let kind = assert_corrupt_at(load_bytes(&too_many), AT_ENTRIES);
        assert!(matches!(kind, CorruptKind::OutOfRange { .. }));
    }

    #[test]
    fn names_len_rules() {
        let bytes = saved(&small_index());
        let real = bytes.len() as u64;
        let names_len = layout_of(&bytes).offsets - 80;
        let mut longer = bytes.clone();
        set_u64(&mut longer, AT_NAMES_LEN, names_len + 1);
        assert_corrupt_at(load_bytes(&longer), real);
        let mut shorter = bytes.clone();
        set_u64(&mut shorter, AT_NAMES_LEN, names_len - 1);
        assert_corrupt_at(load_bytes(&shorter), real - 1);
        for huge in [u64::from(u32::MAX) + 1, u64::MAX] {
            let mut b = bytes.clone();
            set_u64(&mut b, AT_NAMES_LEN, huge);
            assert_corrupt_at(load_bytes(&b), AT_NAMES_LEN);
        }
    }

    #[test]
    fn huge_counts_do_not_allocate() {
        // Near the limits the formula is about 163 GB; allocating it would
        // abort the test process.
        let bytes = saved(&small_index());
        let real = bytes.len() as u64;
        let mut b = bytes.clone();
        set_u32(&mut b, AT_ENTRIES, MAX_ENTRIES);
        set_u64(&mut b, AT_NAMES_LEN, u64::from(u32::MAX));
        assert_corrupt_at(load_bytes(&b), real);
        // Header lengths agreeing with the formula still disagree with the
        // real length.
        let formula = Layout::new(MAX_ENTRIES, u64::from(u32::MAX)).unwrap().end;
        set_u64(&mut b, AT_FILE_LEN, formula);
        assert_corrupt_at(load_bytes(&b), AT_FILE_LEN);
        set_u32(&mut b, AT_ENTRIES, u32::MAX);
        set_u64(&mut b, AT_NAMES_LEN, u64::MAX);
        set_u64(&mut b, AT_FILE_LEN, real);
        assert_corrupt_at(load_bytes(&b), AT_ENTRIES);
        // The layout itself refuses to overflow.
        assert!(Layout::new(u32::MAX, u64::MAX).is_none());
        assert!(Layout::new(0, u64::MAX - 79).is_none());
    }

    #[test]
    fn offset_rules() {
        let bytes = saved(&small_index());
        let layout = layout_of(&bytes);
        let names_len = layout.offsets - 80;
        let at = |k: u64| layout.offsets + 4 * k;
        let o1 = get_u32(&bytes, at(1));

        let mut first = bytes.clone();
        set_u32(&mut first, at(0), 1);
        assert_corrupt_at(load_bytes(&first), at(0));

        let mut descending = bytes.clone();
        set_u32(&mut descending, at(2), o1 - 1);
        assert_corrupt_at(load_bytes(&descending), at(2));

        let mut equal = bytes.clone();
        set_u32(&mut equal, at(2), o1);
        assert_corrupt_at(load_bytes(&equal), at(2));

        let mut last = bytes.clone();
        set_u32(&mut last, at(50), names_len as u32 - 1);
        assert_corrupt_at(load_bytes(&last), at(50));

        let mut past = bytes.clone();
        set_u32(&mut past, at(50), u32::MAX);
        assert_corrupt_at(load_bytes(&past), at(50));
    }

    #[test]
    fn name_byte_rules() {
        let bytes = saved(&small_index());
        let layout = layout_of(&bytes);
        // Name 1 is "Applications", from o1 to o2 - 1.
        let o1 = u64::from(get_u32(&bytes, layout.offsets + 4));
        let o2 = u64::from(get_u32(&bytes, layout.offsets + 8));

        let mut no_separator = bytes.clone();
        no_separator[(80 + o1 - 1) as usize] = b'x';
        assert_corrupt_at(load_bytes(&no_separator), 80 + o1 - 1);

        let mut zero_inside = bytes.clone();
        zero_inside[(80 + o1 + 3) as usize] = 0;
        assert_corrupt_at(load_bytes(&zero_inside), 80 + o1 + 3);

        // A zero inside the last name, past every separator but its own.
        let mut zero_last = bytes.clone();
        let names_end = layout.offsets;
        zero_last[(names_end - 2) as usize] = 0;
        assert_corrupt_at(load_bytes(&zero_last), names_end - 2);

        for bad in [0xFF, 0x80, 0xC3] {
            let mut not_utf8 = bytes.clone();
            not_utf8[(80 + o2 - 2) as usize] = bad;
            let kind = assert_corrupt_at(load_bytes(&not_utf8), 80 + o2 - 2);
            assert!(matches!(kind, CorruptKind::InvalidValue { .. }));
        }
    }

    #[test]
    fn parent_rules() {
        let bytes = saved(&small_index());
        let layout = layout_of(&bytes);
        let at = |i: u64| layout.parent + 4 * i;

        let mut root = bytes.clone();
        set_u32(&mut root, at(0), 1);
        assert_corrupt_at(load_bytes(&root), at(0));

        for bad in [3, 4, u32::MAX] {
            let mut forward = bytes.clone();
            set_u32(&mut forward, at(3), bad);
            assert_corrupt_at(load_bytes(&forward), at(3));
        }

        // Entry 3 is a file; entry 4 cannot be inside it.
        let mut not_folder = bytes.clone();
        set_u32(&mut not_folder, at(4), 3);
        assert_corrupt_at(load_bytes(&not_folder), at(4));
        // Nor inside the link to a folder (entry 9).
        let mut link_parent = bytes.clone();
        set_u32(&mut link_parent, at(10), 9);
        assert_corrupt_at(load_bytes(&link_parent), at(10));
    }

    #[test]
    fn flag_rules() {
        let bytes = saved(&small_index());
        let layout = layout_of(&bytes);
        let at = |i: u64| (layout.flags + 2 * i) as usize;
        let word = |b: &[u8], i: u64| u16::from_le_bytes([b[at(i)], b[at(i) + 1]]);
        let set =
            |b: &mut [u8], i: u64, w: u16| b[at(i)..at(i) + 2].copy_from_slice(&w.to_le_bytes());

        // Bit 15 is reserved.
        let mut b = bytes.clone();
        let w = word(&b, 3) | 1 << 15;
        set(&mut b, 3, w);
        assert_corrupt_at(load_bytes(&b), at(3) as u64);
        // Any label loads.
        for label in 1..=7u16 {
            let mut b = bytes.clone();
            let w = word(&b, 3) | label << IndexFlags::LABEL_SHIFT;
            set(&mut b, 3, w);
            let loaded = load_bytes(&b).unwrap().unwrap();
            assert_eq!(loaded.flags(3).unwrap().label(), label as u8);
        }
        // The package bit only on a folder: entry 3 is a file, 2 a folder.
        let mut b = bytes.clone();
        let w = word(&b, 3) | IndexFlags::PACKAGE;
        set(&mut b, 3, w);
        assert_corrupt_at(load_bytes(&b), at(3) as u64);
        let mut b = bytes.clone();
        let w = word(&b, 2) | IndexFlags::PACKAGE;
        set(&mut b, 2, w);
        assert!(
            load_bytes(&b)
                .unwrap()
                .unwrap()
                .flags(2)
                .unwrap()
                .is_package()
        );
        // The housekeeping bit: alone on a file is fine; on a folder whose
        // child lacks it is not (entry 3 is the only child of entry 2).
        let mut b = bytes.clone();
        let w = word(&b, 3) | IndexFlags::SYSTEM_ITEM;
        set(&mut b, 3, w);
        assert!(load_bytes(&b).unwrap().is_some());
        let mut b = bytes.clone();
        let w = word(&b, 2) | IndexFlags::SYSTEM_ITEM;
        set(&mut b, 2, w);
        assert_corrupt_at(load_bytes(&b), at(3) as u64);
        let w = word(&b, 3) | IndexFlags::SYSTEM_ITEM;
        set(&mut b, 3, w);
        assert!(load_bytes(&b).unwrap().is_some());
        for class in [12u16, 13, 15] {
            let mut b = bytes.clone();
            let w = (word(&b, 3) & !IndexFlags::CLASS_MASK) | class << IndexFlags::CLASS_SHIFT;
            set(&mut b, 3, w);
            assert_corrupt_at(load_bytes(&b), at(3) as u64);
        }
        // Every valid class loads.
        for class in KindClass::ALL {
            let mut b = bytes.clone();
            let w =
                (word(&b, 3) & !IndexFlags::CLASS_MASK) | class.bits() << IndexFlags::CLASS_SHIFT;
            set(&mut b, 3, w);
            assert!(load_bytes(&b).unwrap().is_some());
        }
        let mut root_file = bytes.clone();
        let w = word(&root_file, 0) & !IndexFlags::KIND_MASK;
        set(&mut root_file, 0, w);
        assert_corrupt_at(load_bytes(&root_file), at(0) as u64);
    }

    #[test]
    fn value_arrays_take_any_value() {
        let bytes = saved(&small_index());
        let layout = layout_of(&bytes);
        let mut b = bytes.clone();
        for i in 0..50 {
            set_u64(&mut b, layout.node + 8 * i, u64::MAX - i);
            set_u64(&mut b, layout.size + 8 * i, u64::MAX);
            set_u64(&mut b, layout.modified + 8 * i, i64::MIN as u64);
            set_u32(&mut b, layout.rsize + 4 * i, u32::MAX);
        }
        let loaded = load_bytes(&b).unwrap().unwrap();
        assert!(loaded.sizes().iter().all(|&s| s == u64::MAX));
        assert_eq!(loaded.nodes()[1], u64::MAX - 1);
    }

    /// Hands out one byte per `read`.
    struct OneByte<'a>(&'a [u8]);

    impl Read for OneByte<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match (self.0.split_first(), buf.first_mut()) {
                (Some((&b, rest)), Some(slot)) => {
                    *slot = b;
                    self.0 = rest;
                    Ok(1)
                }
                _ => Ok(0),
            }
        }
    }

    #[test]
    fn short_reads_still_load() {
        let index = small_index();
        let bytes = saved(&index);
        let loaded = load(&mut OneByte(&bytes), bytes.len() as u64, ID)
            .unwrap()
            .unwrap();
        assert_same(&index, &loaded);
    }

    #[test]
    fn input_shorter_than_its_length_is_truncated() {
        let bytes = saved(&small_index());
        let real = bytes.len() as u64;
        for cut in [100, (real / 2) as usize, bytes.len() - 1] {
            let c = assert_corrupt(load(&mut Cursor::new(&bytes[..cut]), real, ID));
            assert!(matches!(c.kind, CorruptKind::Truncated { .. }), "{c}");
            assert_eq!(c.location, Location::Byte(cut as u64));
        }
    }

    struct Failing;

    impl Read for Failing {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("device gone"))
        }
    }

    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("disk full"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn io_errors_are_io() {
        assert!(matches!(
            load(&mut Failing, 1000, ID),
            Err(Error::Io { .. })
        ));
        assert!(matches!(
            save(&small_index(), &mut Failing),
            Err(Error::Io { .. })
        ));
    }

    #[test]
    fn save_without_identity_is_refused() {
        let index = fixtures::index(&small_specs());
        assert_eq!(index.identity(), None);
        let mut out = Vec::new();
        assert!(matches!(save(&index, &mut out), Err(Error::Unsupported(_))));
        assert!(out.is_empty());
    }

    /// Runs every accessor and the test queries over a loaded index.
    fn exercise(index: &Index) {
        assert_eq!(index.verify(), Ok(()));
        let mut path = String::new();
        for i in 0..index.len() {
            let _ = index.entry(i);
            let _ = index.subtree(i);
            let _ = index.crumbs(i);
            path.clear();
            index.folder_path(i, "/", &mut path);
        }
        let cancel = AtomicBool::new(false);
        for query in queries() {
            let _ = index.search(&query, None, &cancel);
        }
    }

    #[test]
    fn random_patches_never_panic() {
        let bytes = saved(&small_index());
        let layout = layout_of(&bytes);
        let mut rng = Rng(0x5EED);
        let (mut loaded, mut none, mut corrupt) = (0, 0, 0);
        for round in 0..600 {
            let mut b = bytes.clone();
            let mut file_len = b.len() as u64;
            match round % 5 {
                // A few bytes anywhere.
                0 => {
                    for _ in 0..=rng.below(4) {
                        let at = rng.below(b.len() as u64) as usize;
                        b[at] ^= 1 + rng.below(255) as u8;
                    }
                }
                // One bit of a count or length in the header.
                1 => {
                    let at = *rng.pick(&[12, 13, 14, 15, 16, 17, 18, 19, 23, 72, 73, 79]);
                    b[at] ^= 1 << rng.below(8);
                }
                // A random word in the arrays.
                2 => {
                    let span = layout.end - layout.offsets - 4;
                    let at = layout.offsets + rng.below(span);
                    set_u32(&mut b, at, rng.next() as u32);
                }
                // A small value in an offset, parent or flag slot.
                3 => {
                    let (start, width) = *rng.pick(&[
                        (layout.offsets, 4u64),
                        (layout.parent, 4),
                        (layout.flags, 2),
                    ]);
                    let at = (start + width * rng.below(50)) as usize;
                    b[at] = rng.below(64) as u8;
                }
                // Cut, passing the real or the original length.
                _ => {
                    let cut = rng.below(b.len() as u64) as usize;
                    b.truncate(cut);
                    if rng.below(2) == 0 {
                        file_len = cut as u64;
                    }
                }
            }
            match load(&mut Cursor::new(&b), file_len, ID) {
                Ok(Some(index)) => {
                    exercise(&index);
                    loaded += 1;
                }
                Ok(None) => none += 1,
                Err(Error::Corrupt(c)) => {
                    assert_eq!(c.structure, STRUCTURE);
                    corrupt += 1;
                }
                Err(e) => panic!("unexpected error {e:?}"),
            }
        }
        assert!(
            loaded > 0 && none > 0 && corrupt > 0,
            "{loaded} {none} {corrupt}"
        );
    }

    /// Deletes the file when dropped, pass or fail.
    struct TempFile(std::path::PathBuf);

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    #[ignore = "measurement; run in release with --nocapture"]
    #[allow(clippy::print_stdout)]
    fn index_load_time() {
        let built = Instant::now();
        let specs = fixtures::generated(1_000_000, 0x00D1_5CA7);
        let index = with_identity(&fixtures::index(&specs));
        drop(specs);
        let built = built.elapsed();
        let temp = TempFile(
            std::env::temp_dir().join(format!("disk-atlas-index-load-{}.idx", std::process::id())),
        );
        let saving = Instant::now();
        {
            let mut out = io::BufWriter::new(std::fs::File::create(&temp.0).unwrap());
            save(&index, &mut out).unwrap();
        }
        let saving = saving.elapsed();
        let file_len = std::fs::metadata(&temp.0).unwrap().len();
        let load_once = || -> (Duration, Index) {
            let started = Instant::now();
            let mut f = std::fs::File::open(&temp.0).unwrap();
            let len = f.metadata().unwrap().len();
            let loaded = load(&mut f, len, ID).unwrap().unwrap();
            (started.elapsed(), loaded)
        };
        let (first, loaded) = load_once();
        assert_same(&index, &loaded);
        drop(loaded);
        let mut times: Vec<Duration> = (0..5)
            .map(|_| {
                let (t, loaded) = load_once();
                drop(loaded);
                t
            })
            .collect();
        times.sort();
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        println!(
            "index_load_time: {} entries, file {:.1} MB, first {:.1} ms, median of 5 {:.1} ms \
             (min {:.1}, max {:.1}), budget 200 ms; save {:.1} ms, fixture build {:.0} ms",
            index.len(),
            file_len as f64 / 1e6,
            ms(first),
            ms(times[2]),
            ms(times[0]),
            ms(times[4]),
            ms(saving),
            ms(built),
        );
    }
}
