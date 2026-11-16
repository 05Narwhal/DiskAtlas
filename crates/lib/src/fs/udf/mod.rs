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

//! UDF (ECMA-167, OSTA UDF 1.02 to 2.60), read-only.
//!
//! The path from the start: the anchor volume descriptor pointer (sector 256,
//! or near the end of the volume) names the volume descriptor sequence; its
//! partition descriptors and logical volume descriptor give the partition
//! maps (physical, and the metadata partition of UDF 2.50 and later); the file
//! set descriptor names the root folder's file entry. Folders are runs of
//! file identifier descriptors; a file's content is short or long allocation
//! descriptors, or bytes embedded in its file entry.
//!
//! Not supported, and named in the error: virtual (packet-written) and
//! sparable (CD-RW) partitions. Every descriptor's tag (checksum, CRC and
//! location) is verified before use.
//!
//! Node ids hold the partition reference number in the top 16 bits and the
//! logical block of the file entry in the low 48.

use std::sync::Arc;

use crate::error::{CorruptKind, Corruption, Error, Location, Result};
use crate::fs::iso9660::days_from_civil;
use crate::fs::{
    DirBatch, DirEntry, DirProgress, DirStream, EntryFlags, Extent, FileSystem, Metadata, NodeId,
    NodeKind, Owner, Stream, Timestamp, VolumeInfo,
};
use crate::io::block::{BlockSource, MemSource, check_range};

#[cfg(test)]
mod fuzz_seeds;
#[cfg(test)]
pub mod test_image;

const STRUCTURE: &str = "UDF structure";

/// Sector sizes tried, most likely first.
const SECTOR_SIZES: [u64; 3] = [2048, 512, 4096];
/// Most descriptors read from a volume descriptor sequence.
const MAX_DESCRIPTORS: u64 = 64;
/// Largest folder read, in bytes.
pub const MAX_DIR_BYTES: u64 = 256 * 1024 * 1024;
/// Most allocation descriptors (and so extents) one file may have.
const MAX_EXTENTS: usize = 1_000_000;
/// Most continuation extents followed for one file.
const MAX_CONTINUATIONS: usize = 4096;
/// Largest symbolic link target read.
const MAX_LINK_BYTES: u64 = 65_536;
/// Most bytes read for one descriptor.
const DESC_READ: u64 = 4096;
/// Bytes of folder read ahead while listing.
const WINDOW: u64 = 32 * 1024;

const TAG_PVD: u16 = 1;
const TAG_AVDP: u16 = 2;
const TAG_PD: u16 = 5;
const TAG_LVD: u16 = 6;
const TAG_TD: u16 = 8;
const TAG_FSD: u16 = 256;
const TAG_FID: u16 = 257;
const TAG_AED: u16 = 258;
const TAG_FE: u16 = 261;
const TAG_EFE: u16 = 266;

fn le16(b: &[u8], at: usize) -> Option<u16> {
    let raw = b.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes(raw.try_into().ok()?))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    let raw = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes(raw.try_into().ok()?))
}

fn le64(b: &[u8], at: usize) -> Option<u64> {
    let raw = b.get(at..at.checked_add(8)?)?;
    Some(u64::from_le_bytes(raw.try_into().ok()?))
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
const CRC_TABLE: [u16; 256] = {
    let mut table = [0u16; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = (i as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// CRC-16/CCITT (polynomial 0x1021, start 0, no reflection) as UDF uses.
fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &byte in data {
        let index = usize::from((crc >> 8) as u8 ^ byte);
        crc = (crc << 8) ^ CRC_TABLE.get(index).copied().unwrap_or(0);
    }
    crc
}

/// Checks the tag at the start of `buf`: checksum, CRC (when the tag
/// declares one and `buf` holds it) and, when given, the location. Returns
/// the tag identifier.
fn verify_tag(buf: &[u8], location: Option<u32>) -> Option<u16> {
    let tag = buf.get(..16)?;
    let sum = tag
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 4)
        .fold(0u8, |acc, (_, b)| acc.wrapping_add(*b));
    if Some(&sum) != tag.get(4) {
        return None;
    }
    let crc_len = usize::from(le16(buf, 10)?);
    if crc_len > 0 {
        let body = buf.get(16..16usize.checked_add(crc_len)?)?;
        if crc16(body) != le16(buf, 8)? {
            return None;
        }
    }
    if let Some(want) = location
        && le32(buf, 12)? != want
    {
        return None;
    }
    le16(buf, 0)
}

fn corrupt(location: Location, kind: CorruptKind) -> Error {
    Corruption::new(STRUCTURE, location, kind).into()
}

fn bad_tag(byte: u64, expected: u16) -> Error {
    corrupt(
        Location::Byte(byte),
        CorruptKind::BadSignature {
            expected: u64::from(expected),
            found: 0,
        },
    )
}

fn out_of_range(field: &'static str, value: u64, limit_name: &'static str, limit: u64) -> Error {
    corrupt(
        Location::Unspecified,
        CorruptKind::OutOfRange {
            field,
            value,
            limit_name,
            limit,
        },
    )
}

fn not_found(node: NodeId) -> Error {
    Error::NotFound { node: node.0 }
}

/// OSTA compressed Unicode: the first byte says how many bits a character has.
fn osta(bytes: &[u8]) -> String {
    let Some((&mode, rest)) = bytes.split_first() else {
        return String::new();
    };
    match mode {
        8 | 254 => rest.iter().map(|&b| char::from(b)).collect(),
        16 | 255 => {
            let units: Vec<u16> = rest
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_be_bytes(*c))
                .collect();
            String::from_utf16_lossy(&units)
        }
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// A fixed-size string field whose last byte holds the used length.
fn dstring(field: &[u8]) -> String {
    let Some((&used, body)) = field.split_last() else {
        return String::new();
    };
    let n = usize::from(used).min(body.len());
    osta(body.get(..n).unwrap_or(&[]))
        .trim_end_matches('\0')
        .to_owned()
}

/// A 12-byte UDF timestamp as UTC.
fn timestamp(b: &[u8]) -> Option<Timestamp> {
    let word = le16(b, 0)?;
    let kind = word >> 12;
    let mut minutes = i64::from(word & 0x0FFF);
    if minutes & 0x800 != 0 {
        minutes = minutes.saturating_sub(0x1000);
    }
    // Type 1 carries a zone (-2047 means unspecified); type 0 is UTC.
    let offset = if kind == 1 && (-1440..=1440).contains(&minutes) {
        minutes
    } else {
        0
    };
    let year = i64::from(le16(b, 2)?.cast_signed());
    let month = i64::from(*b.get(4)?);
    let day = i64::from(*b.get(5)?);
    let hour = i64::from(*b.get(6)?);
    let minute = i64::from(*b.get(7)?);
    let second = i64::from(*b.get(8)?);
    if !(1..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let nanos = u32::from(*b.get(9)?)
        .saturating_mul(10_000_000)
        .saturating_add(u32::from(*b.get(10)?).saturating_mul(100_000))
        .saturating_add(u32::from(*b.get(11)?).saturating_mul(1000));
    let seconds = days_from_civil(year, month, day)
        .saturating_mul(86_400)
        .saturating_add(hour.saturating_mul(3600))
        .saturating_add(minute.saturating_mul(60))
        .saturating_add(second)
        .saturating_sub(offset.saturating_mul(60));
    Some(Timestamp::from_unix(seconds, nanos.min(999_999_999)))
}

/// A block of a partition, by partition reference number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Loc {
    part: u16,
    block: u32,
}

impl Loc {
    fn node(self) -> NodeId {
        NodeId((u64::from(self.part) << 48) | u64::from(self.block))
    }

    fn from_node(node: NodeId) -> Option<Self> {
        Some(Self {
            part: u16::try_from(node.0 >> 48).ok()?,
            block: u32::try_from(node.0 & 0xFFFF_FFFF_FFFF).ok()?,
        })
    }

    /// A long allocation descriptor's block and partition (bytes 4 to 10).
    fn long(b: &[u8], at: usize) -> Option<Self> {
        Some(Self {
            block: le32(b, at.checked_add(4)?)?,
            part: le16(b, at.checked_add(8)?)?,
        })
    }
}

/// One run of the metadata file, in the bytes of the volume.
#[derive(Debug, Clone, Copy)]
struct MetaRun {
    /// Offset of the run in the metadata file.
    start: u64,
    /// Offset of the run in the volume.
    disk: u64,
    len: u64,
}

/// What a partition reference number stands for.
enum PartMap {
    Physical {
        /// Volume offset of the partition's first byte.
        start: u64,
        len: u64,
    },
    Metadata {
        /// The physical partition that holds the metadata file, and the
        /// home of file content.
        under: u16,
        runs: Vec<MetaRun>,
    },
    Unsupported(&'static str),
}

/// A piece of a file's content: `len` bytes at file offset `start`.
#[derive(Debug, Clone, Copy)]
struct Seg {
    start: u64,
    len: u64,
    /// Volume offset, or `None` for a hole that reads as zeros.
    disk: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdForm {
    Short,
    Long,
    Embedded,
}

/// A file entry, read.
struct Entry {
    loc: Loc,
    kind: NodeKind,
    size: u64,
    perm: u32,
    uid: u32,
    gid: u32,
    modified: Option<Timestamp>,
    accessed: Option<Timestamp>,
    created: Option<Timestamp>,
    form: AdForm,
    /// The allocation descriptors, or the embedded data.
    area: Vec<u8>,
}

struct Inner {
    source: Arc<dyn BlockSource>,
    /// Logical block size.
    bs: u64,
    maps: Vec<PartMap>,
}

impl Inner {
    /// The pieces of the volume that hold `len` bytes from `block` of
    /// partition `part`.
    fn resolve(&self, part: u16, block: u32, len: u64) -> Result<Vec<(u64, u64)>> {
        let map = self.maps.get(usize::from(part)).ok_or_else(|| {
            out_of_range(
                "partition reference",
                u64::from(part),
                "partition maps",
                self.maps.len() as u64,
            )
        })?;
        let offset = u64::from(block)
            .checked_mul(self.bs)
            .ok_or_else(|| corrupt(Location::Block(u64::from(block)), overflow("block offset")))?;
        let end = offset
            .checked_add(len)
            .ok_or_else(|| corrupt(Location::Block(u64::from(block)), overflow("extent end")))?;
        let size = self.source.len();
        match map {
            PartMap::Physical { start, len: plen } => {
                if end > *plen {
                    return Err(out_of_range("extent end", end, "partition length", *plen));
                }
                let disk = start.checked_add(offset).ok_or_else(|| {
                    corrupt(Location::Block(u64::from(block)), overflow("extent start"))
                })?;
                if len == 0 {
                    return Ok(Vec::new());
                }
                if disk >= size {
                    return Err(out_of_range("extent start", disk, "volume size", size));
                }
                Ok(vec![(disk, len.min(size.saturating_sub(disk)))])
            }
            PartMap::Metadata { runs, .. } => {
                let mut pieces = Vec::new();
                let mut pos = offset;
                let mut left = len;
                for run in runs {
                    if left == 0 {
                        break;
                    }
                    let run_end = run.start.saturating_add(run.len);
                    if pos >= run_end {
                        continue;
                    }
                    if pos < run.start {
                        break;
                    }
                    let take = left.min(run_end.saturating_sub(pos));
                    let disk = run.disk.saturating_add(pos.saturating_sub(run.start));
                    pieces.push((disk, take));
                    pos = pos.saturating_add(take);
                    left = left.saturating_sub(take);
                }
                if left > 0 {
                    return Err(out_of_range(
                        "metadata partition extent",
                        end,
                        "metadata file",
                        runs.last().map_or(0, |r| r.start.saturating_add(r.len)),
                    ));
                }
                Ok(pieces)
            }
            PartMap::Unsupported(what) => Err(Error::Unsupported((*what).into())),
        }
    }

    /// `len` bytes from `loc`.
    fn read_at_loc(&self, loc: Loc, len: u64) -> Result<Vec<u8>> {
        let mut out = vec![0u8; usize::try_from(len).map_err(|_| overflow_err("read length"))?];
        let mut at = 0usize;
        for (disk, piece) in self.resolve(loc.part, loc.block, len)? {
            let piece = usize::try_from(piece).map_err(|_| overflow_err("read length"))?;
            let end = at.checked_add(piece).ok_or_else(|| overflow_err("read"))?;
            let dest = out.get_mut(at..end).ok_or_else(|| overflow_err("read"))?;
            self.source.read_at(disk, dest)?;
            at = end;
        }
        Ok(out)
    }

    /// The file entry at `loc`.
    fn entry(&self, loc: Loc) -> Result<Entry> {
        // A file entry is one block; the last block of a trimmed image may
        // be short, so read what the volume has.
        let want = self.bs;
        let pieces = self.resolve(loc.part, loc.block, want)?;
        let mut buf = Vec::new();
        for (disk, piece) in pieces {
            let mut chunk = vec![0u8; usize::try_from(piece).map_err(|_| overflow_err("entry"))?];
            self.source.read_at(disk, &mut chunk)?;
            buf.extend_from_slice(&chunk);
        }
        let at = Location::Block(u64::from(loc.block));
        let id = verify_tag(&buf, Some(loc.block)).ok_or_else(|| {
            corrupt(
                at,
                CorruptKind::BadSignature {
                    expected: u64::from(TAG_FE),
                    found: u64::from(le16(&buf, 0).unwrap_or(0)),
                },
            )
        })?;
        let (header, size_at, atime, mtime, ctime, ea_at, uid_at) = match id {
            TAG_FE => (176usize, 56usize, 72usize, 84usize, None, 168usize, 36usize),
            TAG_EFE => (216, 56, 80, 92, Some(104usize), 208, 36),
            other => {
                return Err(corrupt(
                    at,
                    CorruptKind::BadSignature {
                        expected: u64::from(TAG_FE),
                        found: u64::from(other),
                    },
                ));
            }
        };
        let truncated = || {
            corrupt(
                at,
                CorruptKind::Truncated {
                    needed: header as u64,
                    available: buf.len() as u64,
                },
            )
        };
        let ea_len = le32(&buf, ea_at).ok_or_else(truncated)?;
        let ad_len = le32(&buf, ea_at.saturating_add(4)).ok_or_else(truncated)?;
        let area_at = header
            .checked_add(usize::try_from(ea_len).map_err(|_| overflow_err("extended attributes"))?)
            .ok_or_else(|| overflow_err("extended attributes"))?;
        let area_end = area_at
            .checked_add(usize::try_from(ad_len).map_err(|_| overflow_err("descriptors"))?)
            .ok_or_else(|| overflow_err("descriptors"))?;
        let area = buf
            .get(area_at..area_end)
            .ok_or_else(|| {
                corrupt(
                    at,
                    CorruptKind::OutOfRange {
                        field: "allocation descriptors",
                        value: area_end as u64,
                        limit_name: "block size",
                        limit: buf.len() as u64,
                    },
                )
            })?
            .to_vec();
        let file_type = buf.get(27).copied().unwrap_or(0);
        let flags = le16(&buf, 34).unwrap_or(0);
        let form = match flags & 7 {
            0 => AdForm::Short,
            1 => AdForm::Long,
            3 => AdForm::Embedded,
            _ => {
                return Err(Error::Unsupported(
                    "UDF extended allocation descriptors".into(),
                ));
            }
        };
        let kind = match file_type {
            4 => NodeKind::Directory,
            0 | 5 => NodeKind::File,
            12 => NodeKind::Symlink,
            _ => NodeKind::Other,
        };
        let time = |off: usize| buf.get(off..off.saturating_add(12)).and_then(timestamp);
        Ok(Entry {
            loc,
            kind,
            size: le64(&buf, size_at).ok_or_else(truncated)?,
            perm: le32(&buf, uid_at.saturating_add(8)).unwrap_or(0),
            uid: le32(&buf, uid_at).unwrap_or(u32::MAX),
            gid: le32(&buf, uid_at.saturating_add(4)).unwrap_or(u32::MAX),
            modified: time(mtime),
            accessed: time(atime),
            created: ctime.and_then(time),
            form,
            area,
        })
    }

    /// The partition a short allocation descriptor of `entry` refers to: the
    /// one the entry sits in, except that a file recorded in a metadata
    /// partition keeps its content in the partition below it (a metadata
    /// partition holds no file content). Unconfirmed against real discs.
    fn short_ad_part(&self, entry: &Entry) -> u16 {
        match self.maps.get(usize::from(entry.loc.part)) {
            Some(PartMap::Metadata { under, .. }) if entry.kind == NodeKind::File => *under,
            _ => entry.loc.part,
        }
    }

    /// The extents of a file entry, in file order, continuation extents
    /// followed.
    fn segments(&self, entry: &Entry) -> Result<Vec<Seg>> {
        let mut segs: Vec<Seg> = Vec::new();
        let mut at = 0u64;
        let mut area = entry.area.clone();
        let mut continuations = 0usize;
        loop {
            let step = if entry.form == AdForm::Long { 16 } else { 8 };
            let mut next: Option<(Loc, u64)> = None;
            for ad in area.chunks_exact(step) {
                let raw = le32(ad, 0).unwrap_or(0);
                let kind = raw >> 30;
                let len = u64::from(raw & 0x3FFF_FFFF);
                let loc = if entry.form == AdForm::Long {
                    Loc::long(ad, 0)
                } else {
                    le32(ad, 4).map(|block| Loc {
                        part: self.short_ad_part(entry),
                        block,
                    })
                }
                .ok_or_else(|| overflow_err("allocation descriptor"))?;
                if kind == 3 {
                    next = Some((loc, len));
                    break;
                }
                if len == 0 {
                    continue;
                }
                if kind == 0 {
                    for (disk, piece) in self.resolve(loc.part, loc.block, len)? {
                        segs.push(Seg {
                            start: at,
                            len: piece,
                            disk: Some(disk),
                        });
                        at = at.saturating_add(piece);
                    }
                } else {
                    segs.push(Seg {
                        start: at,
                        len,
                        disk: None,
                    });
                    at = at.saturating_add(len);
                }
                if segs.len() > MAX_EXTENTS {
                    return Err(corrupt(
                        Location::Block(u64::from(entry.loc.block)),
                        CorruptKind::LimitExceeded {
                            what: "file extents",
                            limit: MAX_EXTENTS as u64,
                        },
                    ));
                }
            }
            let Some((loc, len)) = next else {
                return Ok(segs);
            };
            continuations = continuations.saturating_add(1);
            if continuations > MAX_CONTINUATIONS {
                return Err(corrupt(
                    Location::Block(u64::from(entry.loc.block)),
                    CorruptKind::LimitExceeded {
                        what: "continuation extents",
                        limit: MAX_CONTINUATIONS as u64,
                    },
                ));
            }
            let want = len.clamp(24, 1 << 20);
            let buf = self.read_at_loc(loc, want)?;
            if verify_tag(&buf, Some(loc.block)) != Some(TAG_AED) {
                return Err(bad_tag(u64::from(loc.block), TAG_AED));
            }
            let l_ad = usize::try_from(le32(&buf, 20).unwrap_or(0)).unwrap_or(usize::MAX);
            area = buf
                .get(24..24usize.saturating_add(l_ad))
                .ok_or_else(|| {
                    out_of_range("allocation extent length", l_ad as u64, "extent", len)
                })?
                .to_vec();
        }
    }

    /// The content of a file entry as a readable source.
    fn content(&self, entry: &Entry) -> Result<Arc<dyn BlockSource>> {
        if entry.form == AdForm::Embedded {
            let n = usize::try_from(entry.size)
                .unwrap_or(usize::MAX)
                .min(entry.area.len());
            let bytes = entry.area.get(..n).unwrap_or(&[]).to_vec();
            return Ok(Arc::new(MemSource::new(bytes)));
        }
        Ok(Arc::new(UdfSource {
            source: Arc::clone(&self.source),
            segs: self.segments(entry)?,
            size: entry.size,
        }))
    }
}

fn overflow(field: &'static str) -> CorruptKind {
    CorruptKind::Overflow { field }
}

fn overflow_err(field: &'static str) -> Error {
    corrupt(Location::Unspecified, overflow(field))
}

/// The bytes of a file: its extents, with holes reading as zeros.
struct UdfSource {
    source: Arc<dyn BlockSource>,
    segs: Vec<Seg>,
    size: u64,
}

impl BlockSource for UdfSource {
    fn len(&self) -> u64 {
        self.size
    }

    fn sector_size(&self) -> u32 {
        512
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(offset, buf.len(), self.size)?;
        let mut done = 0usize;
        let mut pos = offset;
        let mut idx = self
            .segs
            .partition_point(|s| s.start.saturating_add(s.len) <= pos);
        while done < buf.len() {
            let left = buf.len().saturating_sub(done);
            let (take, disk) = match self.segs.get(idx) {
                None => (left, None),
                Some(seg) if pos < seg.start => {
                    let gap = usize::try_from(seg.start.saturating_sub(pos)).unwrap_or(left);
                    (gap.min(left), None)
                }
                Some(seg) => {
                    let into = pos.saturating_sub(seg.start);
                    let avail = usize::try_from(seg.len.saturating_sub(into)).unwrap_or(left);
                    idx = idx.saturating_add(1);
                    (avail.min(left), seg.disk.map(|d| d.saturating_add(into)))
                }
            };
            let dest = buf
                .get_mut(done..done.saturating_add(take))
                .ok_or_else(|| overflow_err("read"))?;
            match disk {
                Some(at) => self.source.read_at(at, dest)?,
                None => dest.fill(0),
            }
            done = done.saturating_add(take);
            pos = pos.saturating_add(take as u64);
        }
        Ok(())
    }
}

/// A folder entry, read.
struct Fid {
    name: String,
    dir: bool,
    hidden: bool,
    loc: Loc,
}

/// Walks the file identifier descriptors of one folder.
struct Walker {
    src: Arc<dyn BlockSource>,
    size: u64,
    pos: u64,
    window: Vec<u8>,
    window_at: u64,
}

impl Walker {
    fn new(src: Arc<dyn BlockSource>) -> Self {
        let size = src.len();
        Self {
            src,
            size,
            pos: 0,
            window: Vec::new(),
            window_at: 0,
        }
    }

    fn bytes(&mut self, at: u64, len: u64) -> Result<&[u8]> {
        let end = at.saturating_add(len);
        let have = self.window_at.saturating_add(self.window.len() as u64);
        if at < self.window_at || end > have {
            let want = len.max(WINDOW).min(self.size.saturating_sub(at));
            let mut buf = vec![0u8; usize::try_from(want).map_err(|_| overflow_err("window"))?];
            self.src.read_at(at, &mut buf)?;
            self.window = buf;
            self.window_at = at;
        }
        let from = usize::try_from(at.saturating_sub(self.window_at))
            .map_err(|_| overflow_err("window"))?;
        let to = from.saturating_add(usize::try_from(len).map_err(|_| overflow_err("window"))?);
        self.window
            .get(from..to)
            .ok_or_else(|| overflow_err("folder read"))
    }

    fn next_fid(&mut self) -> Result<Option<Fid>> {
        loop {
            let left = self.size.saturating_sub(self.pos);
            if left < 38 {
                return Ok(None);
            }
            let pos = self.pos;
            let head = self.bytes(pos, 38)?;
            let l_fi = u64::from(head.get(19).copied().unwrap_or(0));
            let l_iu = u64::from(le16(head, 36).unwrap_or(0));
            let total = 38u64
                .saturating_add(l_iu)
                .saturating_add(l_fi)
                .saturating_add(3)
                & !3;
            if total > left {
                return Err(corrupt(
                    Location::Byte(pos),
                    CorruptKind::Truncated {
                        needed: total,
                        available: left,
                    },
                ));
            }
            let rec = self.bytes(pos, total)?.to_vec();
            if verify_tag(&rec, None) != Some(TAG_FID) {
                return Err(bad_tag(pos, TAG_FID));
            }
            self.pos = pos.saturating_add(total);
            let flags = rec.get(18).copied().unwrap_or(0);
            if flags & 0x0C != 0 {
                continue; // deleted or the parent entry
            }
            let name_at = 38usize.saturating_add(usize::try_from(l_iu).unwrap_or(0));
            let name_end = name_at.saturating_add(usize::try_from(l_fi).unwrap_or(0));
            let name = osta(rec.get(name_at..name_end).unwrap_or(&[])).replace(['/', ' '], "_");
            let Some(loc) = Loc::long(&rec, 20) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            return Ok(Some(Fid {
                name,
                dir: flags & 0x02 != 0,
                hidden: flags & 0x01 != 0,
                loc,
            }));
        }
    }
}

/// A mounted UDF volume.
pub struct Udf {
    inner: Arc<Inner>,
    root: Loc,
    info: VolumeInfo,
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Udf>();
};

struct Anchor {
    sector: u64,
    main: (u32, u32),
    reserve: (u32, u32),
}

/// Finds the anchor volume descriptor pointer and the sector size.
fn find_anchor(source: &dyn BlockSource) -> Result<Anchor> {
    let size = source.len();
    for ss in SECTOR_SIZES {
        let last = size.checked_div(ss).unwrap_or(0);
        if last == 0 {
            continue;
        }
        let candidates = [Some(256u64), last.checked_sub(257), last.checked_sub(1)];
        for sector in candidates.into_iter().flatten() {
            let Some(at) = sector.checked_mul(ss) else {
                continue;
            };
            if at.saturating_add(512) > size {
                continue;
            }
            let mut buf = [0u8; 512];
            if source.read_at(at, &mut buf).is_err() {
                continue;
            }
            let Ok(location) = u32::try_from(sector) else {
                continue;
            };
            if verify_tag(&buf, Some(location)) == Some(TAG_AVDP) {
                let pair = |at| Some((le32(&buf, at)?, le32(&buf, at.saturating_add(4))?));
                if let (Some(main), Some(reserve)) = (pair(16), pair(24)) {
                    return Ok(Anchor {
                        sector: ss,
                        main,
                        reserve,
                    });
                }
            }
        }
    }
    Err(corrupt(
        Location::Unspecified,
        CorruptKind::Other("no UDF anchor volume descriptor pointer"),
    ))
}

#[derive(Default)]
struct Vds {
    /// Partition descriptors: (sequence, number, start sector, sectors).
    parts: Vec<(u32, u16, u32, u32)>,
    /// The logical volume descriptor and its sequence number.
    lvd: Option<(u32, Vec<u8>)>,
    label: Option<String>,
}

/// Reads one volume descriptor sequence.
fn read_vds(source: &dyn BlockSource, ss: u64, extent: (u32, u32)) -> Vds {
    let mut vds = Vds::default();
    let (len, loc) = (u64::from(extent.0), u64::from(extent.1));
    let count = len.checked_div(ss).unwrap_or(0).min(MAX_DESCRIPTORS);
    let size = source.len();
    for i in 0..count {
        let sector = loc.saturating_add(i);
        let Some(at) = sector.checked_mul(ss) else {
            break;
        };
        if at >= size {
            break;
        }
        let want = ss.max(DESC_READ.min(2048)).min(size.saturating_sub(at));
        let mut buf = vec![0u8; usize::try_from(want).unwrap_or(0)];
        if source.read_at(at, &mut buf).is_err() {
            continue;
        }
        let Ok(location) = u32::try_from(sector) else {
            continue;
        };
        let Some(id) = verify_tag(&buf, Some(location)) else {
            continue;
        };
        let seq = le32(&buf, 16).unwrap_or(0);
        match id {
            TAG_PVD => {
                if vds.label.is_none() {
                    vds.label = Some(dstring(buf.get(24..56).unwrap_or(&[])));
                }
            }
            TAG_PD => {
                if let (Some(number), Some(start), Some(plen)) =
                    (le16(&buf, 22), le32(&buf, 188), le32(&buf, 192))
                {
                    match vds.parts.iter_mut().find(|p| p.1 == number) {
                        Some(old) if old.0 < seq => *old = (seq, number, start, plen),
                        Some(_) => {}
                        None => vds.parts.push((seq, number, start, plen)),
                    }
                }
            }
            TAG_LVD => {
                if vds.lvd.as_ref().is_none_or(|(old, _)| *old < seq) {
                    vds.lvd = Some((seq, buf));
                }
            }
            TAG_TD => break,
            _ => {}
        }
    }
    vds
}

/// A partition map as the logical volume descriptor lists it.
enum RawMap {
    Physical(u16),
    Metadata { part: u16, main: u32, mirror: u32 },
    Unsupported(&'static str),
}

fn parse_maps(lvd: &[u8]) -> Result<Vec<RawMap>> {
    let count = le32(lvd, 268).unwrap_or(0);
    let table = usize::try_from(le32(lvd, 264).unwrap_or(0)).unwrap_or(0);
    let end = 440usize.saturating_add(table).min(lvd.len());
    if count > 64 {
        return Err(out_of_range(
            "partition map count",
            u64::from(count),
            "limit",
            64,
        ));
    }
    let mut maps = Vec::new();
    let mut at = 440usize;
    for _ in 0..count {
        let kind = lvd.get(at).copied().unwrap_or(0);
        let len = usize::from(lvd.get(at.saturating_add(1)).copied().unwrap_or(0));
        if len == 0 || at.saturating_add(len) > end {
            return Err(out_of_range(
                "partition map",
                at as u64,
                "map table",
                end as u64,
            ));
        }
        let map = match kind {
            1 => RawMap::Physical(le16(lvd, at.saturating_add(4)).unwrap_or(0)),
            2 => {
                let id = lvd
                    .get(at.saturating_add(5)..at.saturating_add(28))
                    .unwrap_or(&[]);
                if id.starts_with(b"*UDF Metadata Partition") {
                    RawMap::Metadata {
                        part: le16(lvd, at.saturating_add(38)).unwrap_or(0),
                        main: le32(lvd, at.saturating_add(40)).unwrap_or(u32::MAX),
                        mirror: le32(lvd, at.saturating_add(44)).unwrap_or(u32::MAX),
                    }
                } else if id.starts_with(b"*UDF Virtual Partition") {
                    RawMap::Unsupported("UDF virtual partition (packet-written disc, VAT)")
                } else if id.starts_with(b"*UDF Sparable Partition") {
                    RawMap::Unsupported("UDF sparable partition (CD-RW)")
                } else {
                    RawMap::Unsupported("UDF partition map of an unknown type")
                }
            }
            _ => RawMap::Unsupported("UDF partition map of an unknown type"),
        };
        maps.push(map);
        at = at.saturating_add(len);
    }
    Ok(maps)
}

/// Mounts a UDF volume.
pub fn mount(source: Arc<dyn BlockSource>) -> Result<Udf> {
    let anchor = find_anchor(&*source)?;
    let ss = anchor.sector;
    let mut vds = read_vds(&*source, ss, anchor.main);
    if vds.lvd.is_none() || vds.parts.is_empty() {
        let reserve = read_vds(&*source, ss, anchor.reserve);
        if reserve.lvd.is_some() && !reserve.parts.is_empty() {
            vds = reserve;
        }
    }
    let Some((_, lvd)) = vds.lvd.take() else {
        return Err(corrupt(
            Location::Unspecified,
            CorruptKind::Other("no UDF logical volume descriptor"),
        ));
    };
    let bs = u64::from(le32(&lvd, 212).unwrap_or(0));
    if !(512..=65_536).contains(&bs) || !bs.is_power_of_two() {
        return Err(corrupt(
            Location::Unspecified,
            CorruptKind::InvalidValue {
                field: "logical block size",
                value: bs,
            },
        ));
    }
    let raw = parse_maps(&lvd)?;
    let size = source.len();
    let physical = |number: u16| -> Result<PartMap> {
        let (_, _, start, len) = vds
            .parts
            .iter()
            .find(|p| p.1 == number)
            .copied()
            .ok_or_else(|| {
                corrupt(
                    Location::Unspecified,
                    CorruptKind::InvalidValue {
                        field: "partition number",
                        value: u64::from(number),
                    },
                )
            })?;
        let start = u64::from(start).saturating_mul(ss);
        if start >= size {
            return Err(out_of_range("partition start", start, "volume size", size));
        }
        Ok(PartMap::Physical {
            start,
            len: u64::from(len).saturating_mul(ss),
        })
    };
    // First the physical partitions, so a metadata file can be read from one.
    let mut maps = Vec::with_capacity(raw.len());
    for map in &raw {
        maps.push(match map {
            RawMap::Physical(number) => physical(*number)?,
            RawMap::Metadata { .. } => PartMap::Unsupported("UDF metadata partition"),
            RawMap::Unsupported(what) => PartMap::Unsupported(what),
        });
    }
    for map in &raw {
        if let RawMap::Unsupported(what) = map {
            return Err(Error::Unsupported((*what).into()));
        }
    }
    let mut inner = Inner {
        source: Arc::clone(&source),
        bs,
        maps,
    };
    for (index, map) in raw.iter().enumerate() {
        let RawMap::Metadata { part, main, mirror } = map else {
            continue;
        };
        let under = raw
            .iter()
            .position(|m| matches!(m, RawMap::Physical(n) if n == part))
            .and_then(|p| u16::try_from(p).ok())
            .ok_or_else(|| {
                corrupt(
                    Location::Unspecified,
                    CorruptKind::InvalidValue {
                        field: "metadata partition's partition number",
                        value: u64::from(*part),
                    },
                )
            })?;
        let runs = [*main, *mirror]
            .into_iter()
            .filter(|&b| b != u32::MAX)
            .find_map(|block| {
                let loc = Loc { part: under, block };
                let entry = inner.entry(loc).ok()?;
                let segs = inner.segments(&entry).ok()?;
                let runs: Vec<MetaRun> = segs
                    .iter()
                    .filter_map(|s| {
                        Some(MetaRun {
                            start: s.start,
                            disk: s.disk?,
                            len: s.len,
                        })
                    })
                    .collect();
                (!runs.is_empty()).then_some(runs)
            })
            .ok_or_else(|| {
                corrupt(
                    Location::Unspecified,
                    CorruptKind::Other("UDF metadata file unreadable"),
                )
            })?;
        if let Some(slot) = inner.maps.get_mut(index) {
            *slot = PartMap::Metadata { under, runs };
        }
    }
    let inner = Arc::new(inner);
    let fsd_loc = Loc::long(&lvd, 248).ok_or_else(|| overflow_err("file set descriptor"))?;
    let fsd = inner.read_at_loc(fsd_loc, bs)?;
    if verify_tag(&fsd, Some(fsd_loc.block)) != Some(TAG_FSD) {
        return Err(bad_tag(u64::from(fsd_loc.block), TAG_FSD));
    }
    let root = Loc::long(&fsd, 400).ok_or_else(|| overflow_err("root directory"))?;
    let root_entry = inner.entry(root)?;
    if root_entry.kind != NodeKind::Directory {
        return Err(corrupt(
            Location::Block(u64::from(root.block)),
            CorruptKind::InvalidValue {
                field: "root file type",
                value: 0,
            },
        ));
    }
    let name = dstring(lvd.get(84..212).unwrap_or(&[]));
    let name = if name.is_empty() {
        vds.label.clone().unwrap_or_default()
    } else {
        name
    };
    let mut info = VolumeInfo::new(if name.is_empty() {
        "UDF volume".to_owned()
    } else {
        name
    });
    info.format = Some("UDF");
    info.total_bytes = inner.maps.iter().find_map(|m| match m {
        PartMap::Physical { len, .. } => Some(*len),
        _ => None,
    });
    info.local_times = false;
    Ok(Udf { inner, root, info })
}

impl Udf {
    fn entry(&self, node: NodeId) -> Result<Entry> {
        let loc = Loc::from_node(node).ok_or_else(|| not_found(node))?;
        self.inner.entry(loc)
    }

    fn folder(&self, node: NodeId) -> Result<Walker> {
        let entry = self.entry(node)?;
        if entry.kind != NodeKind::Directory {
            return Err(Error::NotADirectory { node: node.0 });
        }
        if entry.size > MAX_DIR_BYTES {
            return Err(out_of_range(
                "folder size",
                entry.size,
                "folder limit",
                MAX_DIR_BYTES,
            ));
        }
        Ok(Walker::new(self.inner.content(&entry)?))
    }

    fn link_target(&self, entry: &Entry) -> Result<String> {
        if entry.size > MAX_LINK_BYTES {
            return Err(out_of_range(
                "link target size",
                entry.size,
                "limit",
                MAX_LINK_BYTES,
            ));
        }
        let source = self.inner.content(entry)?;
        let mut data = vec![0u8; usize::try_from(source.len()).unwrap_or(0)];
        source.read_at(0, &mut data)?;
        let mut parts: Vec<String> = Vec::new();
        let mut absolute = false;
        let mut at = 0usize;
        while let Some(head) = data.get(at..at.saturating_add(4)) {
            let kind = head.first().copied().unwrap_or(0);
            let len = usize::from(head.get(1).copied().unwrap_or(0));
            let name_at = at.saturating_add(4);
            let name = data
                .get(name_at..name_at.saturating_add(len))
                .unwrap_or(&[]);
            match kind {
                1 | 2 => absolute = true,
                3 => parts.push("..".to_owned()),
                4 => parts.push(".".to_owned()),
                5 => parts.push(osta(name)),
                _ => {}
            }
            at = name_at.saturating_add(len);
        }
        let joined = parts.join("/");
        Ok(if absolute {
            format!("/{joined}")
        } else {
            joined
        })
    }

    fn mode(entry: &Entry) -> u16 {
        let class = |shift: u32| ((entry.perm >> shift) & 7) as u16;
        let bits = (class(10) << 6) | (class(5) << 3) | class(0);
        let kind: u16 = match entry.kind {
            NodeKind::Directory => 0o040_000,
            NodeKind::File => 0o100_000,
            NodeKind::Symlink => 0o120_000,
            NodeKind::Other => 0,
        };
        kind | bits
    }

    fn shown_size(&self, entry: &Entry) -> Option<u64> {
        match entry.kind {
            NodeKind::Directory => None,
            NodeKind::Symlink => self
                .link_target(entry)
                .ok()
                .map(|t| u64::try_from(t.len()).unwrap_or(0)),
            _ => Some(entry.size),
        }
    }
}

struct UdfDir {
    fs: Arc<Inner>,
    walker: Walker,
    done: bool,
}

impl DirStream for UdfDir {
    fn next_batch(&mut self, out: &mut DirBatch, max: usize) -> Result<DirProgress> {
        if self.done {
            return Ok(DirProgress::Done);
        }
        let mut added = 0usize;
        while added < max {
            match self.walker.next_fid() {
                Ok(Some(fid)) => {
                    let entry = self.fs.entry(fid.loc).ok();
                    let kind = entry.as_ref().map_or(
                        if fid.dir {
                            NodeKind::Directory
                        } else {
                            NodeKind::File
                        },
                        |e| e.kind,
                    );
                    out.push(DirEntry {
                        node: fid.loc.node(),
                        name: &fid.name,
                        kind,
                        size: entry
                            .as_ref()
                            .filter(|e| matches!(e.kind, NodeKind::File | NodeKind::Other))
                            .map(|e| e.size),
                        resource_size: None,
                        modified: entry.as_ref().and_then(|e| e.modified),
                        flags: if fid.hidden {
                            EntryFlags::HIDDEN
                        } else {
                            EntryFlags::EMPTY
                        },
                    });
                    added = added.saturating_add(1);
                }
                Ok(None) => {
                    self.done = true;
                    return Ok(DirProgress::Done);
                }
                Err(e) => {
                    self.done = true;
                    return Err(e);
                }
            }
        }
        Ok(DirProgress::More)
    }
}

impl FileSystem for Udf {
    fn info(&self) -> VolumeInfo {
        self.info.clone()
    }

    fn root(&self) -> NodeId {
        self.root.node()
    }

    fn metadata(&self, node: NodeId) -> Result<Metadata> {
        let entry = self.entry(node)?;
        let link_target = if entry.kind == NodeKind::Symlink {
            self.link_target(&entry).ok()
        } else {
            None
        };
        let owner = (entry.uid != u32::MAX || entry.gid != u32::MAX).then(|| Owner {
            user: if entry.uid == u32::MAX { 0 } else { entry.uid },
            group: if entry.gid == u32::MAX { 0 } else { entry.gid },
            mode: Self::mode(&entry),
        });
        Ok(Metadata {
            node,
            kind: entry.kind,
            size: self.shown_size(&entry),
            resource_size: None,
            created: entry.created,
            modified: entry.modified,
            accessed: entry.accessed,
            flags: EntryFlags::EMPTY,
            link_target,
            finder_codes: None,
            owner,
            attributes: None,
            finder_info: None,
        })
    }

    fn lookup(&self, dir: NodeId, name: &str) -> Result<Option<NodeId>> {
        let fold = |s: &str| s.chars().flat_map(char::to_lowercase).collect::<String>();
        let want = fold(name);
        let mut walker = self.folder(dir)?;
        let mut loose = None;
        while let Some(fid) = walker.next_fid()? {
            if fid.name == name {
                return Ok(Some(fid.loc.node()));
            }
            if loose.is_none() && fold(&fid.name) == want {
                loose = Some(fid.loc.node());
            }
        }
        Ok(loose)
    }

    fn read_dir(&self, dir: NodeId) -> Result<Box<dyn DirStream>> {
        Ok(Box::new(UdfDir {
            fs: Arc::clone(&self.inner),
            walker: self.folder(dir)?,
            done: false,
        }))
    }

    fn extents(&self, node: NodeId, stream: Stream) -> Result<Vec<Extent>> {
        let entry = self.entry(node)?;
        if entry.kind == NodeKind::Directory {
            return Err(Error::IsADirectory { node: node.0 });
        }
        if stream == Stream::Resource
            || entry.kind == NodeKind::Symlink
            || entry.form == AdForm::Embedded
        {
            return Ok(Vec::new());
        }
        let mut out: Vec<Extent> = Vec::new();
        for seg in self.inner.segments(&entry)? {
            let Some(disk) = seg.disk else { continue };
            let len = seg.len.min(entry.size.saturating_sub(seg.start));
            if len == 0 {
                continue;
            }
            match out.last_mut() {
                Some(last) if last.offset.saturating_add(last.len) == disk => {
                    last.len = last.len.saturating_add(len);
                }
                _ => out.push(Extent { offset: disk, len }),
            }
        }
        Ok(out)
    }

    fn open(&self, node: NodeId, stream: Stream) -> Result<Arc<dyn BlockSource>> {
        let entry = self.entry(node)?;
        if entry.kind == NodeKind::Directory {
            return Err(Error::IsADirectory { node: node.0 });
        }
        if stream == Stream::Resource {
            return Err(Error::NoSuchStream { node: node.0 });
        }
        if entry.kind == NodeKind::Symlink {
            return Ok(Arc::new(MemSource::new(
                self.link_target(&entry)?.into_bytes(),
            )));
        }
        self.inner.content(&entry)
    }
}

#[cfg(test)]
mod tests;
