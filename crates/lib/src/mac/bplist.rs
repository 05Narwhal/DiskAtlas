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

//! Reader for binary property lists (`bplist00`), the format of Apple's
//! extended attributes and Finder view settings. The output is the same
//! [`Value`] tree the XML reader gives. Source: `r10-everyday.md` section
//! 5.3, written from the published layout of `CFBinaryPList.c`.
//!
//! Every position, count and object number is checked against the input;
//! an unknown marker is an error. Bounds: input [`MAX_BPLIST`], objects
//! [`MAX_OBJECTS`] (counted as decoded, so a shared object is paid for each
//! time it is used), depth [`MAX_DEPTH`], a string [`MAX_STRING`] bytes. A
//! container that holds itself, directly or through others, is an error.

use std::collections::HashSet;

use crate::error::{CorruptKind, Result};
use crate::image::plist::Value;

use super::util::{be16, be32, be64, corrupt, corrupt_text, len64, to_usize, truncated};

const WHAT: &str = "binary property list";

/// Most bytes of input.
pub const MAX_BPLIST: usize = 1 << 20;
/// Most objects the table may declare, and most decoded.
pub const MAX_OBJECTS: usize = 65_536;
/// Deepest nesting of containers.
pub const MAX_DEPTH: usize = 32;
/// Longest string, in bytes.
pub const MAX_STRING: usize = 64 * 1024;

const MAGIC: &[u8; 8] = b"bplist00";
const TRAILER: usize = 32;

/// Whether `bytes` start like a binary property list.
pub fn is_bplist(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
}

/// Parses a property list of either kind: binary by its magic, otherwise XML.
pub fn parse_any(bytes: &[u8]) -> Result<Value> {
    if is_bplist(bytes) {
        parse(bytes)
    } else {
        crate::image::plist::parse(bytes)
    }
}

fn bad(offset: u64, field: &'static str, value: u64) -> crate::error::Error {
    corrupt(WHAT, offset, CorruptKind::InvalidValue { field, value })
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset_size: usize,
    ref_size: usize,
    count: usize,
    table: usize,
    /// Objects decoded so far.
    produced: usize,
    /// Containers being decoded now, to refuse one that holds itself.
    open: HashSet<usize>,
}

/// Parses a binary property list.
pub fn parse(bytes: &[u8]) -> Result<Value> {
    if bytes.len() > MAX_BPLIST {
        return Err(corrupt(
            WHAT,
            0,
            CorruptKind::OutOfRange {
                field: "size",
                value: len64(bytes),
                limit_name: "largest property list",
                limit: MAX_BPLIST as u64,
            },
        ));
    }
    if !is_bplist(bytes) {
        return Err(corrupt(
            WHAT,
            0,
            CorruptKind::BadSignature {
                expected: u64::from_be_bytes(*MAGIC),
                found: bytes
                    .first_chunk::<8>()
                    .map_or(0, |magic| u64::from_be_bytes(*magic)),
            },
        ));
    }
    let Some(trailer_at) = bytes
        .len()
        .checked_sub(TRAILER)
        .filter(|&at| at >= MAGIC.len())
    else {
        return Err(truncated(WHAT, 0, 8 + TRAILER as u64, len64(bytes)));
    };
    let offset_size = usize::from(bytes.get(trailer_at.saturating_add(6)).copied().unwrap_or(0));
    let ref_size = usize::from(bytes.get(trailer_at.saturating_add(7)).copied().unwrap_or(0));
    for (field, size) in [("offset size", offset_size), ("reference size", ref_size)] {
        if !matches!(size, 1 | 2 | 4 | 8) {
            return Err(bad(trailer_at as u64, field, size as u64));
        }
    }
    let count = be64(bytes, trailer_at.saturating_add(8)).unwrap_or(0);
    let root = be64(bytes, trailer_at.saturating_add(16)).unwrap_or(0);
    let table = be64(bytes, trailer_at.saturating_add(24)).unwrap_or(0);
    if count == 0 || count > MAX_OBJECTS as u64 {
        return Err(corrupt(
            WHAT,
            trailer_at as u64,
            CorruptKind::OutOfRange {
                field: "object count",
                value: count,
                limit_name: "most objects",
                limit: MAX_OBJECTS as u64,
            },
        ));
    }
    if root >= count {
        return Err(bad(trailer_at as u64, "root object", root));
    }
    let (count, root, table) = (to_usize(count), to_usize(root), to_usize(table));
    let table_end = count
        .checked_mul(offset_size)
        .and_then(|len| table.checked_add(len));
    match table_end {
        Some(end) if table >= MAGIC.len() && end <= trailer_at => {}
        _ => return Err(bad(trailer_at as u64, "offset table position", table as u64)),
    }
    let mut reader = Reader {
        bytes,
        offset_size,
        ref_size,
        count,
        table,
        produced: 0,
        open: HashSet::new(),
    };
    reader.object(root, 1)
}

impl Reader<'_> {
    /// The unsigned big-endian integer of `size` bytes (1, 2, 4 or 8) at `at`.
    fn uint(&self, at: usize, size: usize) -> Option<u64> {
        let end = at.checked_add(size)?;
        let slice = self.bytes.get(at..end)?;
        match size {
            1 => slice.first().map(|&b| u64::from(b)),
            2 => be16(slice, 0).map(u64::from),
            4 => be32(slice, 0).map(u64::from),
            8 => be64(slice, 0),
            _ => None,
        }
    }

    /// Where object `number` starts.
    fn position(&self, number: usize) -> Result<usize> {
        let at = number
            .checked_mul(self.offset_size)
            .and_then(|n| n.checked_add(self.table));
        let offset = at
            .and_then(|at| self.uint(at, self.offset_size))
            .map(to_usize);
        match offset {
            Some(offset) if offset >= MAGIC.len() && offset < self.table => Ok(offset),
            _ => Err(bad(self.table as u64, "object offset", number as u64)),
        }
    }

    /// The count after a marker: the low nibble, or the integer object that
    /// follows when the nibble is 15. Returns it and where the content starts.
    fn count_after(&self, low: u8, at: usize) -> Result<(usize, usize)> {
        if low != 0x0F {
            return Ok((usize::from(low), at.saturating_add(1)));
        }
        let marker_at = at.saturating_add(1);
        let marker = self.bytes.get(marker_at).copied().unwrap_or(0);
        let size = match marker {
            0x10 => 1,
            0x11 => 2,
            0x12 => 4,
            0x13 => 8,
            _ => return Err(bad(marker_at as u64, "count marker", u64::from(marker))),
        };
        let value = self
            .uint(marker_at.saturating_add(1), size)
            .ok_or_else(|| truncated(WHAT, marker_at as u64, size as u64, len64(self.bytes)))?;
        Ok((to_usize(value), marker_at.saturating_add(1).saturating_add(size)))
    }

    /// `len` bytes at `at`, inside the object area.
    fn slice(&self, at: usize, len: usize) -> Result<&[u8]> {
        at.checked_add(len)
            .filter(|&end| end <= self.table)
            .and_then(|end| self.bytes.get(at..end))
            .ok_or_else(|| truncated(WHAT, at as u64, len as u64, len64(self.bytes)))
    }

    fn string_len(&self, units: usize, at: usize, width: usize) -> Result<usize> {
        let len = units.checked_mul(width).filter(|&len| len <= MAX_STRING);
        len.ok_or_else(|| {
            corrupt(
                WHAT,
                at as u64,
                CorruptKind::OutOfRange {
                    field: "string length",
                    value: units as u64,
                    limit_name: "longest string",
                    limit: MAX_STRING as u64,
                },
            )
        })
    }

    fn object(&mut self, number: usize, depth: usize) -> Result<Value> {
        if depth > MAX_DEPTH {
            return Err(corrupt(
                WHAT,
                self.table as u64,
                CorruptKind::LimitExceeded {
                    what: "nesting depth",
                    limit: MAX_DEPTH as u64,
                },
            ));
        }
        self.produced = self.produced.saturating_add(1);
        if self.produced > MAX_OBJECTS {
            return Err(corrupt(
                WHAT,
                self.table as u64,
                CorruptKind::LimitExceeded {
                    what: "objects decoded",
                    limit: MAX_OBJECTS as u64,
                },
            ));
        }
        if number >= self.count {
            return Err(bad(self.table as u64, "object number", number as u64));
        }
        let at = self.position(number)?;
        let marker = self.bytes.get(at).copied().unwrap_or(0);
        let (high, low) = (marker >> 4, marker & 0x0F);
        match (high, low) {
            (0x0, 0x0 | 0x8 | 0x9) => Ok(match low {
                0x8 => Value::Bool(false),
                0x9 => Value::Bool(true),
                // Null has no counterpart in the tree: an empty string stands in.
                _ => Value::String(String::new()),
            }),
            (0x1, n @ 0..=3) => {
                let size = 1usize << n;
                let raw = self
                    .uint(at.saturating_add(1), size)
                    .ok_or_else(|| truncated(WHAT, at as u64, size as u64, len64(self.bytes)))?;
                Ok(Value::Integer(if n == 3 {
                    // Eight-byte integers are signed.
                    i128::from(i64::from_be_bytes(raw.to_be_bytes()))
                } else {
                    i128::from(raw)
                }))
            }
            (0x2, 0x2 | 0x3) => {
                let size = if low == 2 { 4 } else { 8 };
                let slice = self.slice(at.saturating_add(1), size)?;
                let real = if size == 4 {
                    be32(slice, 0).map(|bits| f64::from(f32::from_bits(bits)))
                } else {
                    be64(slice, 0).map(f64::from_bits)
                };
                real.map(Value::Real)
                    .ok_or_else(|| truncated(WHAT, at as u64, size as u64, len64(self.bytes)))
            }
            (0x3, 0x3) => {
                let slice = self.slice(at.saturating_add(1), 8)?;
                let seconds = be64(slice, 0).map_or(f64::NAN, f64::from_bits);
                iso_date(seconds)
                    .map(Value::Date)
                    .ok_or_else(|| bad(at as u64, "date", 0))
            }
            (0x4, n) => {
                let (len, start) = self.count_after(n, at)?;
                Ok(Value::Data(self.slice(start, len)?.to_vec()))
            }
            (0x5, n) => {
                let (units, start) = self.count_after(n, at)?;
                let len = self.string_len(units, at, 1)?;
                let raw = self.slice(start, len)?;
                // ASCII, but a damaged file may hold more: bytes above 0x7F read as Latin-1.
                Ok(Value::String(raw.iter().map(|&b| char::from(b)).collect()))
            }
            (0x6, n) => {
                let (units, start) = self.count_after(n, at)?;
                let len = self.string_len(units, at, 2)?;
                let raw = self.slice(start, len)?;
                let units = raw
                    .chunks_exact(2)
                    .filter_map(|pair| <[u8; 2]>::try_from(pair).ok())
                    .map(u16::from_be_bytes);
                Ok(Value::String(
                    char::decode_utf16(units)
                        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
                        .collect(),
                ))
            }
            (0x8, n) if n <= 7 => {
                let size = usize::from(n).saturating_add(1);
                let slice = self.slice(at.saturating_add(1), size)?;
                let value = slice
                    .iter()
                    .fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
                Ok(Value::Integer(i128::from(value)))
            }
            (0xA | 0xC, n) => {
                let (items, start) = self.count_after(n, at)?;
                let refs = self.refs(start, items)?;
                let mut out = Vec::with_capacity(refs.len().min(1024));
                self.enter(number, at)?;
                for target in refs {
                    out.push(self.object(target, depth.saturating_add(1))?);
                }
                self.open.remove(&number);
                Ok(Value::Array(out))
            }
            (0xD, n) => {
                let (items, start) = self.count_after(n, at)?;
                let total = items.checked_mul(2).ok_or_else(|| bad(at as u64, "count", items as u64))?;
                let refs = self.refs(start, total)?;
                let (keys, values) = refs.split_at(items.min(refs.len()));
                self.enter(number, at)?;
                let mut out = Vec::with_capacity(items.min(1024));
                for (key, value) in keys.iter().zip(values) {
                    let Value::String(key) = self.object(*key, depth.saturating_add(1))? else {
                        return Err(corrupt_text(WHAT, "a dictionary key is not a string"));
                    };
                    out.push((key, self.object(*value, depth.saturating_add(1))?));
                }
                self.open.remove(&number);
                Ok(Value::Dict(out))
            }
            _ => Err(bad(at as u64, "object marker", u64::from(marker))),
        }
    }

    /// Marks `number` as being decoded; a second visit while it is open is a loop.
    fn enter(&mut self, number: usize, at: usize) -> Result<()> {
        if self.open.insert(number) {
            Ok(())
        } else {
            Err(corrupt(WHAT, at as u64, CorruptKind::Loop { what: "container" }))
        }
    }

    /// The `n` object numbers that start at `start`.
    fn refs(&self, start: usize, n: usize) -> Result<Vec<usize>> {
        if n > MAX_OBJECTS.saturating_mul(2) {
            return Err(corrupt(
                WHAT,
                start as u64,
                CorruptKind::OutOfRange {
                    field: "container size",
                    value: n as u64,
                    limit_name: "most objects",
                    limit: MAX_OBJECTS as u64,
                },
            ));
        }
        let len = n
            .checked_mul(self.ref_size)
            .ok_or_else(|| bad(start as u64, "count", n as u64))?;
        // Checks the whole run is inside the object area before reading any of it.
        self.slice(start, len)?;
        let mut out = Vec::with_capacity(n);
        for index in 0..n {
            let at = start.saturating_add(index.saturating_mul(self.ref_size));
            let number = self
                .uint(at, self.ref_size)
                .map(to_usize)
                .ok_or_else(|| truncated(WHAT, at as u64, self.ref_size as u64, len64(self.bytes)))?;
            out.push(number);
        }
        Ok(out)
    }
}

/// "2001-01-01T00:00:00Z" style text for `seconds` since 2001-01-01 UTC, or
/// `None` for a value that is not a time in years 1 to 9999.
fn iso_date(seconds: f64) -> Option<String> {
    // About 2,000 years either side of 2001, far inside i64.
    if !seconds.is_finite() || seconds.abs() > 6.0e10 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    let whole = seconds.floor() as i64;
    civil(whole)
}

/// Civil date and time of `seconds` after 2001-01-01 00:00:00 UTC, for
/// |seconds| <= 6e10 (the bound makes every step below fit i64).
#[allow(clippy::arithmetic_side_effects)]
fn civil(seconds: i64) -> Option<String> {
    // 11,323 days from 1970-01-01 to 2001-01-01.
    let days = seconds.div_euclid(86_400) + 11_323;
    let secs = seconds.rem_euclid(86_400);
    // Howard Hinnant's days-to-civil.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    if !(1..=9999).contains(&year) {
        return None;
    }
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A property list from raw objects (object 0 is the root).
    fn build(objects: &[&[u8]], ref_size: u8, offset_size: u8) -> Vec<u8> {
        let mut out = b"bplist00".to_vec();
        let mut offsets = Vec::new();
        for object in objects {
            offsets.push(out.len());
            out.extend_from_slice(object);
        }
        let table = out.len();
        for offset in offsets {
            out.extend_from_slice(&offset.to_be_bytes()[8 - usize::from(offset_size)..]);
        }
        out.extend_from_slice(&[0; 6]);
        out.push(offset_size);
        out.push(ref_size);
        out.extend_from_slice(&(objects.len() as u64).to_be_bytes());
        out.extend_from_slice(&0u64.to_be_bytes());
        out.extend_from_slice(&(table as u64).to_be_bytes());
        out
    }

    #[test]
    fn dictionary_array_and_scalars() {
        // {"k": ["v", 300, true], "u": "é" (UTF-16)}
        let plist = build(
            &[
                &[0xD2, 1, 3, 2, 4],
                b"\x51k",
                &[0xA3, 5, 6, 7],
                b"\x51u",
                &[0x61, 0x00, 0xE9],
                b"\x51v",
                &[0x11, 0x01, 0x2C],
                &[0x09],
            ],
            1,
            1,
        );
        let value = parse(&plist).unwrap();
        let list = value.get("k").unwrap().as_array().unwrap();
        assert_eq!(list[0].as_str(), Some("v"));
        assert_eq!(list[1].as_integer(), Some(300));
        assert_eq!(list[2], Value::Bool(true));
        assert_eq!(value.get("u").unwrap().as_str(), Some("é"));
    }

    #[test]
    fn counts_in_an_integer_object_and_two_byte_references() {
        // A 16-byte ASCII string whose count is the integer object 0x10 0x10.
        let mut text = vec![0x5F, 0x10, 0x10];
        text.extend_from_slice(b"sixteen chars ok");
        let plist = build(&[&text], 2, 2);
        assert_eq!(parse(&plist).unwrap().as_str(), Some("sixteen chars ok"));
        // The same count marker as a four-byte integer (0x12) is accepted; 16-byte (0x14) is not.
        let bad_marker = build(&[&[0x5F, 0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, b'x']], 1, 1);
        assert!(parse(&bad_marker).is_err());
    }

    #[test]
    fn a_date_reads_as_iso_text() {
        // 1 day after the Apple epoch.
        let mut object = vec![0x33];
        object.extend_from_slice(&86_400f64.to_be_bytes());
        let value = parse(&build(&[&object], 1, 1)).unwrap();
        assert_eq!(value, Value::Date("2001-01-02T00:00:00Z".into()));
    }

    #[test]
    fn a_container_that_holds_itself_is_an_error() {
        // Object 0 is an array holding object 0.
        let plist = build(&[&[0xA1, 0]], 1, 1);
        assert!(parse(&plist).is_err());
        // Two arrays holding each other.
        let plist = build(&[&[0xA1, 1], &[0xA1, 0]], 1, 1);
        assert!(parse(&plist).is_err());
    }

    #[test]
    fn nesting_past_the_limit_and_hostile_counts_are_errors() {
        // 33 nested arrays: depth 33.
        let objects: Vec<Vec<u8>> = (0..33u8)
            .map(|i| if i == 32 { vec![0x09] } else { vec![0xA1, i + 1] })
            .collect();
        let refs: Vec<&[u8]> = objects.iter().map(Vec::as_slice).collect();
        assert!(parse(&build(&refs, 1, 1)).is_err());
        // An array that claims 60,000 elements but has two bytes.
        let plist = build(&[&[0xAF, 0x11, 0xEA, 0x60, 0, 0]], 1, 1);
        assert!(parse(&plist).is_err());
        // A string longer than the file.
        assert!(parse(&build(&[&[0x5F, 0x12, 0x7F, 0xFF, 0xFF, 0xFF, b'x']], 1, 1)).is_err());
        // Not a property list, too short, a bad trailer.
        assert!(parse(b"bplist00").is_err());
        assert!(parse(b"<?xml").is_err());
        let mut plist = build(&[&[0x09]], 1, 1);
        let n = plist.len();
        plist[n - 26] = 3; // offset size 3
        assert!(parse(&plist).is_err());
    }

    #[test]
    fn shared_objects_are_paid_for_every_time() {
        // An array of 70,000 references to one string: past the object budget.
        let mut list = vec![0xAF, 0x12, 0x00, 0x01, 0x11, 0x70];
        for _ in 0..70_000 {
            list.extend_from_slice(&[0, 1]);
        }
        let plist = build(&[&list, b"Qx"], 2, 4);
        assert!(parse(&plist).is_err());
    }

    #[test]
    fn random_bytes_never_panic() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..3000 {
            let mut bytes = b"bplist00".to_vec();
            let len = {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state % 120) as usize
            };
            for _ in 0..len {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                bytes.push(state as u8);
            }
            let _ = parse(&bytes);
        }
    }
}

#[cfg(test)]
mod fuzz_seeds {
    /// Writes seeds for the `bplist` fuzz target into `DISK_ATLAS_FUZZ_SEEDS/bplist`.
    #[test]
    #[ignore = "writes files; run by the fuzz job"]
    fn write_fuzz_seeds() {
        let Some(root) = std::env::var_os("DISK_ATLAS_FUZZ_SEEDS") else {
            return;
        };
        let dir = std::path::Path::new(&root).join("bplist");
        std::fs::create_dir_all(&dir).expect("create seed directory");
        // {"k": "v"}
        let mut plist = b"bplist00".to_vec();
        for object in [&[0xD1u8, 1, 2][..], b"\x51k", b"\x51v"] {
            plist.extend_from_slice(object);
        }
        // Offsets 8, 11, 13; table at 15.
        plist.extend_from_slice(&[8, 11, 13]);
        plist.extend_from_slice(&[0; 6]);
        plist.extend_from_slice(&[1, 1]);
        plist.extend_from_slice(&3u64.to_be_bytes());
        plist.extend_from_slice(&0u64.to_be_bytes());
        plist.extend_from_slice(&15u64.to_be_bytes());
        assert!(super::parse(&plist).is_ok(), "the seed is a valid list");
        std::fs::write(dir.join("dict"), &plist).expect("write seed");
        std::fs::write(dir.join("quarantine"), b"0083;5f4e1b2c;Safari;UUID").expect("write seed");
    }
}
