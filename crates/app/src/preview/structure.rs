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

//! Plain-words explanations of known file headers for the bytes view.
//! Built in M5 task 3c from `.claude/docs/design/m5-quicklook-copy.md` §4.2.
//!
//! [`describe`] looks at the first bytes of a stream and, when they match a
//! known header, returns one [`Structure`]: its fields, each with the byte
//! range it covers, a value in words a person can read and a one-sentence
//! note. The bytes view shows the note of whichever field the selection
//! touches and tints the field's whole range.
//!
//! Everything read here came from a disk and is hostile: every read goes
//! through `head.get(..)`, offsets are added with `checked_add`, and a field
//! that is not wholly inside `head` is left out rather than shown half read.
//! Nothing loops over a count taken from the file except the AppleSingle
//! entry table, which stops at [`MAX_ENTRIES`].
//!
//! Each layout is a `const` table of [`Row`]s plus a recogniser. At most one
//! layout is returned: they are tried in the order of the design's table and
//! the first that matches wins. Only the start of the stream is looked at, so
//! a MacBinary file whose data begins with a PNG is described as MacBinary.

// No caller until the bytes view explains the selection (M5 task 5b). Not an `expect` under test,
// where the tests use everything.
#![cfg_attr(
    not(test),
    expect(dead_code, reason = "the bytes view (M5 task 5b) is the first caller")
)]

use std::ops::Range;

use chrono::DateTime;
use disk_atlas_lib::fs::Stream;

/// Most AppleSingle / AppleDouble entry descriptors described.
const MAX_ENTRIES: usize = 16;

/// Seconds from the Mac epoch (1904-01-01) to the Unix epoch (1970-01-01).
const MAC_TO_UNIX: i64 = 2_082_844_800;

/// One explained part of a header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// The bytes of the stream this field occupies. Always wholly inside
    /// the `head` given to [`describe`].
    pub range: Range<u64>,
    /// Short name, e.g. "Width".
    pub name: &'static str,
    /// The decoded value, as text for a person.
    pub value: String,
    /// One plain sentence saying what the field is for.
    pub note: &'static str,
}

/// A recognised header and its explained fields, in file order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Structure {
    /// Plain-words name of the format, e.g. "PNG picture".
    pub title: &'static str,
    /// Never empty.
    pub fields: Vec<Field>,
}

/// How a field's bytes are decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldKind {
    U8,
    U16Be,
    U16Le,
    U32Be,
    U32Le,
    U64Be,
    U64Le,
    /// Four bytes shown as text, `.` for anything not printable.
    FourCc,
    /// Raw bytes, shown as at most 16 hex bytes.
    Bytes,
    /// A length byte then that many characters, in a field of fixed size.
    PascalString,
    /// Unsigned 32-bit big-endian seconds since 1904-01-01 00:00:00 UTC.
    MacDate,
    /// Signed 32-bit big-endian fixed point, 16 bits before the point.
    Fixed16_16,
}

impl FieldKind {
    /// The size this kind needs, or `None` when the table row says.
    fn width(self) -> Option<usize> {
        match self {
            Self::U8 => Some(1),
            Self::U16Be | Self::U16Le => Some(2),
            Self::U32Be | Self::U32Le | Self::FourCc | Self::MacDate | Self::Fixed16_16 => Some(4),
            Self::U64Be | Self::U64Le => Some(8),
            Self::Bytes | Self::PascalString => None,
        }
    }

    /// The bytes as a number, for the kinds that are one.
    fn number(self, bytes: &[u8]) -> Option<u64> {
        let big = || bytes.iter().fold(0u64, |n, &b| (n << 8) | u64::from(b));
        let little = || {
            bytes
                .iter()
                .rev()
                .fold(0u64, |n, &b| (n << 8) | u64::from(b))
        };
        match self {
            Self::U8
            | Self::U16Be
            | Self::U32Be
            | Self::U64Be
            | Self::MacDate
            | Self::Fixed16_16 => Some(big()),
            Self::U16Le | Self::U32Le | Self::U64Le => Some(little()),
            Self::FourCc | Self::Bytes | Self::PascalString => None,
        }
    }
}

/// `(offset, length, name, kind, note)`.
type Row = (usize, usize, &'static str, FieldKind, &'static str);

/// Adds words to a field's value: given the field's name, its raw bytes and
/// its number (for the numeric kinds), the text for the parentheses.
type Label = fn(&str, &[u8], Option<u64>) -> Option<String>;

fn no_label(_: &str, _: &[u8], _: Option<u64>) -> Option<String> {
    None
}

/// The structures recognised at the start of the stream. `head` is its
/// first 4096 bytes (or all of it when shorter) and `len` the stream's full
/// length. Empty when nothing is recognised; never panics, whatever the
/// bytes.
pub fn describe(head: &[u8], stream: Stream, len: u64) -> Vec<Structure> {
    let resource = if stream == Stream::Resource {
        resource_fork_header(head, len)
    } else {
        None
    };
    resource
        .or_else(|| apple_single_double(head))
        .or_else(|| mac_binary(head, len))
        .or_else(|| png(head))
        .or_else(|| jpeg(head))
        .or_else(|| gif(head))
        .or_else(|| tiff(head))
        .or_else(|| zip_local_header(head))
        .or_else(|| pdf(head))
        .or_else(|| binary_plist(head))
        .or_else(|| mach_o(head))
        .or_else(|| binhex(head))
        .into_iter()
        .collect()
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// `length` bytes of `head` at `offset`, when they are all there.
fn slice(head: &[u8], offset: usize, length: usize) -> Option<&[u8]> {
    head.get(offset..offset.checked_add(length)?)
}

/// Big-endian `u32` at `offset`.
fn u32_be(head: &[u8], offset: usize) -> Option<u32> {
    let bytes: [u8; 4] = slice(head, offset, 4)?.try_into().ok()?;
    Some(u32::from_be_bytes(bytes))
}

/// Reads one row. `None` when its bytes are not wholly inside `head`.
fn read_field(head: &[u8], row: &Row, label: Label) -> Option<Field> {
    let &(offset, length, name, kind, note) = row;
    if kind.width().is_some_and(|width| width != length) {
        return None;
    }
    let bytes = slice(head, offset, length)?;
    let number = kind.number(bytes);
    let mut value = plain_value(kind, bytes, number);
    if let Some(words) = label(name, bytes, number) {
        value = format!("{value} ({words})");
    }
    Some(Field {
        range: u64::try_from(offset).ok()?..u64::try_from(offset.checked_add(length)?).ok()?,
        name,
        value,
        note,
    })
}

/// The structure made of the rows that are inside `head`, or `None` when
/// none is.
fn structure(title: &'static str, head: &[u8], rows: &[Row], label: Label) -> Option<Structure> {
    let fields: Vec<Field> = rows
        .iter()
        .filter_map(|row| read_field(head, row, label))
        .collect();
    (!fields.is_empty()).then_some(Structure { title, fields })
}

// ---------------------------------------------------------------------------
// Value text
// ---------------------------------------------------------------------------

fn plain_value(kind: FieldKind, bytes: &[u8], number: Option<u64>) -> String {
    match (kind, number) {
        (FieldKind::FourCc, _) => format!("'{}'", printable(bytes)),
        (FieldKind::Bytes, _) => hex(bytes),
        (FieldKind::PascalString, _) => pascal(bytes),
        (FieldKind::MacDate, Some(n)) => mac_date(n),
        (FieldKind::Fixed16_16, Some(n)) => fixed_16_16(n),
        (_, Some(n)) => n.to_string(),
        (_, None) => hex(bytes),
    }
}

/// ASCII printable bytes as they are, anything else as `.`.
fn printable(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if (0x20..0x7F).contains(&b) {
                char::from(b)
            } else {
                '.'
            }
        })
        .collect()
}

/// At most 16 bytes as hex pairs, then an ellipsis when there are more.
fn hex(bytes: &[u8]) -> String {
    let shown = bytes.get(..16).unwrap_or(bytes);
    let mut text = shown
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ");
    if bytes.len() > shown.len() {
        text.push_str(" …");
    }
    text
}

/// A Pascal string in a field of fixed size. Mac OS Roman is not decoded:
/// bytes outside printable ASCII show as `.`.
fn pascal(bytes: &[u8]) -> String {
    let Some((&count, rest)) = bytes.split_first() else {
        return "\"\"".to_owned();
    };
    let shown = rest
        .get(..usize::from(count).min(rest.len()))
        .unwrap_or_default();
    format!("\"{}\"", printable(shown))
}

/// `YYYY-MM-DD HH:MM:SS UTC` for seconds since 1904-01-01; 0 is "not set".
fn mac_date(seconds: u64) -> String {
    if seconds == 0 {
        return "not set".to_owned();
    }
    // `seconds` came from four bytes, so it fits an i64 with room to spare.
    let unix = i64::try_from(seconds).unwrap_or(i64::MAX) - MAC_TO_UNIX;
    match DateTime::from_timestamp(unix, 0) {
        Some(time) => time.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        None => "not a valid date".to_owned(),
    }
}

/// A signed 16.16 number with up to four decimals.
fn fixed_16_16(raw: u64) -> String {
    let signed = i32::from_be_bytes(u32::try_from(raw).unwrap_or(0).to_be_bytes());
    let text = format!("{:.4}", f64::from(signed) / 65536.0);
    let text = text.trim_end_matches('0').trim_end_matches('.');
    match text {
        "" | "-0" => "0".to_owned(),
        other => other.to_owned(),
    }
}

/// `0x` and eight hex digits.
fn hex32(n: u64) -> String {
    format!("0x{n:08X}")
}

// ---------------------------------------------------------------------------
// Resource fork header
// ---------------------------------------------------------------------------

const RESOURCE_FORK: [Row; 4] = [
    (
        0,
        4,
        "Where the resources start",
        FieldKind::U32Be,
        "Bytes from the start of the fork to the stored resource data.",
    ),
    (
        4,
        4,
        "Where the resource map starts",
        FieldKind::U32Be,
        "Bytes from the start of the fork to the map, which holds the list of resource types and where each resource is.",
    ),
    (
        8,
        4,
        "Length of the resources",
        FieldKind::U32Be,
        "How many bytes of resource data there are.",
    ),
    (
        12,
        4,
        "Length of the resource map",
        FieldKind::U32Be,
        "How many bytes the map takes; it holds the type list.",
    ),
];

/// Recognised when, with the four numbers read as big-endian:
///
/// - both offsets are at least 16 (past this header) and the map is at least
///   28 bytes (the smallest map header Inside Macintosh describes);
/// - the data (`offset + length`) and the map each end inside `len`, with
///   the sums made by `checked_add`;
/// - the data and the map do not overlap, in either order.
///
/// Real forks (data at 0x100, map straight after the data) pass. Four random
/// words almost never do: both ends must land inside the stream's length.
fn resource_fork_header(head: &[u8], len: u64) -> Option<Structure> {
    let data_offset = u64::from(u32_be(head, 0)?);
    let map_offset = u64::from(u32_be(head, 4)?);
    let data_length = u64::from(u32_be(head, 8)?);
    let map_length = u64::from(u32_be(head, 12)?);
    if data_offset < 16 || map_offset < 16 || map_length < 28 {
        return None;
    }
    let data_end = data_offset.checked_add(data_length)?;
    let map_end = map_offset.checked_add(map_length)?;
    if data_end > len || map_end > len {
        return None;
    }
    if map_offset < data_end && data_offset < map_end {
        return None;
    }
    structure("Resource fork header", head, &RESOURCE_FORK, no_label)
}

// ---------------------------------------------------------------------------
// AppleSingle / AppleDouble (RFC 1740)
// ---------------------------------------------------------------------------

const APPLE_SINGLE_MAGIC: u32 = 0x0005_1600;
const APPLE_DOUBLE_MAGIC: u32 = 0x0005_1607;

const APPLE_SINGLE: [Row; 3] = [
    (
        0,
        4,
        "Magic number",
        FieldKind::Bytes,
        "Marks the file as AppleSingle or AppleDouble.",
    ),
    (
        4,
        4,
        "Version",
        FieldKind::Bytes,
        "Which edition of the format the file follows.",
    ),
    (
        24,
        2,
        "Number of entries",
        FieldKind::U16Be,
        "How many parts the file is made of; each has a line in the list that follows.",
    ),
];

/// The entry descriptors start right after the 26-byte header.
const ENTRIES_AT: usize = 26;
const ENTRY_SIZE: usize = 12;

const ENTRY_NAMES: [[&str; 3]; MAX_ENTRIES] = [
    ["Entry 1: kind", "Entry 1: offset", "Entry 1: length"],
    ["Entry 2: kind", "Entry 2: offset", "Entry 2: length"],
    ["Entry 3: kind", "Entry 3: offset", "Entry 3: length"],
    ["Entry 4: kind", "Entry 4: offset", "Entry 4: length"],
    ["Entry 5: kind", "Entry 5: offset", "Entry 5: length"],
    ["Entry 6: kind", "Entry 6: offset", "Entry 6: length"],
    ["Entry 7: kind", "Entry 7: offset", "Entry 7: length"],
    ["Entry 8: kind", "Entry 8: offset", "Entry 8: length"],
    ["Entry 9: kind", "Entry 9: offset", "Entry 9: length"],
    ["Entry 10: kind", "Entry 10: offset", "Entry 10: length"],
    ["Entry 11: kind", "Entry 11: offset", "Entry 11: length"],
    ["Entry 12: kind", "Entry 12: offset", "Entry 12: length"],
    ["Entry 13: kind", "Entry 13: offset", "Entry 13: length"],
    ["Entry 14: kind", "Entry 14: offset", "Entry 14: length"],
    ["Entry 15: kind", "Entry 15: offset", "Entry 15: length"],
    ["Entry 16: kind", "Entry 16: offset", "Entry 16: length"],
];

const ENTRY_KIND_NOTE: &str = "What this part of the file holds.";
const ENTRY_OFFSET_NOTE: &str = "Where this part starts, in bytes from the start of the file.";
const ENTRY_LENGTH_NOTE: &str = "How many bytes this part takes.";

fn apple_single_label(name: &str, bytes: &[u8], _: Option<u64>) -> Option<String> {
    match (name, bytes) {
        ("Magic number", [0x00, 0x05, 0x16, 0x00]) => Some("AppleSingle".to_owned()),
        ("Magic number", [0x00, 0x05, 0x16, 0x07]) => Some("AppleDouble".to_owned()),
        ("Version", [0x00, 0x02, 0x00, 0x00]) => Some("version 2".to_owned()),
        ("Version", [0x00, 0x01, 0x00, 0x00]) => Some("version 1".to_owned()),
        _ => None,
    }
}

/// The RFC 1740 entry ids.
fn entry_kind(label_for: &str, _: &[u8], id: Option<u64>) -> Option<String> {
    if !label_for.ends_with(": kind") {
        return None;
    }
    let text = match id? {
        1 => "data fork",
        2 => "resource fork",
        3 => "real name",
        4 => "comment",
        5 => "black-and-white icon",
        6 => "colour icon",
        8 => "file dates",
        9 => "Finder info",
        10 => "Macintosh file info",
        11 => "ProDOS file info",
        12 => "MS-DOS file info",
        13 => "short name",
        14 => "AFP file info",
        15 => "AFP directory ID",
        _ => return None,
    };
    Some(text.to_owned())
}

fn apple_single_double(head: &[u8]) -> Option<Structure> {
    let title = match u32_be(head, 0)? {
        APPLE_SINGLE_MAGIC => "AppleSingle file",
        APPLE_DOUBLE_MAGIC => "AppleDouble companion file",
        _ => return None,
    };
    let mut found = structure(title, head, &APPLE_SINGLE, apple_single_label)?;
    let count = slice(head, 24, 2)
        .and_then(|b| <[u8; 2]>::try_from(b).ok())
        .map_or(0, |b| usize::from(u16::from_be_bytes(b)));
    for (i, names) in ENTRY_NAMES.iter().enumerate().take(count.min(MAX_ENTRIES)) {
        let start = ENTRIES_AT + i * ENTRY_SIZE;
        if slice(head, start, ENTRY_SIZE).is_none() {
            break;
        }
        let rows: [(Row, Label); 3] = [
            (
                (start, 4, names[0], FieldKind::U32Be, ENTRY_KIND_NOTE),
                entry_kind,
            ),
            (
                (start + 4, 4, names[1], FieldKind::U32Be, ENTRY_OFFSET_NOTE),
                no_label,
            ),
            (
                (start + 8, 4, names[2], FieldKind::U32Be, ENTRY_LENGTH_NOTE),
                no_label,
            ),
        ];
        found.fields.extend(
            rows.iter()
                .filter_map(|(row, label)| read_field(head, row, *label)),
        );
    }
    Some(found)
}

// ---------------------------------------------------------------------------
// MacBinary
// ---------------------------------------------------------------------------

const MAC_BINARY: [Row; 7] = [
    (
        1,
        64,
        "File name",
        FieldKind::PascalString,
        "The name the file had on the Mac.",
    ),
    (
        65,
        4,
        "File type",
        FieldKind::FourCc,
        "The four-letter code the Mac used to tell what kind of file this is.",
    ),
    (
        69,
        4,
        "Creator",
        FieldKind::FourCc,
        "The four-letter code of the program that made the file.",
    ),
    (
        83,
        4,
        "Data fork length",
        FieldKind::U32Be,
        "How many bytes the file's data part has.",
    ),
    (
        87,
        4,
        "Resource fork length",
        FieldKind::U32Be,
        "How many bytes the file's resource part has.",
    ),
    (
        91,
        4,
        "Created",
        FieldKind::MacDate,
        "When the file was first made.",
    ),
    (
        95,
        4,
        "Modified",
        FieldKind::MacDate,
        "When the file was last changed.",
    ),
];

/// MacBinary II specification: byte 0 is zero, byte 1 is the file name
/// length (1 to 63), bytes 74 and 82 are zero. A 128-byte header cannot fit
/// in a shorter stream.
fn mac_binary(head: &[u8], len: u64) -> Option<Structure> {
    let byte = |at: usize| head.get(at).copied();
    let plausible = len >= 128
        && byte(0) == Some(0)
        && byte(1).is_some_and(|n| (1..=63).contains(&n))
        && byte(74) == Some(0)
        && byte(82) == Some(0);
    if !plausible {
        return None;
    }
    structure("MacBinary file", head, &MAC_BINARY, no_label)
}

// ---------------------------------------------------------------------------
// PNG
// ---------------------------------------------------------------------------

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

const PNG: [Row; 6] = [
    (
        0,
        8,
        "Signature",
        FieldKind::Bytes,
        "The eight bytes every PNG picture starts with.",
    ),
    (
        8,
        4,
        "Header chunk length",
        FieldKind::U32Be,
        "How many bytes the picture's header block holds; it is always 13.",
    ),
    (
        16,
        4,
        "Width",
        FieldKind::U32Be,
        "How many pixels wide the picture is.",
    ),
    (
        20,
        4,
        "Height",
        FieldKind::U32Be,
        "How many pixels high the picture is.",
    ),
    (
        24,
        1,
        "Bit depth",
        FieldKind::U8,
        "How many bits each colour value of a pixel uses.",
    ),
    (
        25,
        1,
        "Colour type",
        FieldKind::U8,
        "How the pixels store colour: shades of grey, full colour, a palette, with or without transparency.",
    ),
];

fn png_label(name: &str, _: &[u8], number: Option<u64>) -> Option<String> {
    if name != "Colour type" {
        return None;
    }
    let text = match number? {
        0 => "greyscale",
        2 => "colour",
        3 => "palette",
        4 => "greyscale with transparency",
        6 => "colour with transparency",
        _ => return None,
    };
    Some(text.to_owned())
}

fn png(head: &[u8]) -> Option<Structure> {
    if !head.starts_with(&PNG_SIGNATURE) {
        return None;
    }
    structure("PNG picture", head, &PNG, png_label)
}

// ---------------------------------------------------------------------------
// JPEG
// ---------------------------------------------------------------------------

const JPEG_START: Row = (
    0,
    2,
    "Start marker",
    FieldKind::Bytes,
    "The two bytes every JPEG picture starts with.",
);
const JPEG_MARKER: Row = (
    2,
    2,
    "First marker",
    FieldKind::Bytes,
    "Names the first block of the picture file; JFIF and Exif blocks carry details about the picture.",
);
const JPEG_MARKER_LENGTH: Row = (
    4,
    2,
    "First marker length",
    FieldKind::U16Be,
    "How many bytes that first block takes, counting these two.",
);
const JPEG_IDENTIFIER: Row = (
    6,
    4,
    "Block identifier",
    FieldKind::FourCc,
    "Says which standard the first block follows: JFIF for ordinary pictures, Exif for camera photos.",
);

fn jpeg_label(name: &str, bytes: &[u8], _: Option<u64>) -> Option<String> {
    match (name, bytes) {
        ("Start marker", _) => Some("start of picture".to_owned()),
        ("First marker", [_, marker]) => {
            let text = match marker {
                0xE0 => "JFIF header",
                0xE1 => "Exif or XMP details",
                0xE2 => "colour profile or other details",
                0xEE => "Adobe details",
                0xDB => "quantisation table",
                0xC0..=0xC2 => "picture size and format",
                0xC4 => "Huffman table",
                0xFE => "comment",
                _ => return None,
            };
            Some(text.to_owned())
        }
        _ => None,
    }
}

/// Markers that stand alone, with no length after them.
fn marker_has_length(marker: u8) -> bool {
    !matches!(marker, 0x00 | 0x01 | 0xD0..=0xD9 | 0xFF)
}

fn jpeg(head: &[u8]) -> Option<Structure> {
    if !head.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut rows = vec![JPEG_START];
    if let Some(&[0xFF, marker]) = slice(head, 2, 2) {
        rows.push(JPEG_MARKER);
        if marker_has_length(marker) {
            rows.push(JPEG_MARKER_LENGTH);
        }
        let identifier = slice(head, 6, 4);
        if (marker == 0xE0 && identifier.is_some_and(|i| i == b"JFIF"))
            || (marker == 0xE1 && identifier.is_some_and(|i| i == b"Exif"))
        {
            rows.push(JPEG_IDENTIFIER);
        }
    }
    structure("JPEG picture", head, &rows, jpeg_label)
}

// ---------------------------------------------------------------------------
// GIF
// ---------------------------------------------------------------------------

const GIF: [Row; 4] = [
    (
        0,
        6,
        "Signature",
        FieldKind::Bytes,
        "The six characters every GIF picture starts with: GIF, then the edition (87a or 89a).",
    ),
    (
        6,
        2,
        "Width",
        FieldKind::U16Le,
        "How many pixels wide the picture is.",
    ),
    (
        8,
        2,
        "Height",
        FieldKind::U16Le,
        "How many pixels high the picture is.",
    ),
    (
        10,
        1,
        "Flags",
        FieldKind::U8,
        "Packed settings: whether the picture has a shared colour table and how large it is.",
    ),
];

fn gif_label(name: &str, bytes: &[u8], number: Option<u64>) -> Option<String> {
    match name {
        "Signature" => Some(format!("\"{}\"", printable(bytes))),
        "Flags" => {
            let flags = number?;
            if flags & 0x80 == 0 {
                Some("no shared colour table".to_owned())
            } else {
                // The table has 2^(n + 1) entries, n in the low three bits.
                Some(format!(
                    "shared colour table of {} colours",
                    2u32 << (flags & 7)
                ))
            }
        }
        _ => None,
    }
}

fn gif(head: &[u8]) -> Option<Structure> {
    if !(head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a")) {
        return None;
    }
    structure("GIF picture", head, &GIF, gif_label)
}

// ---------------------------------------------------------------------------
// TIFF
// ---------------------------------------------------------------------------

const TIFF_LITTLE: [Row; 3] = [
    (
        0,
        2,
        "Byte order",
        FieldKind::Bytes,
        "Says whether numbers in this file store their lowest byte first (II) or last (MM).",
    ),
    (
        2,
        2,
        "Magic number",
        FieldKind::U16Le,
        "Always 42 in a TIFF picture.",
    ),
    (
        4,
        4,
        "First directory offset",
        FieldKind::U32Le,
        "Where the first list of picture settings starts, in bytes from the start of the file.",
    ),
];

const TIFF_BIG: [Row; 3] = [
    TIFF_LITTLE[0],
    (
        2,
        2,
        "Magic number",
        FieldKind::U16Be,
        "Always 42 in a TIFF picture.",
    ),
    (
        4,
        4,
        "First directory offset",
        FieldKind::U32Be,
        "Where the first list of picture settings starts, in bytes from the start of the file.",
    ),
];

fn tiff_label(name: &str, bytes: &[u8], _: Option<u64>) -> Option<String> {
    match (name, bytes) {
        ("Byte order", b"II") => Some("lowest byte first".to_owned()),
        ("Byte order", b"MM") => Some("highest byte first".to_owned()),
        _ => None,
    }
}

fn tiff(head: &[u8]) -> Option<Structure> {
    let rows = if head.starts_with(b"II*\0") {
        &TIFF_LITTLE
    } else if head.starts_with(b"MM\0*") {
        &TIFF_BIG
    } else {
        return None;
    };
    structure("TIFF picture", head, rows, tiff_label)
}

// ---------------------------------------------------------------------------
// ZIP local file header (PKWARE APPNOTE 4.3.7)
// ---------------------------------------------------------------------------

const ZIP: [Row; 7] = [
    (
        0,
        4,
        "Signature",
        FieldKind::Bytes,
        "Marks the start of one stored file inside a ZIP archive.",
    ),
    (
        4,
        2,
        "Version needed",
        FieldKind::U16Le,
        "The ZIP version a program needs to unpack this file, times ten.",
    ),
    (
        6,
        2,
        "Flags",
        FieldKind::U16Le,
        "Option bits, such as whether the file is encrypted.",
    ),
    (
        8,
        2,
        "Compression method",
        FieldKind::U16Le,
        "How the file was squeezed to save space.",
    ),
    (
        14,
        4,
        "CRC-32",
        FieldKind::U32Le,
        "A checksum of the file's contents, used to detect damage.",
    ),
    (
        18,
        4,
        "Compressed size",
        FieldKind::U32Le,
        "How many bytes the file takes inside the archive.",
    ),
    (
        22,
        4,
        "Original size",
        FieldKind::U32Le,
        "How many bytes the file takes once unpacked.",
    ),
];

const ZIP_NAME_LENGTH: Row = (
    26,
    2,
    "File name length",
    FieldKind::U16Le,
    "How many bytes of file name follow this header.",
);

fn zip_label(name: &str, _: &[u8], number: Option<u64>) -> Option<String> {
    match name {
        "Signature" => Some("PK, local file header".to_owned()),
        "Version needed" => {
            let n = number?;
            Some(format!("{}.{}", n / 10, n % 10))
        }
        "Compression method" => {
            let text = match number? {
                0 => "stored, not squeezed",
                8 => "deflated",
                9 => "deflate64",
                12 => "bzip2",
                14 => "LZMA",
                93 => "zstd",
                95 => "xz",
                98 => "PPMd",
                _ => return None,
            };
            Some(text.to_owned())
        }
        "CRC-32" => Some(hex32(number?)),
        _ => None,
    }
}

fn zip_local_header(head: &[u8]) -> Option<Structure> {
    if !head.starts_with(b"PK\x03\x04") {
        return None;
    }
    let mut rows = ZIP.to_vec();
    rows.push(ZIP_NAME_LENGTH);
    structure("ZIP archive entry", head, &rows, zip_label)
}

// ---------------------------------------------------------------------------
// PDF
// ---------------------------------------------------------------------------

const PDF: [Row; 2] = [
    (
        0,
        5,
        "Header",
        FieldKind::Bytes,
        "The five characters every PDF document starts with.",
    ),
    (
        5,
        3,
        "Version",
        FieldKind::Bytes,
        "Which edition of the PDF format the document follows, such as 1.7.",
    ),
];

fn text_label(name: &str, bytes: &[u8], _: Option<u64>) -> Option<String> {
    match name {
        "Header" | "Version" | "Magic number" | "Format version" => {
            Some(format!("\"{}\"", printable(bytes)))
        }
        _ => None,
    }
}

fn pdf(head: &[u8]) -> Option<Structure> {
    if !head.starts_with(b"%PDF-") {
        return None;
    }
    structure("PDF document", head, &PDF, text_label)
}

// ---------------------------------------------------------------------------
// Binary property list
// ---------------------------------------------------------------------------

const BINARY_PLIST: [Row; 2] = [
    (
        0,
        6,
        "Magic number",
        FieldKind::Bytes,
        "The characters \"bplist\" mark an Apple property list stored in its compact binary form.",
    ),
    (
        6,
        2,
        "Format version",
        FieldKind::Bytes,
        "Which edition of the binary format this is; \"00\" is the one in common use.",
    ),
];

fn binary_plist(head: &[u8]) -> Option<Structure> {
    if !head.starts_with(b"bplist00") {
        return None;
    }
    structure("Property list (binary)", head, &BINARY_PLIST, text_label)
}

// ---------------------------------------------------------------------------
// Mach-O and universal binaries
// ---------------------------------------------------------------------------

const MACH_O_MAGIC: Row = (
    0,
    4,
    "Magic number",
    FieldKind::Bytes,
    "Marks the file as a Mach-O program and says whether it is 32-bit or 64-bit.",
);

const MACH_O_BIG: [Row; 4] = [
    MACH_O_MAGIC,
    (
        4,
        4,
        "CPU type",
        FieldKind::U32Be,
        "The kind of processor the program is built for.",
    ),
    (
        12,
        4,
        "File type",
        FieldKind::U32Be,
        "What kind of file this is: a program, a library, a plug-in and so on.",
    ),
    (
        16,
        4,
        "Load command count",
        FieldKind::U32Be,
        "How many instructions to the system loader follow the header.",
    ),
];

const MACH_O_LITTLE: [Row; 4] = [
    MACH_O_MAGIC,
    (
        4,
        4,
        "CPU type",
        FieldKind::U32Le,
        "The kind of processor the program is built for.",
    ),
    (
        12,
        4,
        "File type",
        FieldKind::U32Le,
        "What kind of file this is: a program, a library, a plug-in and so on.",
    ),
    (
        16,
        4,
        "Load command count",
        FieldKind::U32Le,
        "How many instructions to the system loader follow the header.",
    ),
];

/// A universal header is always big-endian; each architecture record that
/// follows it is 20 bytes, of which only the first CPU type is read.
const UNIVERSAL: [Row; 3] = [
    (
        0,
        4,
        "Magic number",
        FieldKind::Bytes,
        "Marks a universal file that holds one program for each of several processors.",
    ),
    (
        4,
        4,
        "Number of processors",
        FieldKind::U32Be,
        "How many versions of the program the file holds.",
    ),
    (
        8,
        4,
        "First version's CPU type",
        FieldKind::U32Be,
        "The kind of processor the first version in the file is built for.",
    ),
];

fn mach_o_label(name: &str, bytes: &[u8], number: Option<u64>) -> Option<String> {
    let text = match (name, bytes, number) {
        ("Magic number", [0xFE, 0xED, 0xFA, 0xCE], _) => "32-bit, high byte first",
        ("Magic number", [0xFE, 0xED, 0xFA, 0xCF], _) => "64-bit, high byte first",
        ("Magic number", [0xCE, 0xFA, 0xED, 0xFE], _) => "32-bit, low byte first",
        ("Magic number", [0xCF, 0xFA, 0xED, 0xFE], _) => "64-bit, low byte first",
        ("Magic number", [0xCA, 0xFE, 0xBA, 0xBE], _) => "universal",
        ("CPU type" | "First version's CPU type", _, Some(cpu)) => match cpu {
            7 => "x86",
            0x0100_0007 => "x86-64",
            12 => "ARM",
            0x0100_000C => "ARM64",
            18 => "PowerPC",
            0x0100_0012 => "PowerPC 64-bit",
            _ => return None,
        },
        ("File type", _, Some(kind)) => match kind {
            1 => "object file",
            2 => "executable",
            3 => "fixed VM library",
            4 => "core dump",
            5 => "preloaded executable",
            6 => "dynamic library",
            7 => "dynamic linker",
            8 => "bundle",
            9 => "stub library",
            10 => "debug symbols",
            11 => "kernel extension",
            _ => return None,
        },
        _ => return None,
    };
    Some(text.to_owned())
}

/// Most architectures a universal file is believed to hold. `CAFEBABE` is
/// also the start of every Java class file, whose next four bytes are a
/// version number (45 or more); `file(1)` makes the same cut.
const MAX_UNIVERSAL_ARCHS: u32 = 30;

fn mach_o(head: &[u8]) -> Option<Structure> {
    match slice(head, 0, 4)? {
        [0xFE, 0xED, 0xFA, 0xCE | 0xCF] => {
            structure("Program file (Mach-O)", head, &MACH_O_BIG, mach_o_label)
        }
        [0xCE | 0xCF, 0xFA, 0xED, 0xFE] => {
            structure("Program file (Mach-O)", head, &MACH_O_LITTLE, mach_o_label)
        }
        [0xCA, 0xFE, 0xBA, 0xBE] => {
            let count = u32_be(head, 4)?;
            if !(1..=MAX_UNIVERSAL_ARCHS).contains(&count) {
                return None;
            }
            structure(
                "Program file for several processors (universal)",
                head,
                &UNIVERSAL,
                mach_o_label,
            )
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// BinHex 4.0 (RFC 1741)
// ---------------------------------------------------------------------------

const BINHEX_BANNER: &[u8] = b"(This file must be converted with BinHex";

/// The banner line is the longest text looked at for its closing bracket.
const BINHEX_SEARCH: usize = 80;

fn binhex(head: &[u8]) -> Option<Structure> {
    if !head.starts_with(BINHEX_BANNER) {
        return None;
    }
    let window = head.get(..BINHEX_SEARCH.min(head.len()))?;
    let close = window.iter().position(|&b| b == b')')?;
    let end = close.checked_add(1)?;
    Some(Structure {
        title: "BinHex 4.0 text",
        fields: vec![Field {
            range: 0..u64::try_from(end).ok()?,
            name: "Banner",
            value: format!("\"{}\"", printable(window.get(..end)?)),
            note: "The line that says this text is a Mac file turned into plain letters; a BinHex program turns it back.",
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- fixtures -----------------------------------------------------------

    fn put(bytes: &mut [u8], at: usize, data: &[u8]) {
        bytes[at..at + data.len()].copy_from_slice(data);
    }

    fn resource_fork() -> Vec<u8> {
        let mut b = vec![0u8; 0x150];
        put(&mut b, 0, &0x100u32.to_be_bytes());
        put(&mut b, 4, &0x120u32.to_be_bytes());
        put(&mut b, 8, &0x20u32.to_be_bytes());
        put(&mut b, 12, &0x30u32.to_be_bytes());
        b
    }

    fn apple_double() -> Vec<u8> {
        let mut b = vec![0u8; 26 + 24];
        put(&mut b, 0, &APPLE_DOUBLE_MAGIC.to_be_bytes());
        put(&mut b, 4, &0x0002_0000u32.to_be_bytes());
        put(&mut b, 24, &2u16.to_be_bytes());
        // Entry 1: Finder info (9), at 50, 32 bytes. Entry 2: resource fork (2).
        put(&mut b, 26, &9u32.to_be_bytes());
        put(&mut b, 30, &50u32.to_be_bytes());
        put(&mut b, 34, &32u32.to_be_bytes());
        put(&mut b, 38, &2u32.to_be_bytes());
        put(&mut b, 42, &82u32.to_be_bytes());
        put(&mut b, 46, &1234u32.to_be_bytes());
        b
    }

    fn mac_binary_bytes() -> Vec<u8> {
        let mut b = vec![0u8; 128];
        b[1] = 5;
        put(&mut b, 2, b"Hello");
        put(&mut b, 65, b"TEXT");
        put(&mut b, 69, b"ttxt");
        put(&mut b, 83, &1000u32.to_be_bytes());
        put(&mut b, 87, &0u32.to_be_bytes());
        put(&mut b, 91, &(2_082_844_800u32 + 86_400 * 365).to_be_bytes());
        put(&mut b, 95, &2_082_844_800u32.to_be_bytes());
        b
    }

    fn png_bytes() -> Vec<u8> {
        let mut b = PNG_SIGNATURE.to_vec();
        b.extend(13u32.to_be_bytes());
        b.extend(b"IHDR");
        b.extend(640u32.to_be_bytes());
        b.extend(480u32.to_be_bytes());
        b.extend([8, 6, 0, 0, 0]);
        b
    }

    fn jpeg_bytes() -> Vec<u8> {
        let mut b = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        b.extend(b"JFIF\0");
        b.extend([1, 1, 0, 0, 1, 0, 1, 0, 0]);
        b
    }

    fn exif_bytes() -> Vec<u8> {
        let mut b = vec![0xFF, 0xD8, 0xFF, 0xE1, 0x1F, 0xFE];
        b.extend(b"Exif\0\0");
        b
    }

    fn gif_bytes() -> Vec<u8> {
        let mut b = b"GIF89a".to_vec();
        b.extend(320u16.to_le_bytes());
        b.extend(200u16.to_le_bytes());
        b.extend([0xF7, 0, 0]);
        b
    }

    fn tiff_le() -> Vec<u8> {
        let mut b = b"II*\0".to_vec();
        b.extend(8u32.to_le_bytes());
        b
    }

    fn tiff_be() -> Vec<u8> {
        let mut b = b"MM\0*".to_vec();
        b.extend(0x1234u32.to_be_bytes());
        b
    }

    fn zip_bytes() -> Vec<u8> {
        let mut b = b"PK\x03\x04".to_vec();
        b.extend(20u16.to_le_bytes());
        b.extend(0u16.to_le_bytes());
        b.extend(8u16.to_le_bytes());
        b.extend([0, 0, 0, 0]);
        b.extend(0xDEAD_BEEFu32.to_le_bytes());
        b.extend(100u32.to_le_bytes());
        b.extend(200u32.to_le_bytes());
        b.extend(5u16.to_le_bytes());
        b.extend(0u16.to_le_bytes());
        b.extend(b"a.txt");
        b
    }

    fn pdf_bytes() -> Vec<u8> {
        b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec()
    }

    fn plist_bytes() -> Vec<u8> {
        b"bplist00\xD1\x01\x02".to_vec()
    }

    fn mach_o_64_le() -> Vec<u8> {
        let mut b = vec![0xCF, 0xFA, 0xED, 0xFE];
        b.extend(0x0100_000Cu32.to_le_bytes());
        b.extend(0u32.to_le_bytes());
        b.extend(2u32.to_le_bytes());
        b.extend(20u32.to_le_bytes());
        b.extend([0u8; 12]);
        b
    }

    fn mach_o_32_be() -> Vec<u8> {
        let mut b = vec![0xFE, 0xED, 0xFA, 0xCE];
        b.extend(18u32.to_be_bytes());
        b.extend(0u32.to_be_bytes());
        b.extend(2u32.to_be_bytes());
        b.extend(5u32.to_be_bytes());
        b.extend([0u8; 12]);
        b
    }

    fn universal() -> Vec<u8> {
        let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE];
        b.extend(2u32.to_be_bytes());
        b.extend(7u32.to_be_bytes());
        b.extend([0u8; 16]);
        b
    }

    const BANNER: &str = "(This file must be converted with BinHex 4.0)";

    fn binhex_bytes() -> Vec<u8> {
        format!("{BANNER}\r:#h9PBA\"~").into_bytes()
    }

    /// Every layout's fixture: title, stream, bytes.
    fn fixtures() -> Vec<(&'static str, Stream, Vec<u8>)> {
        vec![
            ("Resource fork header", Stream::Resource, resource_fork()),
            ("AppleDouble companion file", Stream::Data, apple_double()),
            ("MacBinary file", Stream::Data, mac_binary_bytes()),
            ("PNG picture", Stream::Data, png_bytes()),
            ("JPEG picture", Stream::Data, jpeg_bytes()),
            ("JPEG picture", Stream::Data, exif_bytes()),
            ("GIF picture", Stream::Data, gif_bytes()),
            ("TIFF picture", Stream::Data, tiff_le()),
            ("TIFF picture", Stream::Data, tiff_be()),
            ("ZIP archive entry", Stream::Data, zip_bytes()),
            ("PDF document", Stream::Data, pdf_bytes()),
            ("Property list (binary)", Stream::Data, plist_bytes()),
            ("Program file (Mach-O)", Stream::Data, mach_o_64_le()),
            ("Program file (Mach-O)", Stream::Data, mach_o_32_be()),
            (
                "Program file for several processors (universal)",
                Stream::Data,
                universal(),
            ),
            ("BinHex 4.0 text", Stream::Data, binhex_bytes()),
        ]
    }

    // -- helpers ------------------------------------------------------------

    /// Stream length to claim for a fixture: its own, but at least what the
    /// MacBinary check needs.
    fn claimed_len(bytes: &[u8]) -> u64 {
        bytes.len() as u64
    }

    fn one(bytes: &[u8], stream: Stream, len: u64) -> Structure {
        let mut found = describe(bytes, stream, len);
        assert_eq!(found.len(), 1, "expected exactly one structure: {found:?}");
        found.remove(0)
    }

    fn data(bytes: &[u8]) -> Structure {
        one(bytes, Stream::Data, claimed_len(bytes))
    }

    fn field<'a>(s: &'a Structure, name: &str) -> &'a Field {
        s.fields
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("no field {name:?} in {s:?}"))
    }

    fn range_of(s: &Structure, name: &str) -> Range<u64> {
        field(s, name).range.clone()
    }

    fn has(s: &Structure, name: &str) -> bool {
        s.fields.iter().any(|f| f.name == name)
    }

    /// Every cut of `bytes` must be survivable, show only fields wholly
    /// inside the cut, and show nothing for 0 and 1 bytes.
    fn check_cuts(bytes: &[u8], stream: Stream, len: u64) {
        for n in 0..=bytes.len() {
            let cut = &bytes[..n];
            for s in describe(cut, stream, len) {
                assert!(!s.fields.is_empty());
                for f in &s.fields {
                    assert!(f.range.start < f.range.end);
                    assert!(
                        f.range.end <= n as u64,
                        "{} {:?} reaches past a cut at {n}",
                        s.title,
                        f
                    );
                }
            }
        }
        assert!(describe(&[], stream, len).is_empty());
        assert!(describe(&bytes[..1], stream, len).is_empty());
    }

    // -- one fixture per layout ---------------------------------------------

    #[test]
    fn resource_fork_header_is_described() {
        let bytes = resource_fork();
        let s = one(&bytes, Stream::Resource, 0x150);
        assert_eq!(s.title, "Resource fork header");
        assert_eq!(s.fields.len(), 4);
        assert_eq!(range_of(&s, "Where the resources start"), 0..4);
        assert_eq!(range_of(&s, "Where the resource map starts"), 4..8);
        assert_eq!(range_of(&s, "Length of the resources"), 8..12);
        assert_eq!(range_of(&s, "Length of the resource map"), 12..16);
        assert_eq!(field(&s, "Where the resources start").value, "256");
        assert_eq!(field(&s, "Length of the resource map").value, "48");
        assert!(
            field(&s, "Length of the resource map")
                .note
                .contains("type list")
        );
    }

    #[test]
    fn apple_double_is_described() {
        let s = data(&apple_double());
        assert_eq!(s.title, "AppleDouble companion file");
        assert_eq!(range_of(&s, "Magic number"), 0..4);
        assert_eq!(range_of(&s, "Version"), 4..8);
        assert_eq!(range_of(&s, "Number of entries"), 24..26);
        assert_eq!(range_of(&s, "Entry 1: kind"), 26..30);
        assert_eq!(range_of(&s, "Entry 1: offset"), 30..34);
        assert_eq!(range_of(&s, "Entry 2: length"), 46..50);
        assert_eq!(field(&s, "Magic number").value, "00 05 16 07 (AppleDouble)");
        assert_eq!(field(&s, "Version").value, "00 02 00 00 (version 2)");
        assert_eq!(field(&s, "Number of entries").value, "2");
        assert_eq!(field(&s, "Entry 1: kind").value, "9 (Finder info)");
        assert_eq!(field(&s, "Entry 2: kind").value, "2 (resource fork)");
        assert_eq!(field(&s, "Entry 2: offset").value, "82");
        assert!(!has(&s, "Entry 3: kind"));
    }

    #[test]
    fn apple_single_has_its_own_title() {
        let mut b = apple_double();
        put(&mut b, 0, &APPLE_SINGLE_MAGIC.to_be_bytes());
        let s = data(&b);
        assert_eq!(s.title, "AppleSingle file");
        assert_eq!(field(&s, "Magic number").value, "00 05 16 00 (AppleSingle)");
    }

    #[test]
    fn apple_entry_table_is_bounded() {
        // A count of 65535 with 40 entries' worth of bytes: only 16 are read.
        let mut b = vec![0u8; ENTRIES_AT + 40 * ENTRY_SIZE];
        put(&mut b, 0, &APPLE_SINGLE_MAGIC.to_be_bytes());
        put(&mut b, 24, &u16::MAX.to_be_bytes());
        for i in 0..40 {
            put(&mut b, ENTRIES_AT + i * ENTRY_SIZE, &1u32.to_be_bytes());
        }
        let s = data(&b);
        assert_eq!(s.fields.len(), 3 + MAX_ENTRIES * 3);
        assert!(has(&s, "Entry 16: length"));
        // Entries are read only while their 12 bytes are inside the head.
        let s = data(&b[..ENTRIES_AT + ENTRY_SIZE + 11]);
        assert!(!has(&s, "Entry 2: length"));
        assert!(!has(&s, "Entry 2: kind"));
        assert!(has(&s, "Entry 1: length"));
    }

    #[test]
    fn mac_binary_is_described() {
        let s = data(&mac_binary_bytes());
        assert_eq!(s.title, "MacBinary file");
        assert_eq!(range_of(&s, "File name"), 1..65);
        assert_eq!(range_of(&s, "File type"), 65..69);
        assert_eq!(range_of(&s, "Creator"), 69..73);
        assert_eq!(range_of(&s, "Data fork length"), 83..87);
        assert_eq!(range_of(&s, "Resource fork length"), 87..91);
        assert_eq!(range_of(&s, "Created"), 91..95);
        assert_eq!(range_of(&s, "Modified"), 95..99);
        assert_eq!(field(&s, "File name").value, "\"Hello\"");
        assert_eq!(field(&s, "File type").value, "'TEXT'");
        assert_eq!(field(&s, "Creator").value, "'ttxt'");
        assert_eq!(field(&s, "Data fork length").value, "1000");
        assert_eq!(field(&s, "Created").value, "1971-01-01 00:00:00 UTC");
        assert_eq!(field(&s, "Modified").value, "1970-01-01 00:00:00 UTC");
    }

    #[test]
    fn mac_binary_fails_each_check_in_turn() {
        let good = mac_binary_bytes();
        let len = 128;
        assert_eq!(describe(&good, Stream::Data, len).len(), 1);
        // Byte 0 not zero.
        let mut b = good.clone();
        b[0] = 1;
        assert!(describe(&b, Stream::Data, len).is_empty());
        // Name length 0 and 64.
        for bad in [0u8, 64] {
            let mut b = good.clone();
            b[1] = bad;
            assert!(describe(&b, Stream::Data, len).is_empty());
        }
        // Byte 74 not zero.
        let mut b = good.clone();
        b[74] = 1;
        assert!(describe(&b, Stream::Data, len).is_empty());
        // Byte 82 not zero.
        let mut b = good.clone();
        b[82] = 1;
        assert!(describe(&b, Stream::Data, len).is_empty());
        // Stream shorter than a header.
        assert!(describe(&good, Stream::Data, 127).is_empty());
    }

    #[test]
    fn png_is_described() {
        let s = data(&png_bytes());
        assert_eq!(s.title, "PNG picture");
        assert_eq!(range_of(&s, "Signature"), 0..8);
        assert_eq!(range_of(&s, "Header chunk length"), 8..12);
        assert_eq!(range_of(&s, "Width"), 16..20);
        assert_eq!(range_of(&s, "Height"), 20..24);
        assert_eq!(range_of(&s, "Bit depth"), 24..25);
        assert_eq!(range_of(&s, "Colour type"), 25..26);
        assert_eq!(field(&s, "Width").value, "640");
        assert_eq!(field(&s, "Height").value, "480");
        assert_eq!(field(&s, "Bit depth").value, "8");
        assert_eq!(
            field(&s, "Colour type").value,
            "6 (colour with transparency)"
        );
        assert_eq!(
            field(&s, "Width").note,
            "How many pixels wide the picture is."
        );
    }

    #[test]
    fn jpeg_is_described() {
        let s = data(&jpeg_bytes());
        assert_eq!(s.title, "JPEG picture");
        assert_eq!(range_of(&s, "Start marker"), 0..2);
        assert_eq!(range_of(&s, "First marker"), 2..4);
        assert_eq!(range_of(&s, "First marker length"), 4..6);
        assert_eq!(range_of(&s, "Block identifier"), 6..10);
        assert_eq!(field(&s, "Start marker").value, "FF D8 (start of picture)");
        assert_eq!(field(&s, "First marker").value, "FF E0 (JFIF header)");
        assert_eq!(field(&s, "First marker length").value, "16");
        assert_eq!(field(&s, "Block identifier").value, "'JFIF'");

        let e = data(&exif_bytes());
        assert_eq!(field(&e, "Block identifier").value, "'Exif'");
        assert_eq!(field(&e, "First marker length").value, "8190");
    }

    #[test]
    fn jpeg_without_a_marker_or_identifier() {
        // FF D8 then no marker: only the start is described.
        let s = data(&[0xFF, 0xD8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(s.fields.len(), 1);
        // A marker that stands alone has no length.
        let s = data(&[0xFF, 0xD8, 0xFF, 0xD9, 0x12, 0x34]);
        assert!(has(&s, "First marker"));
        assert!(!has(&s, "First marker length"));
        // JFIF marker without the identifier.
        let s = data(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 16, b'N', b'O', b'P', b'E', 0]);
        assert!(!has(&s, "Block identifier"));
    }

    #[test]
    fn gif_is_described() {
        let s = data(&gif_bytes());
        assert_eq!(s.title, "GIF picture");
        assert_eq!(range_of(&s, "Signature"), 0..6);
        assert_eq!(range_of(&s, "Width"), 6..8);
        assert_eq!(range_of(&s, "Height"), 8..10);
        assert_eq!(range_of(&s, "Flags"), 10..11);
        assert_eq!(field(&s, "Width").value, "320");
        assert_eq!(field(&s, "Height").value, "200");
        assert_eq!(
            field(&s, "Flags").value,
            "247 (shared colour table of 256 colours)"
        );
        assert!(field(&s, "Signature").value.ends_with("(\"GIF89a\")"));
        let mut b = gif_bytes();
        b[10] = 0x07;
        assert!(
            field(&data(&b), "Flags")
                .value
                .contains("no shared colour table")
        );
    }

    #[test]
    fn tiff_is_described_in_both_byte_orders() {
        let s = data(&tiff_le());
        assert_eq!(s.title, "TIFF picture");
        assert_eq!(range_of(&s, "Byte order"), 0..2);
        assert_eq!(range_of(&s, "Magic number"), 2..4);
        assert_eq!(range_of(&s, "First directory offset"), 4..8);
        assert_eq!(field(&s, "Byte order").value, "49 49 (lowest byte first)");
        assert_eq!(field(&s, "Magic number").value, "42");
        assert_eq!(field(&s, "First directory offset").value, "8");

        let s = data(&tiff_be());
        assert_eq!(field(&s, "Byte order").value, "4D 4D (highest byte first)");
        assert_eq!(field(&s, "Magic number").value, "42");
        assert_eq!(field(&s, "First directory offset").value, "4660");
    }

    #[test]
    fn zip_is_described() {
        let s = data(&zip_bytes());
        assert_eq!(s.title, "ZIP archive entry");
        assert_eq!(range_of(&s, "Version needed"), 4..6);
        assert_eq!(range_of(&s, "Flags"), 6..8);
        assert_eq!(range_of(&s, "Compression method"), 8..10);
        assert_eq!(range_of(&s, "CRC-32"), 14..18);
        assert_eq!(range_of(&s, "Compressed size"), 18..22);
        assert_eq!(range_of(&s, "Original size"), 22..26);
        assert_eq!(range_of(&s, "File name length"), 26..28);
        assert_eq!(field(&s, "Version needed").value, "20 (2.0)");
        assert_eq!(field(&s, "Compression method").value, "8 (deflated)");
        assert_eq!(field(&s, "CRC-32").value, "3735928559 (0xDEADBEEF)");
        assert_eq!(field(&s, "Compressed size").value, "100");
        assert_eq!(field(&s, "Original size").value, "200");
        assert_eq!(field(&s, "File name length").value, "5");
    }

    #[test]
    fn pdf_is_described() {
        let s = data(&pdf_bytes());
        assert_eq!(s.title, "PDF document");
        assert_eq!(range_of(&s, "Header"), 0..5);
        assert_eq!(range_of(&s, "Version"), 5..8);
        assert_eq!(field(&s, "Version").value, "31 2E 37 (\"1.7\")");
        assert_eq!(field(&s, "Header").value, "25 50 44 46 2D (\"%PDF-\")");
    }

    #[test]
    fn binary_plist_is_described() {
        let s = data(&plist_bytes());
        assert_eq!(s.title, "Property list (binary)");
        assert_eq!(range_of(&s, "Magic number"), 0..6);
        assert_eq!(range_of(&s, "Format version"), 6..8);
        assert_eq!(
            field(&s, "Magic number").value,
            "62 70 6C 69 73 74 (\"bplist\")"
        );
        assert_eq!(field(&s, "Format version").value, "30 30 (\"00\")");
    }

    #[test]
    fn mach_o_is_described() {
        let s = data(&mach_o_64_le());
        assert_eq!(s.title, "Program file (Mach-O)");
        assert_eq!(range_of(&s, "Magic number"), 0..4);
        assert_eq!(range_of(&s, "CPU type"), 4..8);
        assert_eq!(range_of(&s, "File type"), 12..16);
        assert_eq!(range_of(&s, "Load command count"), 16..20);
        assert_eq!(
            field(&s, "Magic number").value,
            "CF FA ED FE (64-bit, low byte first)"
        );
        assert_eq!(field(&s, "CPU type").value, "16777228 (ARM64)");
        assert_eq!(field(&s, "File type").value, "2 (executable)");
        assert_eq!(field(&s, "Load command count").value, "20");

        let s = data(&mach_o_32_be());
        assert_eq!(
            field(&s, "Magic number").value,
            "FE ED FA CE (32-bit, high byte first)"
        );
        assert_eq!(field(&s, "CPU type").value, "18 (PowerPC)");
        assert_eq!(field(&s, "Load command count").value, "5");
    }

    #[test]
    fn mach_o_other_byte_order_and_width() {
        // 32-bit little-endian and 64-bit big-endian forms are recognised.
        for magic in [[0xCE, 0xFA, 0xED, 0xFE], [0xFE, 0xED, 0xFA, 0xCF]] {
            let mut b = mach_o_32_be();
            put(&mut b, 0, &magic);
            assert_eq!(data(&b).title, "Program file (Mach-O)");
        }
        // Unknown numbers are shown as the number alone.
        let mut b = mach_o_32_be();
        put(&mut b, 4, &9999u32.to_be_bytes());
        put(&mut b, 12, &99u32.to_be_bytes());
        let s = data(&b);
        assert_eq!(field(&s, "CPU type").value, "9999");
        assert_eq!(field(&s, "File type").value, "99");
    }

    #[test]
    fn universal_is_described() {
        let s = data(&universal());
        assert_eq!(s.title, "Program file for several processors (universal)");
        assert_eq!(range_of(&s, "Magic number"), 0..4);
        assert_eq!(range_of(&s, "Number of processors"), 4..8);
        assert_eq!(range_of(&s, "First version's CPU type"), 8..12);
        assert_eq!(field(&s, "Magic number").value, "CA FE BA BE (universal)");
        assert_eq!(field(&s, "Number of processors").value, "2");
        assert_eq!(field(&s, "First version's CPU type").value, "7 (x86)");
    }

    #[test]
    fn binhex_is_described() {
        let s = data(&binhex_bytes());
        assert_eq!(s.title, "BinHex 4.0 text");
        assert_eq!(s.fields.len(), 1);
        assert_eq!(range_of(&s, "Banner"), 0..BANNER.len() as u64);
        assert_eq!(field(&s, "Banner").value, format!("\"{BANNER}\""));
    }

    // -- truncation ---------------------------------------------------------

    #[test]
    fn every_layout_survives_every_cut() {
        for (title, stream, bytes) in fixtures() {
            let len = claimed_len(&bytes);
            assert_eq!(describe(&bytes, stream, len)[0].title, title);
            check_cuts(&bytes, stream, len);
        }
    }

    #[test]
    fn fields_not_wholly_inside_the_cut_are_omitted() {
        let png = png_bytes();
        let s = data(&png[..23]);
        assert!(has(&s, "Width"));
        assert!(!has(&s, "Height"), "Height ends at 24");
        let s = data(&png[..8]);
        assert_eq!(s.fields.len(), 1);
        assert_eq!(s.fields[0].name, "Signature");

        let zip = zip_bytes();
        let s = data(&zip[..27]);
        assert!(has(&s, "Original size"));
        assert!(!has(&s, "File name length"));

        let mb = mac_binary_bytes();
        let s = one(&mb[..90], Stream::Data, 128);
        assert!(has(&s, "Data fork length"));
        assert!(!has(&s, "Resource fork length"));

        // Too short to see the second number of a universal header.
        assert!(describe(&universal()[..7], Stream::Data, 100).is_empty());
        // BinHex banner cut before its closing bracket has no field to show.
        assert!(describe(&binhex_bytes()[..BANNER.len() - 1], Stream::Data, 100).is_empty());
    }

    #[test]
    fn describe_never_reads_past_head() {
        // The stream is long, the head short: only what is in head is used.
        let png = png_bytes();
        let s = one(&png[..20], Stream::Data, 1 << 40);
        assert!(s.fields.iter().all(|f| f.range.end <= 20));
        // A claimed length of u64::MAX does not overflow the resource check.
        let mut b = resource_fork();
        put(&mut b, 8, &u32::MAX.to_be_bytes());
        put(&mut b, 12, &u32::MAX.to_be_bytes());
        assert!(describe(&b, Stream::Resource, u64::MAX).is_empty());
    }

    // -- near misses --------------------------------------------------------

    fn assert_none(bytes: &[u8]) {
        assert!(
            describe(bytes, Stream::Data, bytes.len() as u64 + 4096).is_empty(),
            "should not be recognised: {bytes:02X?}"
        );
    }

    #[test]
    fn resource_fork_header_near_misses() {
        let good = resource_fork();
        // Only read for the resource stream.
        assert!(describe(&good, Stream::Data, 0x150).is_empty());
        // The map would end past the stream.
        assert!(describe(&good, Stream::Resource, 0x14F).is_empty());
        // Data and map overlap.
        let mut b = good.clone();
        put(&mut b, 4, &0x110u32.to_be_bytes());
        assert!(describe(&b, Stream::Resource, 0x150).is_empty());
        // Map too small to hold its own header.
        let mut b = good.clone();
        put(&mut b, 12, &4u32.to_be_bytes());
        assert!(describe(&b, Stream::Resource, 0x150).is_empty());
        // Offsets inside the header itself.
        let mut b = good.clone();
        put(&mut b, 0, &8u32.to_be_bytes());
        assert!(describe(&b, Stream::Resource, 0x150).is_empty());
        // Sums past what a u32 holds are made in u64 and refused against len.
        let mut b = good.clone();
        put(&mut b, 0, &u32::MAX.to_be_bytes());
        put(&mut b, 8, &u32::MAX.to_be_bytes());
        assert!(describe(&b, Stream::Resource, 0x150).is_empty());
        // All zeros.
        assert!(describe(&[0u8; 256], Stream::Resource, 256).is_empty());
    }

    #[test]
    fn a_resource_fork_without_a_header_match_falls_through() {
        // A PNG as a resource stream is still described as a PNG.
        let s = one(&png_bytes(), Stream::Resource, 33);
        assert_eq!(s.title, "PNG picture");
    }

    #[test]
    fn apple_double_near_misses() {
        for magic in [0x0005_1601u32, 0x0005_1606, 0x0105_1600, 0x0005_1700] {
            let mut b = apple_double();
            put(&mut b, 0, &magic.to_be_bytes());
            // Not AppleSingle; may only be MacBinary-like, which needs 128 bytes.
            assert_none(&b);
        }
    }

    #[test]
    fn apple_single_is_not_mistaken_for_mac_binary() {
        // Zero bytes at 74 and 82 and a 128-byte stream: the magic wins.
        let mut b = vec![0u8; 200];
        put(&mut b, 0, &APPLE_SINGLE_MAGIC.to_be_bytes());
        assert_eq!(data(&b).title, "AppleSingle file");
    }

    #[test]
    fn png_near_misses() {
        let good = png_bytes();
        for i in 0..8 {
            let mut b = good.clone();
            b[i] ^= 0x01;
            assert_none(&b);
        }
        assert_none(&good[..7]);
    }

    #[test]
    fn jpeg_near_misses() {
        assert_none(&[0xFF, 0xD9, 0xFF, 0xE0]);
        assert_none(&[0xD8, 0xFF, 0xE0, 0x00]);
        assert_none(&[0xFE, 0xD8, 0xFF, 0xE0]);
    }

    #[test]
    fn gif_near_misses() {
        for bad in [&b"GIF88a"[..], b"GIF89b", b"GIF8", b"gif89a", b"GIF87A"] {
            let mut b = bad.to_vec();
            b.extend([0u8; 8]);
            assert_none(&b);
        }
    }

    #[test]
    fn tiff_near_misses() {
        // BigTIFF (43), swapped marks, a wrong magic number.
        assert_none(b"II+\0\x08\0\0\0");
        assert_none(b"MM\0+\0\0\0\x08");
        assert_none(b"IM*\0\x08\0\0\0");
        assert_none(b"II\0*\x08\0\0\0");
        assert_none(b"MM*\0\0\0\0\x08");
    }

    #[test]
    fn zip_near_misses() {
        // End of central directory, spanned archive marker, a wrong byte.
        assert_none(b"PK\x05\x06\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0");
        assert_none(b"PK\x07\x08\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0");
        assert_none(b"PK\x03\x05\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0");
        assert_none(b"pk\x03\x04\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0");
    }

    #[test]
    fn pdf_near_misses() {
        assert_none(b"%PDF1.7\n");
        assert_none(b"%pdf-1.7\n");
        assert_none(b" %PDF-1.7\n");
        assert_none(b"%PD");
    }

    #[test]
    fn binary_plist_near_misses() {
        assert_none(b"bplist01\0\0");
        assert_none(b"bplist0");
        assert_none(b"Bplist00");
        assert_none(b"<?xml version");
    }

    #[test]
    fn mach_o_near_misses() {
        assert_none(&[0xFE, 0xED, 0xFA, 0xCD, 0, 0, 0, 0]);
        assert_none(&[0xFE, 0xED, 0xFA, 0xD0, 0, 0, 0, 0]);
        assert_none(&[0xCE, 0xFA, 0xED, 0xFD, 0, 0, 0, 0]);
        // Fat headers are always stored big-endian: the swapped form never
        // appears on disk.
        assert_none(&[0xBE, 0xBA, 0xFE, 0xCA, 0, 0, 0, 2]);
        // 64-bit fat magic is not part of this layout.
        assert_none(&[0xCA, 0xFE, 0xBA, 0xBF, 0, 0, 0, 2]);
    }

    #[test]
    fn java_class_is_not_universal() {
        // Class file: CAFEBABE, minor 0, major 52 (Java 8).
        assert_none(&[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52, 0, 0]);
        // 31 is the first count refused; 30 is the last allowed; 0 is empty.
        assert_none(&[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 31, 0, 0, 0, 0]);
        assert_none(&[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            data(&[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 30, 0, 0, 0, 7])
                .fields
                .len(),
            3
        );
    }

    #[test]
    fn binhex_near_misses() {
        assert_none(b"(This file must be converted with BinHux 4.0)\r:");
        assert_none(b"This file must be converted with BinHex 4.0)\r:");
        assert_none(b" (This file must be converted with BinHex 4.0)\r:");
        // The banner never closes inside the 80 bytes looked at.
        let mut long = b"(This file must be converted with BinHex".to_vec();
        long.extend(vec![b'x'; 100]);
        long.push(b')');
        assert_none(&long);
    }

    // -- value decoding -----------------------------------------------------

    #[test]
    fn mac_dates() {
        assert_eq!(mac_date(0), "not set");
        assert_eq!(mac_date(2_082_844_800), "1970-01-01 00:00:00 UTC");
        assert_eq!(mac_date(1), "1904-01-01 00:00:01 UTC");
        assert_eq!(mac_date(2_082_844_800 + 86_399), "1970-01-01 23:59:59 UTC");
        assert!(mac_date(u64::from(u32::MAX)).starts_with("2040-"));
        // Out of anything chrono can show: never panics.
        assert_eq!(mac_date(u64::MAX), "not a valid date");
    }

    #[test]
    fn fixed_point_values() {
        assert_eq!(fixed_16_16(0x0001_0000), "1");
        assert_eq!(fixed_16_16(0x0001_8000), "1.5");
        assert_eq!(fixed_16_16(0), "0");
        assert_eq!(fixed_16_16(0x0000_0001), "0");
        assert_eq!(fixed_16_16(0x0000_1000), "0.0625");
        // -1.5
        assert_eq!(fixed_16_16(0xFFFE_8000), "-1.5");
        assert_eq!(fixed_16_16(0xFFFF_FFFF), "0");
        assert_eq!(fixed_16_16(0x7FFF_FFFF), "32768");
    }

    #[test]
    fn every_kind_reads_its_bytes() {
        use FieldKind::*;
        let head: [u8; 20] = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, b'T', b'E', b'X', 0x00, 3, b'a', b'b',
            b'c', 0x00, 0x01, 0x80, 0x00,
        ];
        let rows: [(Row, &str); 12] = [
            ((0, 1, "a", U8, ""), "1"),
            ((0, 2, "b", U16Be, ""), "258"),
            ((0, 2, "c", U16Le, ""), "513"),
            ((0, 4, "d", U32Be, ""), "16909060"),
            ((0, 4, "e", U32Le, ""), "67305985"),
            ((0, 8, "f", U64Be, ""), "72623859790382856"),
            ((0, 8, "g", U64Le, ""), "578437695752307201"),
            ((8, 4, "h", FourCc, ""), "'TEX.'"),
            (
                (8, 12, "i", Bytes, ""),
                "54 45 58 00 03 61 62 63 00 01 80 00",
            ),
            ((12, 4, "j", PascalString, ""), "\"abc\""),
            ((16, 4, "k", Fixed16_16, ""), "1.5"),
            // 0x00018000 seconds is 27 h 18 min 24 s.
            ((16, 4, "l", MacDate, ""), "1904-01-02 03:18:24 UTC"),
        ];
        for (row, expected) in rows {
            let got = read_field(&head, &row, no_label);
            assert_eq!(got.expect(row.2).value, expected, "row {}", row.2);
        }
        // Past the end of the head: not read.
        assert!(read_field(&head, &(8, 13, "i", Bytes, ""), no_label).is_none());
        // More than 16 bytes shows 16 and an ellipsis.
        let long = [0xABu8; 20];
        let f = read_field(&long, &(0, 20, "x", Bytes, ""), no_label).unwrap();
        assert_eq!(f.value, format!("{} …", ["AB"; 16].join(" ")));
        // Exactly 16 shows no ellipsis.
        let f = read_field(&long, &(0, 16, "x", Bytes, ""), no_label).unwrap();
        assert!(!f.value.contains('…'));
        // A row whose length disagrees with its kind is refused.
        assert!(read_field(&head, &(0, 3, "x", U32Be, ""), no_label).is_none());
        // Offsets that would overflow are refused, not wrapped.
        assert!(read_field(&head, &(usize::MAX, 2, "x", Bytes, ""), no_label).is_none());
    }

    #[test]
    fn text_values_show_dots_for_unprintable_bytes() {
        assert_eq!(printable(b"a\x00b\xFFc\x7F "), "a.b.c. ");
        assert_eq!(pascal(&[0, b'x']), "\"\"");
        // The count is larger than the field: clamp to the field.
        assert_eq!(pascal(&[200, b'x', b'y']), "\"xy\"");
        assert_eq!(pascal(&[2, 0x8A, b'y', b'z']), "\".y\"");
        assert_eq!(pascal(&[]), "\"\"");
    }

    #[test]
    fn table_rows_agree_with_their_kinds() {
        let tables: [&[Row]; 12] = [
            &RESOURCE_FORK,
            &APPLE_SINGLE,
            &MAC_BINARY,
            &PNG,
            &GIF,
            &TIFF_LITTLE,
            &TIFF_BIG,
            &ZIP,
            &PDF,
            &BINARY_PLIST,
            &MACH_O_BIG,
            &UNIVERSAL,
        ];
        let others: [Row; 4] = [JPEG_START, JPEG_MARKER, JPEG_MARKER_LENGTH, JPEG_IDENTIFIER];
        for row in tables.iter().flat_map(|t| t.iter()).chain(others.iter()) {
            if let Some(width) = row.3.width() {
                assert_eq!(width, row.1, "{}", row.2);
            }
            assert!(!row.2.is_empty() && !row.4.is_empty());
            assert!(row.4.ends_with('.'), "{}", row.2);
        }
        for (big, little) in MACH_O_BIG.iter().zip(MACH_O_LITTLE.iter()) {
            assert_eq!((big.0, big.1, big.2), (little.0, little.1, little.2));
        }
        // Rows inside one table are in file order and do not overlap.
        for table in tables {
            for pair in table.windows(2) {
                assert!(pair[0].0 + pair[0].1 <= pair[1].0, "{}", pair[1].2);
            }
        }
    }

    // -- hostile input ------------------------------------------------------

    /// A small deterministic generator (xorshift64*), so the sweep repeats.
    struct Rng(u64);

    impl Rng {
        fn step(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.step() % n as u64) as usize
        }
    }

    #[test]
    fn random_heads_never_panic_and_stay_inside() {
        let seeds: Vec<Vec<u8>> = fixtures().into_iter().map(|(_, _, b)| b).collect();
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let mut recognised = 0u32;
        for round in 0..6000 {
            // Half pure noise, half a fixture's start with bytes disturbed.
            let mut head: Vec<u8> = if round % 2 == 0 {
                (0..rng.below(300)).map(|_| rng.step() as u8).collect()
            } else {
                let mut base = seeds[rng.below(seeds.len())].clone();
                base.truncate(rng.below(base.len() + 1));
                for _ in 0..rng.below(4) {
                    if !base.is_empty() {
                        let at = rng.below(base.len());
                        base[at] = rng.step() as u8;
                    }
                }
                base
            };
            // Sometimes pad to a realistic head of zeros and noise.
            if round % 7 == 0 {
                let extra = rng.below(4096);
                head.extend((0..extra).map(|_| rng.step() as u8));
            }
            let len = match rng.below(5) {
                0 => 0,
                1 => head.len() as u64,
                2 => u64::MAX,
                3 => 128,
                _ => rng.step(),
            };
            for stream in [Stream::Data, Stream::Resource] {
                for s in describe(&head, stream, len) {
                    recognised += 1;
                    assert!(!s.fields.is_empty());
                    for f in &s.fields {
                        assert!(f.range.start < f.range.end);
                        assert!(f.range.end <= head.len() as u64, "{} {:?}", s.title, f);
                    }
                }
            }
        }
        // The disturbed fixtures keep the sweep from being vacuous.
        assert!(recognised > 100, "only {recognised} recognised");
    }
}
