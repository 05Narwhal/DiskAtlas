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

//! Reader for Apple icon files (`.icns`, `.VolumeIcon.icns`, and the `icns`
//! resource -16455 of a folder with a custom icon). In-house for the PNG
//! entries and the run-length ARGB and RGB entries (`r1-redesign.md`, Q1);
//! JPEG 2000 entries are left out until a crate for them is approved (Q2).
//!
//! The file is `icns`, a u32 length of the whole file, then elements of
//! type[4], a u32 length that includes these 8 bytes, and data. Every integer
//! is big-endian.
//!
//! | Elements | What |
//! |---|---|
//! | `icp4` `icp5` `icp6` `ic07` to `ic14` | PNG at 16, 32, 64, 128, 256, 512, 1024, 32, 64, 256, 512 px |
//! | `ic04` `ic05` | `ARGB`, then the A, R, G, B planes run-length packed, 16 and 32 px |
//! | `is32` `il32` `ih32` `it32` | the R, G, B planes run-length packed (`it32` after 4 zero bytes), 16, 32, 48, 128 px |
//! | `s8mk` `l8mk` `h8mk` `t8mk` | the 8-bit alpha mask of the matching RGB element, unpacked |
//!
//! The run-length scheme: a byte below 0x80 is followed by that many plus
//! one literal bytes; a byte of 0x80 or more is followed by one byte that is
//! repeated that many minus 125 times (3 to 130).

use std::ops::Range;

use crate::error::{CorruptKind, Result};
use crate::io::block::BlockSource;

use super::resource_map::ResourceMap;
use super::util::{be32, corrupt, corrupt_text, len64, to_usize, truncated};

const WHAT: &str = "icon file";

/// The resource id of a folder's custom icon in its `Icon\r` file.
pub const ICNS_RESOURCE_ID: i16 = -16455;
/// Largest `icns` data read.
pub const MAX_ICNS: usize = 32 << 20;
/// Most elements read.
pub const MAX_ELEMENTS: usize = 256;

/// A decoded picture: 8-bit red, green, blue, alpha, row by row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rgba {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes.
    pub pixels: Vec<u8>,
}

/// What an icon gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IconImage {
    /// A PNG file, not decoded here.
    Png(Vec<u8>),
    /// A decoded picture.
    Rgba(Rgba),
}

/// The elements of one icon file.
#[derive(Debug, Clone)]
pub struct Icns {
    bytes: Vec<u8>,
    elements: Vec<([u8; 4], Range<usize>)>,
}

/// An element that holds a picture: its pixel size and how it is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Storage {
    Png,
    Argb,
    Rgb,
}

fn storage(kind: &[u8; 4]) -> Option<(u32, Storage)> {
    Some(match kind {
        b"icp4" => (16, Storage::Png),
        b"icp5" => (32, Storage::Png),
        b"icp6" => (64, Storage::Png),
        b"ic07" => (128, Storage::Png),
        b"ic08" => (256, Storage::Png),
        b"ic09" => (512, Storage::Png),
        b"ic10" => (1024, Storage::Png),
        b"ic11" => (32, Storage::Png),
        b"ic12" => (64, Storage::Png),
        b"ic13" => (256, Storage::Png),
        b"ic14" => (512, Storage::Png),
        b"ic04" => (16, Storage::Argb),
        b"ic05" => (32, Storage::Argb),
        b"is32" => (16, Storage::Rgb),
        b"il32" => (32, Storage::Rgb),
        b"ih32" => (48, Storage::Rgb),
        b"it32" => (128, Storage::Rgb),
        _ => return None,
    })
}

fn mask_kind(size: u32) -> Option<&'static [u8; 4]> {
    Some(match size {
        16 => b"s8mk",
        32 => b"l8mk",
        48 => b"h8mk",
        128 => b"t8mk",
        _ => return None,
    })
}

const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

impl Icns {
    /// Parses the file: its header and the list of elements. The elements' data are not read.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_ICNS {
            return Err(corrupt(
                WHAT,
                0,
                CorruptKind::OutOfRange {
                    field: "size",
                    value: len64(bytes),
                    limit_name: "largest icon file",
                    limit: MAX_ICNS as u64,
                },
            ));
        }
        if bytes.get(..4) != Some(b"icns") {
            return Err(corrupt(
                WHAT,
                0,
                CorruptKind::BadSignature {
                    expected: u64::from(u32::from_be_bytes(*b"icns")),
                    found: u64::from(be32(bytes, 0).unwrap_or(0)),
                },
            ));
        }
        let declared = to_usize(u64::from(
            be32(bytes, 4).ok_or_else(|| truncated(WHAT, 4, 4, len64(bytes)))?,
        ));
        // Trailing bytes past the declared length are ignored; a length past the file is cut to it.
        let end = declared.min(bytes.len());
        let mut elements = Vec::new();
        let mut at = 8usize;
        while at.saturating_add(8) <= end {
            if elements.len() >= MAX_ELEMENTS {
                return Err(corrupt(
                    WHAT,
                    at as u64,
                    CorruptKind::LimitExceeded {
                        what: "elements",
                        limit: MAX_ELEMENTS as u64,
                    },
                ));
            }
            let (Some(kind), Some(len)) = (
                bytes.get(at..).and_then(<[u8]>::first_chunk::<4>).copied(),
                be32(bytes, at.saturating_add(4)),
            ) else {
                break;
            };
            let len = to_usize(u64::from(len));
            let stop = at.checked_add(len).filter(|&stop| len >= 8 && stop <= end);
            let Some(stop) = stop else {
                return Err(corrupt(
                    WHAT,
                    at as u64,
                    CorruptKind::OutOfRange {
                        field: "element length",
                        value: len as u64,
                        limit_name: "bytes left",
                        limit: end.saturating_sub(at) as u64,
                    },
                ));
            };
            elements.push((kind, at.saturating_add(8)..stop));
            at = stop;
        }
        if elements.is_empty() {
            return Err(corrupt_text(WHAT, "the icon file has no elements"));
        }
        Ok(Self {
            bytes: bytes.to_vec(),
            elements,
        })
    }

    /// The type codes of the elements, in file order.
    pub fn kinds(&self) -> impl Iterator<Item = [u8; 4]> + '_ {
        self.elements.iter().map(|(kind, _)| *kind)
    }

    fn data(&self, kind: &[u8; 4]) -> Option<&[u8]> {
        let (_, range) = self.elements.iter().find(|(k, _)| k == kind)?;
        self.bytes.get(range.clone())
    }

    /// The picture sizes this reader can give, smallest first.
    pub fn sizes(&self) -> Vec<u32> {
        let mut sizes: Vec<u32> = self
            .elements
            .iter()
            .filter_map(|(kind, _)| storage(kind))
            .map(|(size, _)| size)
            .collect();
        sizes.sort_unstable();
        sizes.dedup();
        sizes
    }

    /// The best picture for `wanted` pixels: the smallest that is at least that big, else the
    /// biggest there is. At one size a PNG wins over the packed formats. `None` when the file
    /// holds nothing this reader can give (JPEG 2000 only, or damaged data).
    pub fn best(&self, wanted: u32) -> Option<IconImage> {
        let mut candidates: Vec<([u8; 4], u32, Storage)> = self
            .elements
            .iter()
            .filter_map(|(kind, _)| storage(kind).map(|(size, how)| (*kind, size, how)))
            .collect();
        // Smallest at or above `wanted` first, then the larger ones below, nearest first.
        candidates.sort_by_key(|&(_, size, how)| {
            (
                size < wanted,
                if size >= wanted { size } else { u32::MAX - size },
                how != Storage::Png,
            )
        });
        candidates
            .into_iter()
            .find_map(|(kind, size, how)| self.decode(&kind, size, how))
    }

    fn decode(&self, kind: &[u8; 4], size: u32, how: Storage) -> Option<IconImage> {
        let data = self.data(kind)?;
        match how {
            Storage::Png => data
                .starts_with(PNG_MAGIC)
                .then(|| IconImage::Png(data.to_vec())),
            Storage::Argb => {
                let body = data.strip_prefix(b"ARGB")?;
                let pixels = usize::try_from(size.checked_mul(size)?).ok()?;
                let planes = unpack(body, pixels.checked_mul(4)?)?;
                let (a, rest) = planes.split_at(pixels);
                let (r, rest) = rest.split_at(pixels);
                let (g, b) = rest.split_at(pixels);
                let mut out = Vec::with_capacity(pixels.checked_mul(4)?);
                for i in 0..pixels {
                    out.extend_from_slice(&[*r.get(i)?, *g.get(i)?, *b.get(i)?, *a.get(i)?]);
                }
                Some(IconImage::Rgba(Rgba { width: size, height: size, pixels: out }))
            }
            Storage::Rgb => {
                // `it32` has four zero bytes before the planes.
                let body = if kind == b"it32" { data.get(4..)? } else { data };
                let pixels = usize::try_from(size.checked_mul(size)?).ok()?;
                let planes = unpack(body, pixels.checked_mul(3)?)?;
                let (r, rest) = planes.split_at(pixels);
                let (g, b) = rest.split_at(pixels);
                let mask = mask_kind(size).and_then(|kind| self.data(kind));
                let mut out = Vec::with_capacity(pixels.checked_mul(4)?);
                for i in 0..pixels {
                    let alpha = match mask {
                        Some(mask) => mask.get(i).copied().unwrap_or(255),
                        None => 255,
                    };
                    out.extend_from_slice(&[*r.get(i)?, *g.get(i)?, *b.get(i)?, alpha]);
                }
                Some(IconImage::Rgba(Rgba { width: size, height: size, pixels: out }))
            }
        }
    }

    /// The custom icon of a folder: the `icns` resource of its `Icon\r` fork. `Ok(None)` when the
    /// fork has no such resource.
    pub fn from_resource_fork(fork: &dyn BlockSource) -> Result<Option<Self>> {
        let map = ResourceMap::read(fork)?;
        map.load(fork, *b"icns", ICNS_RESOURCE_ID, MAX_ICNS)?
            .map(|bytes| Self::parse(&bytes))
            .transpose()
    }
}

/// Unpacks run-length data to exactly `want` bytes. `None` when the data stops short; extra data
/// is ignored, and a run that would pass `want` is cut.
fn unpack(data: &[u8], want: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(want);
    let mut at = 0usize;
    while out.len() < want {
        let control = *data.get(at)?;
        at = at.checked_add(1)?;
        if control < 0x80 {
            let count = usize::from(control).checked_add(1)?;
            let run = data.get(at..at.checked_add(count)?)?;
            out.extend_from_slice(run);
            at = at.checked_add(count)?;
        } else {
            let count = usize::from(control).checked_sub(125)?;
            let value = *data.get(at)?;
            at = at.checked_add(1)?;
            out.extend(std::iter::repeat_n(value, count));
        }
    }
    out.truncate(want);
    Some(out)
}

#[cfg(test)]
pub(crate) mod test_icns {
    /// Packs `bytes` with the icns run-length scheme.
    pub fn pack(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let mut run = 1;
            while i + run < bytes.len() && bytes[i + run] == bytes[i] && run < 130 {
                run += 1;
            }
            if run >= 3 {
                out.push((run + 125) as u8);
                out.push(bytes[i]);
                i += run;
            } else {
                let mut end = i + 1;
                while end < bytes.len() && end - i < 128 && !(end + 2 < bytes.len() && bytes[end] == bytes[end + 1] && bytes[end] == bytes[end + 2]) {
                    end += 1;
                }
                out.push((end - i - 1) as u8);
                out.extend_from_slice(&bytes[i..end]);
                i = end;
            }
        }
        out
    }

    /// An icon file of `(kind, data)` elements.
    pub fn file(elements: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (kind, data) in elements {
            body.extend_from_slice(kind);
            body.extend_from_slice(&(data.len() as u32 + 8).to_be_bytes());
            body.extend_from_slice(data);
        }
        let mut out = b"icns".to_vec();
        out.extend_from_slice(&(body.len() as u32 + 8).to_be_bytes());
        out.extend(body);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::test_icns::*;
    use super::*;
    use crate::io::block::MemSource;

    fn fake_png() -> Vec<u8> {
        let mut png = PNG_MAGIC.to_vec();
        png.extend_from_slice(b"rest of the file");
        png
    }

    #[test]
    fn a_png_entry_is_returned_as_it_is_and_the_size_is_chosen() {
        let file = file(&[
            (*b"icp5", fake_png()),
            (*b"ic08", fake_png()),
            (*b"ic07", b"JPEG 2000, not a PNG".to_vec()),
        ]);
        let icns = Icns::parse(&file).unwrap();
        assert_eq!(icns.sizes(), [32, 128, 256]);
        assert_eq!(icns.best(32), Some(IconImage::Png(fake_png())));
        // 128 wanted: ic07 is no PNG, so the next size up is used.
        assert_eq!(icns.best(128), Some(IconImage::Png(fake_png())));
        // Bigger than any: the biggest there is.
        assert!(icns.best(2048).is_some());
    }

    #[test]
    fn packed_rgb_with_a_mask_decodes_to_rgba() {
        let pixels = 16 * 16;
        let planes: Vec<u8> = (0..pixels)
            .map(|_| 200u8)
            .chain((0..pixels).map(|i| (i % 7) as u8))
            .chain((0..pixels).map(|_| 9u8))
            .collect();
        let mask: Vec<u8> = (0..pixels).map(|i| if i < 128 { 255 } else { 0 }).collect();
        let file = file(&[(*b"is32", pack(&planes)), (*b"s8mk", mask)]);
        let Some(IconImage::Rgba(image)) = Icns::parse(&file).unwrap().best(16) else {
            panic!("no picture");
        };
        assert_eq!((image.width, image.height, image.pixels.len()), (16, 16, 16 * 16 * 4));
        assert_eq!(&image.pixels[..4], &[200, 0, 9, 255]);
        assert_eq!(&image.pixels[(130 * 4)..(130 * 4 + 4)], &[200, 4, 9, 0]);
    }

    #[test]
    fn packed_argb_decodes_and_it32_skips_its_zero_header() {
        let pixels = 16 * 16;
        let planes: Vec<u8> = [128u8, 10, 20, 30]
            .into_iter()
            .flat_map(|v| std::iter::repeat_n(v, pixels))
            .collect();
        let mut argb = b"ARGB".to_vec();
        argb.extend(pack(&planes));
        let file1 = file(&[(*b"ic04", argb)]);
        let Some(IconImage::Rgba(image)) = Icns::parse(&file1).unwrap().best(16) else {
            panic!("no picture");
        };
        assert_eq!(&image.pixels[..4], &[10, 20, 30, 128]);
        // it32: 128 x 128 with four zero bytes first.
        let big = 128 * 128;
        let rgb: Vec<u8> = (0..big * 3).map(|i| if i < big { 1 } else { 2 }).collect();
        let mut it32 = vec![0u8; 4];
        it32.extend(pack(&rgb));
        let file2 = file(&[(*b"it32", it32)]);
        let Some(IconImage::Rgba(image)) = Icns::parse(&file2).unwrap().best(128) else {
            panic!("no picture");
        };
        assert_eq!(&image.pixels[..4], &[1, 2, 2, 255]);
    }

    #[test]
    fn the_resource_fork_of_a_folder_icon() {
        use crate::mac::resource_map::test_fork::build;
        let icon = file(&[(*b"icp4", fake_png())]);
        let fork = MemSource::new(build(&[(*b"icns", vec![(ICNS_RESOURCE_ID, None, &icon)])]));
        let icns = Icns::from_resource_fork(&fork).unwrap().unwrap();
        assert!(icns.best(16).is_some());
        let empty = MemSource::new(build(&[(*b"STR ", vec![(1, None, b"x")])]));
        assert!(Icns::from_resource_fork(&empty).unwrap().is_none());
    }

    #[test]
    fn hostile_files() {
        let good = file(&[(*b"icp4", fake_png())]);
        // An element longer than the file, shorter than its header, a bad magic, no elements.
        let mut long = good.clone();
        long[12..16].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(Icns::parse(&long).is_err());
        let mut short = good.clone();
        short[12..16].copy_from_slice(&3u32.to_be_bytes());
        assert!(Icns::parse(&short).is_err());
        assert!(Icns::parse(b"nope....").is_err());
        assert!(Icns::parse(&good[..8]).is_err());
        assert!(Icns::parse(&[]).is_err());
        // 257 elements.
        let many: Vec<([u8; 4], Vec<u8>)> = (0..257).map(|_| (*b"icp4", fake_png())).collect();
        assert!(Icns::parse(&file(&many)).is_err());
        // Packed data that stops short gives no picture, not a panic; a run past the end is cut.
        let short_rgb = file(&[(*b"is32", vec![5, 1, 2, 3])]);
        assert!(Icns::parse(&short_rgb).unwrap().best(16).is_none());
        let planes = vec![7u8; 16 * 16 * 3];
        let mut long_run = pack(&planes);
        long_run.extend_from_slice(&[0xFF, 1]);
        let ok = file(&[(*b"is32", long_run)]);
        assert!(Icns::parse(&ok).unwrap().best(16).is_some());
    }

    #[test]
    fn random_damage_never_panics() {
        let planes = vec![3u8; 16 * 16 * 3];
        let base = file(&[(*b"is32", pack(&planes)), (*b"icp4", fake_png())]);
        let mut state = 0xFACE_FEED_0123_4567u64;
        for _ in 0..3000 {
            let mut bytes = base.clone();
            for _ in 0..3 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let at = (state % bytes.len() as u64) as usize;
                bytes[at] = (state >> 20) as u8;
            }
            if let Ok(icns) = Icns::parse(&bytes) {
                let _ = icns.best(16);
                let _ = icns.best(1024);
            }
        }
    }
}

#[cfg(test)]
mod fuzz_seeds {
    use super::test_icns::*;
    use super::*;

    /// Writes seeds for the `icns` fuzz target into `DISK_ATLAS_FUZZ_SEEDS/icns`.
    #[test]
    #[ignore = "writes files; run by the fuzz job"]
    fn write_fuzz_seeds() {
        let Some(root) = std::env::var_os("DISK_ATLAS_FUZZ_SEEDS") else {
            return;
        };
        let dir = std::path::Path::new(&root).join("icns");
        std::fs::create_dir_all(&dir).expect("create seed directory");
        let mut png = PNG_MAGIC.to_vec();
        png.extend_from_slice(b"seed");
        let packed = file(&[
            (*b"is32", pack(&vec![9u8; 16 * 16 * 3])),
            (*b"s8mk", vec![255u8; 16 * 16]),
            (*b"icp4", png),
        ]);
        assert!(Icns::parse(&packed).is_ok_and(|icns| icns.best(16).is_some()));
        std::fs::write(dir.join("rgb_png"), packed).expect("write seed");
        let mut argb = b"ARGB".to_vec();
        argb.extend(pack(&vec![5u8; 16 * 16 * 4]));
        std::fs::write(dir.join("argb"), file(&[(*b"ic04", argb)])).expect("write seed");
    }
}
