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

//! Reader for `.DS_Store`, the Finder's notes on one folder: view, icon
//! places, window, comments, put-back places. Source: `r10-everyday.md`
//! section 5.3, from Wim Lewis's format notes (`DSStoreFormat.pod` of
//! `Mac::Finder::DSStore`); every integer is big-endian.
//!
//! The file is a buddy allocator holding a B-tree. Offsets count from byte 4:
//!
//! ```text
//! 0   u32 1, "Bud1", root block offset, size, offset again (must match), 16 unknown
//! root block: block count u32, 4 unknown, the address table (count u32, padded to a
//!   multiple of 256 entries), then trees: count u32 and per tree name length u8,
//!   name, block number u32 ("DSDB"), then 32 free lists (ignored)
//! address: offset = address with the low 5 bits cleared, size = 1 << (address & 31)
//! DSDB block: root node u32, levels u32, records u32, nodes u32, page size u32
//! node: rightmost child u32 (0: a leaf), count u32, then records, each preceded
//!   by a child block u32 when the node is not a leaf
//! record: name length u32 (UTF-16 units), name UTF-16BE, code[4], type[4], value
//! ```
//!
//! Types: `bool` 1 byte, `long` and `shor` 4, `type` 4, `comp` and `dutc` 8,
//! `blob` u32 length and bytes, `ustr` u32 units and UTF-16BE. The folder's
//! own records are under the name `.`.
//!
//! **Not checked against a real file in this build:** the put-back codes
//! `ptbL` and `ptbN` (both `ustr` in the notes) must be verified on the
//! `.DS_Store` of a real `.Trashes/501` before they are relied on.

use std::collections::HashSet;

use crate::error::{CorruptKind, Result};
use crate::image::plist::Value as Plist;

use super::bplist;
use super::util::{be32, corrupt, corrupt_text, len64, to_usize, truncated};

const WHAT: &str = ".DS_Store";

/// Largest file read.
pub const MAX_DS_STORE: usize = 16 << 20;
/// Most blocks in the address table.
pub const MAX_BLOCKS: usize = 65_536;
/// Most records kept.
pub const MAX_DS_RECORDS: usize = 100_000;
/// Deepest tree.
pub const MAX_DEPTH: usize = 16;
/// Longest blob kept.
pub const MAX_BLOB: usize = 1 << 20;
/// Longest record name, in UTF-16 units.
pub const MAX_NAME_UNITS: usize = 255;
/// Most trees named in the allocator directory.
const MAX_TREES: usize = 1024;

/// The value of one record.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `bool`.
    Bool(bool),
    /// `long`.
    Long(u32),
    /// `shor`: the last two of four bytes.
    Short(u16),
    /// `type`: a four-character code.
    Type([u8; 4]),
    /// `comp`.
    Comp(u64),
    /// `dutc`: 1/65536 s since 1904-01-01.
    Dutc(u64),
    /// `blob`.
    Blob(Vec<u8>),
    /// `ustr`.
    Text(String),
}

/// One record: a name, a code, a value.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    /// The item the record is about; `.` for the folder.
    pub name: String,
    /// The four-character code.
    pub code: [u8; 4],
    /// The value.
    pub value: Value,
}

/// How a folder is viewed (`vstl`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewStyle {
    /// `icnv`.
    Icon,
    /// `Nlsv`.
    List,
    /// `clmv`.
    Column,
    /// `Flwv`.
    Flow,
    /// `glyv`.
    Gallery,
}

/// What the background of an icon view is.
#[derive(Debug, Clone, PartialEq)]
pub enum Background {
    /// The default.
    Default,
    /// A colour, each part 0.0 to 1.0.
    Colour(f64, f64, f64),
    /// A picture (named by an alias Disk Atlas does not resolve).
    Picture,
}

/// The icon view's settings (`icvp`).
#[derive(Debug, Clone, PartialEq)]
pub struct IconView {
    /// The icon size in points, clamped to 16..=512.
    pub icon_size: Option<f64>,
    /// The background.
    pub background: Background,
}

/// The column a list view is sorted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListSortColumn {
    /// Name.
    Name,
    /// Date modified.
    DateModified,
    /// Date created.
    DateCreated,
    /// Size.
    Size,
    /// Kind.
    Kind,
    /// Label.
    Label,
}

/// Which type each known code must have; a record of another type is dropped.
fn expected_type(code: [u8; 4]) -> Option<&'static [u8; 4]> {
    Some(match &code {
        b"cmmt" | b"ptbL" | b"ptbN" => b"ustr",
        b"Iloc" | b"icvp" | b"bwsp" | b"lsvp" | b"lsvP" | b"BKGD" => b"blob",
        b"vstl" => b"type",
        _ => return None,
    })
}

/// The records of one `.DS_Store`, sorted by (name without regard to case, code).
#[derive(Debug, Clone, Default)]
pub struct DsStore {
    records: Vec<(String, Record)>,
}

struct Walk<'a> {
    bytes: &'a [u8],
    /// Offset and size of each block, `None` for an unused entry.
    blocks: Vec<Option<(usize, usize)>>,
    visited: HashSet<u32>,
    records: Vec<(String, Record)>,
}

impl DsStore {
    /// Parses the bytes of a `.DS_Store`. A damaged structure is an error; a record whose type
    /// does not fit its code is left out and the rest are kept.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_DS_STORE {
            return Err(corrupt(
                WHAT,
                0,
                CorruptKind::OutOfRange {
                    field: "size",
                    value: len64(bytes),
                    limit_name: "largest file",
                    limit: MAX_DS_STORE as u64,
                },
            ));
        }
        if be32(bytes, 0) != Some(1) || bytes.get(4..8) != Some(b"Bud1") {
            return Err(corrupt(
                WHAT,
                0,
                CorruptKind::BadSignature {
                    expected: u64::from(u32::from_be_bytes(*b"Bud1")),
                    found: u64::from(be32(bytes, 4).unwrap_or(0)),
                },
            ));
        }
        let (Some(root_off), Some(root_size), Some(again)) =
            (be32(bytes, 8), be32(bytes, 12), be32(bytes, 16))
        else {
            return Err(truncated(WHAT, 8, 12, len64(bytes)));
        };
        if again != root_off {
            return Err(corrupt(
                WHAT,
                16,
                CorruptKind::InvalidValue {
                    field: "root block offset",
                    value: u64::from(again),
                },
            ));
        }
        let root = slice_at(bytes, root_off, to_usize(u64::from(root_size)))?;
        let count = to_usize(u64::from(be32(root, 0).unwrap_or(0)));
        if count > MAX_BLOCKS {
            return Err(corrupt(
                WHAT,
                u64::from(root_off),
                CorruptKind::OutOfRange {
                    field: "block count",
                    value: count as u64,
                    limit_name: "most blocks",
                    limit: MAX_BLOCKS as u64,
                },
            ));
        }
        let mut blocks = Vec::with_capacity(count);
        for index in 0..count {
            let address = be32(root, index.saturating_mul(4).saturating_add(8))
                .ok_or_else(|| truncated(WHAT, u64::from(root_off), 8, len64(root)))?;
            blocks.push(block_of(bytes, address)?);
        }
        // The table is padded to a multiple of 256 entries.
        let padded = count.div_ceil(256).saturating_mul(256);
        let mut at = padded.saturating_mul(4).saturating_add(8);
        let trees = to_usize(u64::from(be32(root, at).unwrap_or(0)));
        if trees > MAX_TREES {
            return Err(corrupt_text(WHAT, "too many trees in the directory"));
        }
        at = at.saturating_add(4);
        let mut dsdb = None;
        for _ in 0..trees {
            let len = usize::from(root.get(at).copied().unwrap_or(0));
            let name = root.get(at.saturating_add(1)..).and_then(|rest| rest.get(..len));
            let number = be32(root, at.saturating_add(1).saturating_add(len));
            let (Some(name), Some(number)) = (name, number) else {
                return Err(truncated(WHAT, u64::from(root_off), 5, len64(root)));
            };
            if name == b"DSDB" {
                dsdb = Some(number);
            }
            at = at.saturating_add(len).saturating_add(5);
        }
        let Some(dsdb) = dsdb else {
            return Err(corrupt_text(WHAT, "no DSDB tree"));
        };
        let mut walk = Walk {
            bytes,
            blocks,
            visited: HashSet::new(),
            records: Vec::new(),
        };
        let info = walk.block(dsdb)?;
        let root_node = be32(info, 0).ok_or_else(|| truncated(WHAT, 0, 20, len64(info)))?;
        walk.node(root_node, 1)?;
        let mut records = walk.records;
        records.sort_by(|a, b| (&a.0, a.1.code).cmp(&(&b.0, b.1.code)));
        Ok(Self { records })
    }

    /// Every record.
    pub fn records(&self) -> impl Iterator<Item = &Record> {
        self.records.iter().map(|(_, record)| record)
    }

    /// The record `code` of `name`, compared without regard to case.
    fn get(&self, name: &str, code: &[u8; 4]) -> Option<&Value> {
        let key = name.to_lowercase();
        let at = self
            .records
            .partition_point(|(k, record)| (k.as_str(), record.code) < (key.as_str(), *code));
        self.records
            .get(at)
            .filter(|(k, record)| *k == key && record.code == *code)
            .map(|(_, record)| &record.value)
    }

    fn text(&self, name: &str, code: &[u8; 4]) -> Option<&str> {
        match self.get(name, code)? {
            Value::Text(text) => Some(text),
            _ => None,
        }
    }

    fn blob(&self, name: &str, code: &[u8; 4]) -> Option<&[u8]> {
        match self.get(name, code)? {
            Value::Blob(bytes) => Some(bytes),
            _ => None,
        }
    }

    fn plist(&self, name: &str, code: &[u8; 4]) -> Option<Plist> {
        bplist::parse(self.blob(name, code)?).ok()
    }

    /// The Finder comment of `name` (`cmmt`).
    pub fn comment(&self, name: &str) -> Option<&str> {
        self.text(name, b"cmmt").filter(|text| !text.is_empty())
    }

    /// Where `name` was before it was put in the Trash: its folder (`ptbL`) and its name
    /// (`ptbN`).
    pub fn put_back(&self, name: &str) -> Option<(&str, &str)> {
        Some((self.text(name, b"ptbL")?, self.text(name, b"ptbN")?))
    }

    /// The place of `name`'s icon, the icon's centre (`Iloc`: x and y as u32 in the first 8 bytes).
    pub fn icon_place(&self, name: &str) -> Option<(i32, i32)> {
        let blob = self.blob(name, b"Iloc")?;
        let x = i32::from_be_bytes(be32(blob, 0)?.to_be_bytes());
        let y = i32::from_be_bytes(be32(blob, 4)?.to_be_bytes());
        Some((x, y))
    }

    /// How the folder is viewed (`vstl` of `.`).
    pub fn view_style(&self) -> Option<ViewStyle> {
        match self.get(".", b"vstl")? {
            Value::Type(code) => match code {
                b"icnv" => Some(ViewStyle::Icon),
                b"Nlsv" => Some(ViewStyle::List),
                b"clmv" => Some(ViewStyle::Column),
                b"Flwv" => Some(ViewStyle::Flow),
                b"glyv" => Some(ViewStyle::Gallery),
                _ => None,
            },
            _ => None,
        }
    }

    /// The icon view's settings (`icvp` of `.`; the older `BKGD` gives the background when
    /// `icvp` has none).
    pub fn icon_view(&self) -> Option<IconView> {
        let number = |plist: &Plist, key: &str| match plist.get(key)? {
            Plist::Real(real) => Some(*real),
            Plist::Integer(int) => i32::try_from(*int).ok().map(f64::from),
            _ => None,
        };
        let from_icvp = self.plist(".", b"icvp").map(|plist| {
            let background = match plist.get("backgroundType").and_then(Plist::as_integer) {
                Some(1) => match (
                    number(&plist, "backgroundColorRed"),
                    number(&plist, "backgroundColorGreen"),
                    number(&plist, "backgroundColorBlue"),
                ) {
                    (Some(r), Some(g), Some(b)) => Background::Colour(r, g, b),
                    _ => Background::Default,
                },
                Some(2) => Background::Picture,
                _ => Background::Default,
            };
            IconView {
                icon_size: number(&plist, "iconSize").map(|size| size.clamp(16.0, 512.0)),
                background,
            }
        });
        from_icvp.or_else(|| {
            let blob = self.blob(".", b"BKGD")?;
            let background = match blob.get(..4)? {
                b"ClrB" => {
                    let part = |at: usize| {
                        blob.get(at..)
                            .and_then(<[u8]>::first_chunk::<2>)
                            .map_or(0.0, |pair| f64::from(u16::from_be_bytes(*pair)) / 65535.0)
                    };
                    Background::Colour(part(4), part(6), part(8))
                }
                b"PctB" => Background::Picture,
                _ => Background::Default,
            };
            Some(IconView {
                icon_size: None,
                background,
            })
        })
    }

    /// The window's place and size (`bwsp` of `.`: `WindowBounds`, "{{x, y}, {w, h}}").
    pub fn window(&self) -> Option<(i32, i32, u32, u32)> {
        let plist = self.plist(".", b"bwsp")?;
        let text = plist.get("WindowBounds")?.as_str()?;
        let numbers: Vec<i64> = text
            .split(|c: char| !(c.is_ascii_digit() || c == '-'))
            .filter(|part| !part.is_empty())
            .filter_map(|part| part.parse().ok())
            .collect();
        match numbers.as_slice() {
            [x, y, w, h] => Some((
                i32::try_from(*x).ok()?,
                i32::try_from(*y).ok()?,
                u32::try_from(*w).ok()?,
                u32::try_from(*h).ok()?,
            )),
            _ => None,
        }
    }

    /// The list view's sort (`lsvp` or `lsvP` of `.`): the column and whether it is ascending.
    pub fn list_sort(&self) -> Option<(ListSortColumn, bool)> {
        let plist = self
            .plist(".", b"lsvP")
            .or_else(|| self.plist(".", b"lsvp"))?;
        let name = plist.get("sortColumn")?.as_str()?;
        let column = match name {
            "name" => ListSortColumn::Name,
            "dateModified" => ListSortColumn::DateModified,
            "dateCreated" => ListSortColumn::DateCreated,
            "size" | "physicalSize" => ListSortColumn::Size,
            "kind" => ListSortColumn::Kind,
            "label" => ListSortColumn::Label,
            _ => return None,
        };
        let ascending = match plist.get("columns").and_then(|columns| columns.get(name)) {
            Some(column) => !matches!(column.get("ascending"), Some(Plist::Bool(false))),
            None => true,
        };
        Some((column, ascending))
    }
}

/// Reads and parses the `.DS_Store` of the folder `dir`. `None` when there is none, it is too
/// large or it is damaged: the notes are advisory.
pub fn read_store(fs: &dyn crate::fs::FileSystem, dir: crate::fs::NodeId) -> Option<DsStore> {
    let node = fs.lookup(dir, ".DS_Store").ok()??;
    if fs.metadata(node).ok()?.kind != crate::fs::NodeKind::File {
        return None;
    }
    let source = fs.open(node, crate::fs::Stream::Data).ok()?;
    if source.len() > MAX_DS_STORE as u64 {
        return None;
    }
    let mut bytes = vec![0u8; to_usize(source.len())];
    source.read_at(0, &mut bytes).ok()?;
    DsStore::parse(&bytes).ok()
}

/// `len` bytes at offset `offset` (from byte 4) of `bytes`.
fn slice_at(bytes: &[u8], offset: u32, len: usize) -> Result<&[u8]> {
    let start = to_usize(u64::from(offset)).saturating_add(4);
    start
        .checked_add(len)
        .and_then(|end| bytes.get(start..end))
        .ok_or_else(|| truncated(WHAT, u64::from(offset), len as u64, len64(bytes)))
}

/// The (position, size) of the block at `address`, checked against the file. `None` for 0.
fn block_of(bytes: &[u8], address: u32) -> Result<Option<(usize, usize)>> {
    if address == 0 {
        return Ok(None);
    }
    let offset = address & !0x1F;
    let bits = address & 0x1F;
    let size = 1usize.checked_shl(bits).unwrap_or(usize::MAX);
    slice_at(bytes, offset, size)?;
    Ok(Some((to_usize(u64::from(offset)).saturating_add(4), size)))
}

impl Walk<'_> {
    fn block(&self, number: u32) -> Result<&[u8]> {
        let (at, size) = self
            .blocks
            .get(to_usize(u64::from(number)))
            .copied()
            .flatten()
            .ok_or_else(|| {
                corrupt(
                    WHAT,
                    u64::from(number),
                    CorruptKind::InvalidValue {
                        field: "block number",
                        value: u64::from(number),
                    },
                )
            })?;
        self.bytes
            .get(at..at.saturating_add(size))
            .ok_or_else(|| truncated(WHAT, at as u64, size as u64, len64(self.bytes)))
    }

    fn node(&mut self, number: u32, depth: usize) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(corrupt(
                WHAT,
                u64::from(number),
                CorruptKind::LimitExceeded {
                    what: "tree depth",
                    limit: MAX_DEPTH as u64,
                },
            ));
        }
        if !self.visited.insert(number) {
            return Err(corrupt(
                WHAT,
                u64::from(number),
                CorruptKind::Loop { what: "tree node" },
            ));
        }
        let block = self.block(number)?.to_vec();
        let (Some(right), Some(count)) = (be32(&block, 0), be32(&block, 4)) else {
            return Err(truncated(WHAT, u64::from(number), 8, len64(&block)));
        };
        let mut at = 8usize;
        for _ in 0..count {
            let mut child = None;
            if right != 0 {
                child = be32(&block, at);
                at = at.saturating_add(4);
                if child.is_none() {
                    return Err(truncated(WHAT, u64::from(number), 4, len64(&block)));
                }
            }
            let (record, used) = read_record(&block, at)?;
            at = at.saturating_add(used);
            if let Some(child) = child {
                self.node(child, depth.saturating_add(1))?;
            }
            if let Some(record) = record {
                if self.records.len() >= MAX_DS_RECORDS {
                    return Err(corrupt(
                        WHAT,
                        u64::from(number),
                        CorruptKind::LimitExceeded {
                            what: "records",
                            limit: MAX_DS_RECORDS as u64,
                        },
                    ));
                }
                self.records.push((record.name.to_lowercase(), record));
            }
        }
        if right != 0 {
            self.node(right, depth.saturating_add(1))?;
        }
        Ok(())
    }
}

/// UTF-16BE text of `units` units at `at`; returns it and the bytes used.
fn utf16(block: &[u8], at: usize, units: usize) -> Result<String> {
    let len = units.saturating_mul(2);
    let raw = at
        .checked_add(len)
        .and_then(|end| block.get(at..end))
        .ok_or_else(|| truncated(WHAT, at as u64, len as u64, len64(block)))?;
    Ok(char::decode_utf16(
        raw.chunks_exact(2)
            .filter_map(|pair| <[u8; 2]>::try_from(pair).ok())
            .map(u16::from_be_bytes),
    )
    .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
    .collect())
}

/// Reads the record at `at`; `None` for the record (but the length is still returned) when its
/// type does not fit its code.
fn read_record(block: &[u8], at: usize) -> Result<(Option<Record>, usize)> {
    let start = at;
    let name_units = to_usize(u64::from(
        be32(block, at).ok_or_else(|| truncated(WHAT, at as u64, 4, len64(block)))?,
    ));
    if name_units > MAX_NAME_UNITS {
        return Err(corrupt(
            WHAT,
            at as u64,
            CorruptKind::OutOfRange {
                field: "name length",
                value: name_units as u64,
                limit_name: "longest name",
                limit: MAX_NAME_UNITS as u64,
            },
        ));
    }
    let name = utf16(block, at.saturating_add(4), name_units)?;
    let mut at = at.saturating_add(4).saturating_add(name_units.saturating_mul(2));
    let (Some(code), Some(kind)) = (
        block.get(at..).and_then(<[u8]>::first_chunk::<4>).copied(),
        block
            .get(at.saturating_add(4)..)
            .and_then(<[u8]>::first_chunk::<4>)
            .copied(),
    ) else {
        return Err(truncated(WHAT, at as u64, 8, len64(block)));
    };
    at = at.saturating_add(8);
    let fixed = |len: usize| {
        at.checked_add(len)
            .and_then(|end| block.get(at..end))
            .ok_or_else(|| truncated(WHAT, at as u64, len as u64, len64(block)))
    };
    let (value, used) = match &kind {
        b"bool" => (Value::Bool(fixed(1)?.first() != Some(&0)), 1),
        b"long" => (Value::Long(be32(fixed(4)?, 0).unwrap_or(0)), 4),
        b"shor" => (
            Value::Short(u16::from_be_bytes(*fixed(4)?.get(2..).and_then(<[u8]>::first_chunk::<2>).unwrap_or(&[0; 2]))),
            4,
        ),
        b"type" => (Value::Type(*fixed(4)?.first_chunk::<4>().unwrap_or(&[0; 4])), 4),
        b"comp" | b"dutc" => {
            let raw = u64::from_be_bytes(*fixed(8)?.first_chunk::<8>().unwrap_or(&[0; 8]));
            (if &kind == b"comp" { Value::Comp(raw) } else { Value::Dutc(raw) }, 8)
        }
        b"blob" => {
            let len = to_usize(u64::from(be32(fixed(4)?, 0).unwrap_or(0)));
            if len > MAX_BLOB {
                return Err(corrupt(
                    WHAT,
                    at as u64,
                    CorruptKind::OutOfRange {
                        field: "blob length",
                        value: len as u64,
                        limit_name: "longest blob",
                        limit: MAX_BLOB as u64,
                    },
                ));
            }
            let body = at
                .checked_add(4)
                .and_then(|from| block.get(from..from.checked_add(len)?))
                .ok_or_else(|| truncated(WHAT, at as u64, len as u64, len64(block)))?;
            (Value::Blob(body.to_vec()), len.saturating_add(4))
        }
        b"ustr" => {
            let units = to_usize(u64::from(be32(fixed(4)?, 0).unwrap_or(0)));
            if units > MAX_BLOB / 2 {
                return Err(corrupt(
                    WHAT,
                    at as u64,
                    CorruptKind::OutOfRange {
                        field: "text length",
                        value: units as u64,
                        limit_name: "longest text",
                        limit: (MAX_BLOB / 2) as u64,
                    },
                ));
            }
            (
                Value::Text(utf16(block, at.saturating_add(4), units)?),
                units.saturating_mul(2).saturating_add(4),
            )
        }
        // An unknown type has no known length: the node cannot be read past it.
        _ => return Err(corrupt_text(WHAT, "unknown value type")),
    };
    let total = at.saturating_add(used).saturating_sub(start);
    let fits = expected_type(code).is_none_or(|expected| *expected == kind);
    Ok((fits.then_some(Record { name, code, value }), total))
}

/// Builds `.DS_Store` files for tests and fuzz seeds.
#[cfg(test)]
pub(crate) mod test_store {
    /// A record's bytes.
    pub fn record(name: &str, code: &[u8; 4], kind: &[u8; 4], value: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(name.encode_utf16().count() as u32).to_be_bytes());
        out.extend(name.encode_utf16().flat_map(u16::to_be_bytes));
        out.extend_from_slice(code);
        out.extend_from_slice(kind);
        out.extend_from_slice(value);
        out
    }

    pub fn ustr(text: &str) -> Vec<u8> {
        let mut out = (text.encode_utf16().count() as u32).to_be_bytes().to_vec();
        out.extend(text.encode_utf16().flat_map(u16::to_be_bytes));
        out
    }

    pub fn blob(bytes: &[u8]) -> Vec<u8> {
        let mut out = (bytes.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(bytes);
        out
    }

    /// A leaf node holding `records`.
    pub fn leaf(records: &[Vec<u8>]) -> Vec<u8> {
        let mut out = 0u32.to_be_bytes().to_vec();
        out.extend_from_slice(&(records.len() as u32).to_be_bytes());
        for record in records {
            out.extend_from_slice(record);
        }
        out
    }

    /// A node with children: (child block, record) pairs and the rightmost child.
    pub fn inner(pairs: &[(u32, Vec<u8>)], right: u32) -> Vec<u8> {
        let mut out = right.to_be_bytes().to_vec();
        out.extend_from_slice(&(pairs.len() as u32).to_be_bytes());
        for (child, record) in pairs {
            out.extend_from_slice(&child.to_be_bytes());
            out.extend_from_slice(record);
        }
        out
    }

    /// A whole file: block 0 is the DSDB info (root node = block 1), `nodes` are blocks 1...
    pub fn build(nodes: &[Vec<u8>]) -> Vec<u8> {
        let mut blocks: Vec<Vec<u8>> = Vec::new();
        let mut info = 1u32.to_be_bytes().to_vec();
        info.extend_from_slice(&[0; 16]);
        blocks.push(info);
        blocks.extend(nodes.iter().cloned());
        // Place each block at a 32-byte aligned offset, after the 32 bytes of header past byte 4.
        let mut body: Vec<u8> = Vec::new();
        let mut addresses = Vec::new();
        for block in &blocks {
            let bits = (block.len().max(32)).next_power_of_two().trailing_zeros();
            let size = 1usize << bits;
            while body.len() % 32 != 0 {
                body.push(0);
            }
            let offset = 32 + body.len();
            addresses.push(offset as u32 | bits);
            body.extend_from_slice(block);
            body.resize(body.len() + size - block.len(), 0);
        }
        // The root block: count, 4 unknown, the table padded to 256 entries, the directory, free lists.
        let mut root = (blocks.len() as u32).to_be_bytes().to_vec();
        root.extend_from_slice(&[0; 4]);
        for address in &addresses {
            root.extend_from_slice(&address.to_be_bytes());
        }
        root.resize(8 + 256 * 4, 0);
        root.extend_from_slice(&1u32.to_be_bytes());
        root.push(4);
        root.extend_from_slice(b"DSDB");
        root.extend_from_slice(&0u32.to_be_bytes());
        root.extend_from_slice(&[0; 32 * 4]);
        let root_bits = root.len().next_power_of_two().trailing_zeros();
        while body.len() % 32 != 0 {
            body.push(0);
        }
        let root_off = 32 + body.len();
        body.extend_from_slice(&root);
        body.resize(body.len() + (1usize << root_bits) - root.len(), 0);
        let mut out = vec![0, 0, 0, 1];
        out.extend_from_slice(b"Bud1");
        out.extend_from_slice(&(root_off as u32).to_be_bytes());
        out.extend_from_slice(&(1u32 << root_bits).to_be_bytes());
        out.extend_from_slice(&(root_off as u32).to_be_bytes());
        out.extend_from_slice(&[0; 16]);
        out.extend_from_slice(&body);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::test_store::*;
    use super::*;

    fn sample() -> Vec<u8> {
        let mut iloc = 100u32.to_be_bytes().to_vec();
        iloc.extend_from_slice(&0xFFFF_FFF0u32.to_be_bytes());
        iloc.extend_from_slice(&[0xFF; 6]);
        iloc.extend_from_slice(&[0; 2]);
        build(&[leaf(&[
            record(".", b"vstl", b"type", b"Nlsv"),
            record("Notes.txt", b"Iloc", b"blob", &blob(&iloc)),
            record("Notes.txt", b"cmmt", b"ustr", &ustr("Hello, é")),
            record("Trashed", b"ptbL", b"ustr", &ustr("Users/a/Desktop")),
            record("Trashed", b"ptbN", b"ustr", &ustr("Trashed")),
        ])])
    }

    #[test]
    fn a_valid_file_reads_back() {
        let store = DsStore::parse(&sample()).unwrap();
        assert_eq!(store.view_style(), Some(ViewStyle::List));
        assert_eq!(store.comment("Notes.txt"), Some("Hello, é"));
        // Names are compared without regard to case.
        assert_eq!(store.comment("NOTES.TXT"), Some("Hello, é"));
        assert_eq!(store.icon_place("notes.txt"), Some((100, -16)));
        assert_eq!(store.put_back("Trashed"), Some(("Users/a/Desktop", "Trashed")));
        assert_eq!(store.comment("Missing"), None);
        assert_eq!(store.records().count(), 5);
    }

    #[test]
    fn a_record_of_the_wrong_type_is_skipped_and_the_rest_kept() {
        let bytes = build(&[leaf(&[
            record("a", b"cmmt", b"long", &7u32.to_be_bytes()),
            record("b", b"cmmt", b"ustr", &ustr("kept")),
        ])]);
        let store = DsStore::parse(&bytes).unwrap();
        assert_eq!(store.comment("a"), None);
        assert_eq!(store.comment("b"), Some("kept"));
        assert_eq!(store.records().count(), 1);
    }

    #[test]
    fn a_tree_with_children_is_walked_and_a_cycle_is_refused() {
        // Block 1 is the root with children 2 and 3; block 4 loops to itself.
        let fine = build(&[
            inner(&[(2, record("m", b"cmmt", b"ustr", &ustr("middle")))], 3),
            leaf(&[record("a", b"cmmt", b"ustr", &ustr("left"))]),
            leaf(&[record("z", b"cmmt", b"ustr", &ustr("right"))]),
        ]);
        let store = DsStore::parse(&fine).unwrap();
        assert_eq!(store.records().count(), 3);
        assert_eq!(store.comment("z"), Some("right"));
        let looped = build(&[inner(&[(1, record("m", b"cmmt", b"ustr", &ustr("x")))], 0)]);
        assert!(DsStore::parse(&looped).is_err());
    }

    #[test]
    fn depth_seventeen_and_too_many_records_are_refused() {
        // A chain of 17 inner nodes (blocks 1 to 17), the last a leaf.
        let mut nodes: Vec<Vec<u8>> = (1..=17u32).map(|n| inner(&[], n + 1)).collect();
        nodes.push(leaf(&[]));
        assert!(DsStore::parse(&build(&nodes)).is_err());
        // 16 levels is fine.
        let mut ok: Vec<Vec<u8>> = (1..=15u32).map(|n| inner(&[], n + 1)).collect();
        ok.push(leaf(&[]));
        assert!(DsStore::parse(&build(&ok)).is_ok());
        let many: Vec<Vec<u8>> = (0..=MAX_DS_RECORDS)
            .map(|_| record("a", b"xxxx", b"bool", &[1]))
            .collect();
        assert!(DsStore::parse(&build(&[leaf(&many)])).is_err());
    }

    #[test]
    fn hostile_files_are_errors() {
        let good = sample();
        // A block address past the file.
        let mut past = good.clone();
        // The first table entry sits 8 bytes into the root block; find the root through the header.
        let root = u32::from_be_bytes(good[8..12].try_into().unwrap()) as usize + 4;
        past[root + 8..root + 12].copy_from_slice(&0x7FFF_FFE5u32.to_be_bytes());
        assert!(DsStore::parse(&past).is_err());
        // The root offset repeated wrongly, a bad magic, a short file, an oversize record name.
        let mut again = good.clone();
        again[16] ^= 1;
        assert!(DsStore::parse(&again).is_err());
        assert!(DsStore::parse(b"not a ds_store").is_err());
        assert!(DsStore::parse(&good[..20]).is_err());
        let long_name = "x".repeat(256);
        assert!(DsStore::parse(&build(&[leaf(&[record(&long_name, b"cmmt", b"ustr", &ustr("a"))])])).is_err());
        // An unknown value type ends the node with an error.
        assert!(DsStore::parse(&build(&[leaf(&[record("a", b"cmmt", b"zzzz", &[0; 4])])])).is_err());
    }

    #[test]
    fn random_damage_never_panics() {
        let base = sample();
        let mut state = 0x0DDB_A11_C0FFEEu64;
        for _ in 0..3000 {
            let mut bytes = base.clone();
            for _ in 0..4 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let at = (state % bytes.len() as u64) as usize;
                bytes[at] = (state >> 24) as u8;
            }
            let _ = DsStore::parse(&bytes);
        }
    }
}

#[cfg(test)]
mod fuzz_seeds {
    use super::test_store::*;
    use super::*;

    /// Writes seeds for the `ds_store` fuzz target into `DISK_ATLAS_FUZZ_SEEDS/ds_store`.
    #[test]
    #[ignore = "writes files; run by the fuzz job"]
    fn write_fuzz_seeds() {
        let Some(root) = std::env::var_os("DISK_ATLAS_FUZZ_SEEDS") else {
            return;
        };
        let dir = std::path::Path::new(&root).join("ds_store");
        std::fs::create_dir_all(&dir).expect("create seed directory");
        let flat = build(&[leaf(&[
            record(".", b"vstl", b"type", b"icnv"),
            record("a.txt", b"cmmt", b"ustr", &ustr("seed")),
        ])]);
        assert!(DsStore::parse(&flat).is_ok(), "the seed is valid");
        std::fs::write(dir.join("flat"), flat).expect("write seed");
        let tree = build(&[
            inner(&[(2, record("m", b"cmmt", b"ustr", &ustr("middle")))], 3),
            leaf(&[record("a", b"cmmt", b"ustr", &ustr("left"))]),
            leaf(&[record("z", b"cmmt", b"ustr", &ustr("right"))]),
        ]);
        assert!(DsStore::parse(&tree).is_ok(), "the seed is valid");
        std::fs::write(dir.join("tree"), tree).expect("write seed");
    }
}
