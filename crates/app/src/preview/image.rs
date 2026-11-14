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

//! The picture provider: recognises PNG, JPEG, GIF, BMP, TIFF and WebP by
//! signature, reads the file and reports the size its own header declares.
//!
//! The declared size is the provider's real job: the pipeline refuses a
//! picture whose declared size is past its limits before anything decodes it
//! (`.claude/docs/design/m5-quicklook-copy.md` §3). The bytes are hostile, so
//! every read here is a `get` that can fail, every sum is checked, and every
//! walk has a bound. A header that cannot be parsed is an error, never a
//! guess.

use std::sync::atomic::AtomicBool;

use anyhow::anyhow;
use disk_atlas_lib::io::block::BlockSource;
use gpui::ImageFormat;

use super::{Content, EncodedImage, MAX_IMAGE_FILE, PreviewProvider, Sniff, Subject, read_prefix};

/// What the user is told when the header cannot be parsed.
const DAMAGED: &str = "This picture's header is damaged.";

/// Most JPEG segments walked looking for the frame header.
const MAX_JPEG_SEGMENTS: usize = 4096;

/// Most GIF blocks walked. Sub-blocks of one block's data are not counted:
/// they are bounded by the file's length, and a large picture legitimately
/// has more than this many of them.
const MAX_GIF_BLOCKS: usize = 100_000;

/// Most TIFF directory entries looked at.
const MAX_TIFF_ENTRIES: u16 = 4096;

/// BMP `BITMAPCOREHEADER` size; the other sizes are the `BITMAPINFOHEADER`
/// family (40, 52, 56, 108, 124).
const BMP_CORE_HEADER: u32 = 12;
const BMP_INFO_HEADERS: [u32; 5] = [40, 52, 56, 108, 124];

/// Previews pictures.
pub struct ImageProvider;

impl PreviewProvider for ImageProvider {
    fn name(&self) -> &'static str {
        "image"
    }

    fn accepts(&self, _subject: &Subject, sniff: &Sniff) -> bool {
        sniff.len <= MAX_IMAGE_FILE && identify(&sniff.head).is_some()
    }

    fn load(
        &self,
        _subject: &Subject,
        _sniff: &Sniff,
        source: &dyn BlockSource,
        cancel: &AtomicBool,
    ) -> anyhow::Result<Option<Content>> {
        let limit = usize::try_from(MAX_IMAGE_FILE).unwrap_or(usize::MAX);
        let Some(bytes) = read_prefix(source, limit, cancel)? else {
            return Ok(None);
        };
        let Declared {
            format,
            width,
            height,
            frames,
        } = declare(&bytes).ok_or_else(|| anyhow!(DAMAGED))?;
        Ok(Some(Content::Image(EncodedImage {
            format,
            bytes,
            width,
            height,
            frames,
        })))
    }
}

/// What a picture's header says about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Declared {
    format: ImageFormat,
    width: u32,
    height: u32,
    frames: u32,
}

/// The format `head` starts like, by signature only.
fn identify(head: &[u8]) -> Option<ImageFormat> {
    if head.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some(ImageFormat::Png)
    } else if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(ImageFormat::Jpeg)
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        Some(ImageFormat::Gif)
    } else if head.starts_with(b"BM") {
        let size = u32_le(head, 14)?;
        (size == BMP_CORE_HEADER || BMP_INFO_HEADERS.contains(&size)).then_some(ImageFormat::Bmp)
    } else if head.starts_with(b"II*\0") || head.starts_with(b"MM\0*") {
        Some(ImageFormat::Tiff)
    } else if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP".as_slice()) {
        Some(ImageFormat::Webp)
    } else {
        None
    }
}

/// The format and the size `bytes` declares, or `None` when the header
/// cannot be parsed.
fn declare(bytes: &[u8]) -> Option<Declared> {
    let format = identify(bytes)?;
    let (width, height, frames) = match format {
        ImageFormat::Png => {
            let (width, height) = png(bytes)?;
            (width, height, 1)
        }
        ImageFormat::Jpeg => {
            let (width, height) = jpeg(bytes)?;
            (width, height, 1)
        }
        ImageFormat::Gif => gif(bytes)?,
        ImageFormat::Bmp => {
            let (width, height) = bmp(bytes)?;
            (width, height, 1)
        }
        ImageFormat::Tiff => {
            let (width, height) = tiff(bytes)?;
            (width, height, 1)
        }
        ImageFormat::Webp => {
            let (width, height) = webp(bytes)?;
            (width, height, 1)
        }
        // Never recognised by `identify`.
        ImageFormat::Svg => return None,
    };
    Some(Declared {
        format,
        width,
        height,
        frames,
    })
}

fn array<const N: usize>(bytes: &[u8], at: usize) -> Option<[u8; N]> {
    bytes.get(at..at.checked_add(N)?)?.try_into().ok()
}

fn u16_le(bytes: &[u8], at: usize) -> Option<u16> {
    array(bytes, at).map(u16::from_le_bytes)
}

fn u16_be(bytes: &[u8], at: usize) -> Option<u16> {
    array(bytes, at).map(u16::from_be_bytes)
}

fn u32_le(bytes: &[u8], at: usize) -> Option<u32> {
    array(bytes, at).map(u32::from_le_bytes)
}

fn u32_be(bytes: &[u8], at: usize) -> Option<u32> {
    array(bytes, at).map(u32::from_be_bytes)
}

/// The 24-bit little-endian value at `at`.
fn u24_le(bytes: &[u8], at: usize) -> Option<u32> {
    let [a, b, c] = array::<3>(bytes, at)?;
    Some(u32::from_le_bytes([a, b, c, 0]))
}

/// PNG: the IHDR chunk comes first (length 13 at 8, `IHDR` at 12), then
/// width and height as big-endian u32 at 16 and 20.
fn png(bytes: &[u8]) -> Option<(u32, u32)> {
    if u32_be(bytes, 8)? != 13 || bytes.get(12..16)? != b"IHDR" {
        return None;
    }
    Some((u32_be(bytes, 16)?, u32_be(bytes, 20)?))
}

/// JPEG: walk the segments for the frame header (a start-of-frame marker).
/// Height is the u16 at +5 from the `FF`, width the u16 at +7.
fn jpeg(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut at = 2usize;
    for _ in 0..MAX_JPEG_SEGMENTS {
        if *bytes.get(at)? != 0xFF {
            return None;
        }
        let marker = *bytes.get(at.checked_add(1)?)?;
        match marker {
            // Fill byte before a marker.
            0xFF => at = at.checked_add(1)?,
            // A stuffed zero is only valid inside entropy-coded data.
            0x00 => return None,
            // Markers with no length: TEM, RSTn, SOI.
            0x01 | 0xD0..=0xD8 => at = at.checked_add(2)?,
            // End of image or start of scan without a frame header.
            0xD9 | 0xDA => return None,
            // SOFn, except DHT (C4), JPG (C8) and DAC (CC).
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                let height = u16_be(bytes, at.checked_add(5)?)?;
                let width = u16_be(bytes, at.checked_add(7)?)?;
                return Some((u32::from(width), u32::from(height)));
            }
            _ => {
                // The length includes its own two bytes.
                let length = usize::from(u16_be(bytes, at.checked_add(2)?)?);
                if length < 2 {
                    return None;
                }
                at = at.checked_add(2)?.checked_add(length)?;
            }
        }
    }
    None
}

/// Where the sub-blocks starting at `at` end: each is a length byte and that
/// many bytes, up to a zero length.
fn skip_sub_blocks(bytes: &[u8], mut at: usize) -> Option<usize> {
    loop {
        let length = usize::from(*bytes.get(at)?);
        at = at.checked_add(1)?.checked_add(length)?;
        if length == 0 {
            return Some(at);
        }
    }
}

/// Bytes of a GIF colour table described by the low three bits of `packed`.
fn gif_table_size(packed: u8) -> usize {
    3usize << (usize::from(packed & 7) + 1)
}

/// GIF: size from the logical screen descriptor, frames counted by walking
/// the blocks. A file that ends mid-walk has the frames counted so far; none
/// is an error.
fn gif(bytes: &[u8]) -> Option<(u32, u32, u32)> {
    let width = u32::from(u16_le(bytes, 6)?);
    let height = u32::from(u16_le(bytes, 8)?);
    let packed = *bytes.get(10)?;
    let mut at = 13usize;
    if packed & 0x80 != 0 {
        at = at.checked_add(gif_table_size(packed))?;
    }
    let mut frames = 0u32;
    let mut finished = false;
    for _ in 0..MAX_GIF_BLOCKS {
        // The end of the file ends the walk, like the trailer does.
        let Some(&block) = bytes.get(at) else {
            finished = true;
            break;
        };
        let next = match block {
            // Extension: a label byte, then sub-blocks.
            0x21 => at
                .checked_add(2)
                .and_then(|after_label| skip_sub_blocks(bytes, after_label)),
            // Image: nine bytes of descriptor, a local colour table when
            // its packed byte says so, one LZW byte, then sub-blocks.
            0x2C => {
                frames = frames.saturating_add(1);
                let local = bytes.get(at.checked_add(9)?).copied();
                local.and_then(|local| {
                    let mut data = at.checked_add(10)?;
                    if local & 0x80 != 0 {
                        data = data.checked_add(gif_table_size(local))?;
                    }
                    skip_sub_blocks(bytes, data.checked_add(1)?)
                })
            }
            0x3B => {
                finished = true;
                break;
            }
            _ => return None,
        };
        // A block cut short by the end of the file ends the walk.
        let Some(next) = next else {
            finished = true;
            break;
        };
        at = next;
    }
    // Using up the bound without reaching the end is a damaged file: the
    // decoder would see frames this walk did not count.
    (finished && frames > 0).then_some((width, height, frames))
}

/// BMP: header size at 14; 12 gives u16 width and height at 18 and 20,
/// anything else i32 at 18 and 22 with a negative height meaning top-down.
fn bmp(bytes: &[u8]) -> Option<(u32, u32)> {
    let size = u32_le(bytes, 14)?;
    if size == BMP_CORE_HEADER {
        return Some((u32::from(u16_le(bytes, 18)?), u32::from(u16_le(bytes, 20)?)));
    }
    let width = i32::from_le_bytes(array(bytes, 18)?);
    let height = i32::from_le_bytes(array(bytes, 22)?);
    // A negative width has no meaning.
    Some((u32::try_from(width).ok()?, height.unsigned_abs()))
}

/// TIFF: the first directory's width (tag 256) and height (tag 257), each a
/// SHORT or a LONG held in the entry's value field.
fn tiff(bytes: &[u8]) -> Option<(u32, u32)> {
    let little = bytes.starts_with(b"II");
    let read16 = |at| {
        if little {
            u16_le(bytes, at)
        } else {
            u16_be(bytes, at)
        }
    };
    let read32 = |at| {
        if little {
            u32_le(bytes, at)
        } else {
            u32_be(bytes, at)
        }
    };
    let directory = usize::try_from(read32(4)?).ok()?;
    let count = read16(directory)?.min(MAX_TIFF_ENTRIES);
    let mut width = None;
    let mut height = None;
    for index in 0..usize::from(count) {
        let entry = index
            .checked_mul(12)?
            .checked_add(directory)?
            .checked_add(2)?;
        let tag = read16(entry)?;
        if tag != 256 && tag != 257 {
            continue;
        }
        let value = match read16(entry.checked_add(2)?)? {
            3 => u32::from(read16(entry.checked_add(8)?)?),
            4 => read32(entry.checked_add(8)?)?,
            _ => return None,
        };
        if tag == 256 {
            width = Some(value);
        } else {
            height = Some(value);
        }
        if width.is_some() && height.is_some() {
            break;
        }
    }
    Some((width?, height?))
}

/// WebP: the first chunk after `WEBP` says how the size is stored.
fn webp(bytes: &[u8]) -> Option<(u32, u32)> {
    match bytes.get(12..16)? {
        // Lossy: 3-byte frame tag, start code 9D 01 2A, then 14-bit width
        // and height (the top two bits of each are a scale) at 26 and 28.
        b"VP8 " => {
            if bytes.get(23..26)? != [0x9D, 0x01, 0x2A] {
                return None;
            }
            Some((
                u32::from(u16_le(bytes, 26)? & 0x3FFF),
                u32::from(u16_le(bytes, 28)? & 0x3FFF),
            ))
        }
        // Lossless: signature 2F, then 14 bits of width - 1 and 14 bits of
        // height - 1 packed little-endian.
        b"VP8L" => {
            if *bytes.get(20)? != 0x2F {
                return None;
            }
            let bits = u32_le(bytes, 21)?;
            Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
        }
        // Extended: canvas width - 1 and height - 1 as 24-bit values.
        b"VP8X" => Some((
            u24_le(bytes, 24)?.checked_add(1)?,
            u24_le(bytes, 27)?.checked_add(1)?,
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use disk_atlas_lib::error::{Error, Result};
    use disk_atlas_lib::fs::{DirStream, FileSystem, Metadata, NodeId, Stream, VolumeInfo};
    use disk_atlas_lib::io::block::MemSource;

    use super::*;

    /// A volume with nothing on it: the provider never reads its subject.
    struct NoFs;

    impl FileSystem for NoFs {
        fn info(&self) -> VolumeInfo {
            VolumeInfo::new("none")
        }
        fn root(&self) -> NodeId {
            NodeId(1)
        }
        fn metadata(&self, node: NodeId) -> Result<Metadata> {
            Err(Error::NotFound { node: node.0 })
        }
        fn lookup(&self, _dir: NodeId, _name: &str) -> Result<Option<NodeId>> {
            Ok(None)
        }
        fn read_dir(&self, dir: NodeId) -> Result<Box<dyn DirStream>> {
            Err(Error::NotADirectory { node: dir.0 })
        }
    }

    fn subject() -> Subject {
        Subject {
            fs: Arc::new(NoFs),
            node: NodeId(2),
            name: "file".into(),
            stream: Stream::Data,
            finder_codes: None,
            format: None,
        }
    }

    fn sniff_of(bytes: &[u8]) -> Sniff {
        Sniff {
            head: bytes
                .iter()
                .copied()
                .take(super::super::SNIFF_BYTES)
                .collect(),
            len: bytes.len() as u64,
        }
    }

    // Fixtures: the smallest byte strings the headers of each format allow.

    fn png_fixture(width: u32, height: u32) -> Vec<u8> {
        let mut b = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        b.extend_from_slice(&13u32.to_be_bytes());
        b.extend_from_slice(b"IHDR");
        b.extend_from_slice(&width.to_be_bytes());
        b.extend_from_slice(&height.to_be_bytes());
        b.extend_from_slice(&[8, 6, 0, 0, 0]);
        b.extend_from_slice(&[0; 4]); // CRC, not checked here
        b
    }

    fn jpeg_fixture(width: u16, height: u16) -> Vec<u8> {
        let mut b = vec![0xFF, 0xD8];
        // APP0 with 14 bytes of payload (length 16).
        b.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x10]);
        b.extend_from_slice(&[0; 14]);
        // A fill byte and a DHT segment of length 4.
        b.extend_from_slice(&[0xFF, 0xFF, 0xC4, 0x00, 0x04, 0x01, 0x02]);
        // SOF0: length 11, precision 8, height, width, one component.
        b.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 8]);
        b.extend_from_slice(&height.to_be_bytes());
        b.extend_from_slice(&width.to_be_bytes());
        b.extend_from_slice(&[1, 1, 0x11, 0]);
        b.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02]);
        b
    }

    /// `frames` image blocks, the first with a local colour table of 2
    /// entries, behind a global table of 4 and a graphic control extension.
    fn gif_fixture(width: u16, height: u16, frames: usize) -> Vec<u8> {
        let mut b = b"GIF89a".to_vec();
        b.extend_from_slice(&width.to_le_bytes());
        b.extend_from_slice(&height.to_le_bytes());
        b.extend_from_slice(&[0x81, 0, 0]); // global table, 2^(1+1) = 4 entries
        b.extend_from_slice(&[0; 12]);
        // Graphic control extension: label F9, one sub-block of 4.
        b.extend_from_slice(&[0x21, 0xF9, 4, 0, 0, 0, 0, 0]);
        for index in 0..frames {
            b.push(0x2C);
            b.extend_from_slice(&[0; 8]);
            if index == 0 {
                b.push(0x80); // local table of 2 entries
                b.extend_from_slice(&[0; 6]);
            } else {
                b.push(0);
            }
            b.push(2); // LZW minimum code size
            b.extend_from_slice(&[3, 1, 2, 3, 0]); // one sub-block of 3, then end
        }
        b.push(0x3B);
        b
    }

    fn bmp_fixture(header: u32, width: i32, height: i32) -> Vec<u8> {
        let mut b = b"BM".to_vec();
        b.extend_from_slice(&[0; 12]);
        b.extend_from_slice(&header.to_le_bytes());
        if header == 12 {
            b.extend_from_slice(&(width as u16).to_le_bytes());
            b.extend_from_slice(&(height as u16).to_le_bytes());
            b.extend_from_slice(&[1, 0, 24, 0]);
        } else {
            b.extend_from_slice(&width.to_le_bytes());
            b.extend_from_slice(&height.to_le_bytes());
            b.extend_from_slice(&[1, 0, 24, 0]);
            b.resize(14 + header as usize, 0);
        }
        b
    }

    fn tiff_fixture(big: bool, width: u32, height: u32) -> Vec<u8> {
        let u16b = |v: u16| {
            if big {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        let u32b = |v: u32| {
            if big {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        let mut b = if big {
            b"MM\0*".to_vec()
        } else {
            b"II*\0".to_vec()
        };
        b.extend_from_slice(&u32b(8));
        b.extend_from_slice(&u16b(3)); // three entries
        // Tag 254 (ignored), width as a SHORT, height as a LONG.
        for (tag, kind, value) in [(254u16, 4u16, 0u32), (256, 3, width), (257, 4, height)] {
            b.extend_from_slice(&u16b(tag));
            b.extend_from_slice(&u16b(kind));
            b.extend_from_slice(&u32b(1));
            if kind == 3 {
                b.extend_from_slice(&u16b(value as u16));
                b.extend_from_slice(&[0, 0]);
            } else {
                b.extend_from_slice(&u32b(value));
            }
        }
        b.extend_from_slice(&u32b(0));
        b
    }

    fn riff(chunk: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut b = b"RIFF".to_vec();
        b.extend_from_slice(&(4 + 8 + payload.len() as u32).to_le_bytes());
        b.extend_from_slice(b"WEBP");
        b.extend_from_slice(chunk);
        b.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        b.extend_from_slice(payload);
        b
    }

    fn webp_lossy(width: u16, height: u16) -> Vec<u8> {
        let mut p = vec![0x10, 0, 0, 0x9D, 0x01, 0x2A];
        // The top two bits are a scale and must be ignored.
        p.extend_from_slice(&(width | 0x4000).to_le_bytes());
        p.extend_from_slice(&(height | 0xC000).to_le_bytes());
        riff(b"VP8 ", &p)
    }

    fn webp_lossless(width: u32, height: u32) -> Vec<u8> {
        let mut p = vec![0x2F];
        p.extend_from_slice(&((width - 1) | ((height - 1) << 14) | (1 << 28)).to_le_bytes());
        riff(b"VP8L", &p)
    }

    fn webp_extended(width: u32, height: u32) -> Vec<u8> {
        let mut p = vec![0x10, 0, 0, 0];
        p.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
        p.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
        riff(b"VP8X", &p)
    }

    fn size(bytes: &[u8]) -> Option<(ImageFormat, u32, u32, u32)> {
        declare(bytes).map(|d| (d.format, d.width, d.height, d.frames))
    }

    /// Feeds every prefix of `bytes` to the parser: none panics, the ones
    /// shorter than `needed` are errors, and from `needed` on the answer is
    /// the full file's (frames aside, which a GIF counts as it goes).
    fn check_truncation(bytes: &[u8], needed: usize) {
        let full = declare(bytes).expect("the whole fixture parses");
        for n in 0..=bytes.len() {
            let got = declare(&bytes[..n]);
            if n < needed {
                assert!(got.is_none(), "{n} bytes should not parse");
            } else {
                let got = got.unwrap_or_else(|| panic!("{n} bytes should parse"));
                assert_eq!(
                    (got.format, got.width, got.height),
                    (full.format, full.width, full.height)
                );
            }
        }
    }

    #[test]
    fn png_declares_its_size() {
        let png = png_fixture(640, 480);
        assert_eq!(size(&png), Some((ImageFormat::Png, 640, 480, 1)));
        check_truncation(&png, 24);
    }

    #[test]
    fn png_without_ihdr_first_is_damaged() {
        let mut png = png_fixture(1, 1);
        png[12..16].copy_from_slice(b"IDAT");
        assert_eq!(size(&png), None);
        let mut png = png_fixture(1, 1);
        png[8..12].copy_from_slice(&12u32.to_be_bytes());
        assert_eq!(size(&png), None);
    }

    #[test]
    fn jpeg_declares_its_size_past_other_segments() {
        let jpeg = jpeg_fixture(1920, 1080);
        assert_eq!(size(&jpeg), Some((ImageFormat::Jpeg, 1920, 1080, 1)));
        // SOF is at 27 (SOI 2, APP0 18, fill byte 1, DHT 6); its width ends
        // at +9.
        check_truncation(&jpeg, 36);
    }

    #[test]
    fn jpeg_without_a_frame_header_is_damaged() {
        // Start of scan before any frame header.
        assert_eq!(size(&[0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02]), None);
        // A segment length of 0 or 1 cannot be skipped.
        assert_eq!(size(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x01, 0, 0]), None);
        // A byte that is not 0xFF where a marker should start.
        assert_eq!(size(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x02, 0x12]), None);
    }

    #[test]
    fn jpeg_segment_walk_is_bounded() {
        // Far more empty segments than the cap allows, then a frame header.
        let mut jpeg = vec![0xFF, 0xD8];
        for _ in 0..(MAX_JPEG_SEGMENTS + 10) {
            jpeg.extend_from_slice(&[0xFF, 0xE1, 0x00, 0x02]);
        }
        jpeg.extend_from_slice(&[0xFF, 0xC0, 0, 11, 8, 0, 1, 0, 1]);
        assert_eq!(size(&jpeg), None);
    }

    #[test]
    fn gif_declares_size_and_counts_two_frames() {
        let gif = gif_fixture(320, 200, 2);
        assert_eq!(size(&gif), Some((ImageFormat::Gif, 320, 200, 2)));
        assert_eq!(size(&gif_fixture(1, 1, 1)).map(|s| s.3), Some(1));
        assert_eq!(size(&gif_fixture(1, 1, 7)).map(|s| s.3), Some(7));
    }

    #[test]
    fn gif_truncated_counts_what_is_there() {
        let gif = gif_fixture(320, 200, 2);
        let first_image = 6 + 7 + 12 + 8; // header, screen, table, extension
        for n in 0..=gif.len() {
            let got = size(&gif[..n]);
            if n <= first_image {
                assert_eq!(got, None, "{n} bytes");
            } else {
                let (_, width, height, frames) = got.expect("an image block was seen");
                assert_eq!((width, height), (320, 200));
                assert!((1..=2).contains(&frames));
            }
        }
    }

    #[test]
    fn gif_without_images_or_with_junk_is_damaged() {
        let mut gif = gif_fixture(4, 4, 0);
        assert_eq!(size(&gif), None);
        let at = gif.len() - 1;
        gif[at] = 0x99; // not a block type
        assert_eq!(size(&gif), None);
    }

    #[test]
    fn gif_block_walk_is_bounded() {
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&[1, 0, 1, 0, 0, 0, 0]);
        // An endless run of empty extensions, then an image.
        for _ in 0..(MAX_GIF_BLOCKS + 10) {
            gif.extend_from_slice(&[0x21, 0xF9, 0]);
        }
        gif.extend_from_slice(&[0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0, 2, 0, 0x3B]);
        assert_eq!(size(&gif), None);
    }

    #[test]
    fn bmp_with_header_sizes_12_and_40_and_the_rest() {
        let core = bmp_fixture(12, 100, 50);
        assert_eq!(size(&core), Some((ImageFormat::Bmp, 100, 50, 1)));
        check_truncation(&core, 22);

        let info = bmp_fixture(40, 640, 480);
        assert_eq!(size(&info), Some((ImageFormat::Bmp, 640, 480, 1)));
        check_truncation(&info, 26);

        // Top-down bitmaps store a negative height.
        assert_eq!(
            size(&bmp_fixture(40, 640, -480)),
            Some((ImageFormat::Bmp, 640, 480, 1))
        );
        for header in [52, 56, 108, 124] {
            assert_eq!(
                size(&bmp_fixture(header, 3, 2)).map(|s| (s.1, s.2)),
                Some((3, 2))
            );
        }
        // Negative width and a header size outside the list.
        assert_eq!(size(&bmp_fixture(40, -3, 2)), None);
        assert_eq!(identify(&bmp_fixture(41, 3, 2)), None);
        assert_eq!(identify(&bmp_fixture(0, 3, 2)), None);
        // i32::MIN height must not overflow.
        assert_eq!(
            size(&bmp_fixture(40, 1, i32::MIN)).map(|s| s.2),
            Some(1 << 31)
        );
    }

    #[test]
    fn tiff_in_both_byte_orders() {
        for big in [false, true] {
            let tiff = tiff_fixture(big, 800, 600);
            assert_eq!(
                size(&tiff),
                Some((ImageFormat::Tiff, 800, 600, 1)),
                "big: {big}"
            );
            // The height is the third entry: 10 + 2 * 12, its value at +8 to +12.
            check_truncation(&tiff, 46);
        }
    }

    #[test]
    fn tiff_damaged_directory_is_an_error() {
        // The directory offset points past the end.
        let mut tiff = tiff_fixture(false, 8, 8);
        tiff[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(size(&tiff), None);
        // No height entry.
        let mut tiff = tiff_fixture(false, 8, 8);
        tiff[8..10].copy_from_slice(&2u16.to_le_bytes());
        assert_eq!(size(&tiff), None);
        // A dimension of an unsupported type.
        let mut tiff = tiff_fixture(false, 8, 8);
        tiff[10 + 12 + 2..10 + 12 + 4].copy_from_slice(&5u16.to_le_bytes());
        assert_eq!(size(&tiff), None);
    }

    #[test]
    fn tiff_entry_count_is_capped() {
        // 65535 entries claimed, the file holds three: the walk is held to
        // the cap and to the data, and still finds the size.
        let mut tiff = tiff_fixture(false, 8, 8);
        tiff[8..10].copy_from_slice(&u16::MAX.to_le_bytes());
        assert_eq!(size(&tiff).map(|s| (s.1, s.2)), Some((8, 8)));
    }

    #[test]
    fn webp_lossy_lossless_and_extended() {
        let lossy = webp_lossy(1024, 768);
        assert_eq!(size(&lossy), Some((ImageFormat::Webp, 1024, 768, 1)));
        check_truncation(&lossy, 30);

        let lossless = webp_lossless(16384, 1);
        assert_eq!(size(&lossless), Some((ImageFormat::Webp, 16384, 1, 1)));
        check_truncation(&lossless, 25);

        let extended = webp_extended(100_000, 200);
        assert_eq!(size(&extended), Some((ImageFormat::Webp, 100_000, 200, 1)));
        check_truncation(&extended, 30);
    }

    #[test]
    fn webp_damaged_is_an_error() {
        let mut lossy = webp_lossy(8, 8);
        lossy[23] = 0; // start code
        assert_eq!(size(&lossy), None);
        let mut lossless = webp_lossless(8, 8);
        lossless[20] = 0x2E; // signature
        assert_eq!(size(&lossless), None);
        let mut other = webp_lossy(8, 8);
        other[12..16].copy_from_slice(b"ALPH");
        assert_eq!(size(&other), None);
    }

    #[test]
    fn near_miss_signatures_are_not_pictures() {
        let near = |bytes: &[u8]| identify(bytes).is_none();
        assert!(near(b""));
        assert!(near(b"\x89PNG\r\n\x1a")); // one byte short
        assert!(near(b"\x89PNG\r\n\x1a\x0b"));
        assert!(near(&[0xFF, 0xD8]));
        assert!(near(&[0xFF, 0xD9, 0xFF]));
        assert!(near(b"GIF88a"));
        assert!(near(b"GIF89"));
        assert!(near(b"BM")); // no header size
        assert!(near(b"II*\x01"));
        assert!(near(b"MM*\0"));
        assert!(near(b"II+\0")); // BigTIFF
        assert!(near(b"RIFF\0\0\0\0WAVE"));
        assert!(near(b"RIFF\0\0\0\0WEB"));
        assert!(near(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"));
    }

    #[test]
    fn accepts_by_signature_and_size_only() {
        let png = png_fixture(1, 1);
        let accepts = |sniff: &Sniff| ImageProvider.accepts(&subject(), sniff);
        assert!(accepts(&sniff_of(&png)));
        let mut sniff = sniff_of(&png);
        sniff.len = MAX_IMAGE_FILE;
        assert!(accepts(&sniff));
        sniff.len = MAX_IMAGE_FILE + 1;
        assert!(!accepts(&sniff));
        assert!(!accepts(&sniff_of(b"plain text")));
        // The name is never consulted.
        let mut named = subject();
        named.name = "photo.png".into();
        assert!(!ImageProvider.accepts(&named, &sniff_of(b"plain text")));
    }

    #[test]
    fn load_reads_the_whole_file() {
        let png = png_fixture(12, 34);
        let source = MemSource::new(png.clone());
        let sniff = sniff_of(&png);
        let loaded = ImageProvider.load(&subject(), &sniff, &source, &AtomicBool::new(false));
        let Ok(Some(Content::Image(image))) = loaded else {
            panic!("expected an image");
        };
        assert_eq!(
            (image.format, image.width, image.height, image.frames),
            (ImageFormat::Png, 12, 34, 1)
        );
        assert_eq!(image.bytes, png);
        // Cancelled: nothing.
        let cancelled = ImageProvider.load(&subject(), &sniff, &source, &AtomicBool::new(true));
        assert!(matches!(cancelled, Ok(None)));
    }

    #[test]
    fn load_of_a_damaged_header_says_so() {
        let mut png = png_fixture(1, 1);
        png[12..16].copy_from_slice(b"XXXX");
        let source = MemSource::new(png.clone());
        let sniff = sniff_of(&png);
        let Err(error) = ImageProvider.load(&subject(), &sniff, &source, &AtomicBool::new(false))
        else {
            panic!("expected an error");
        };
        assert_eq!(error.to_string(), DAMAGED);
    }

    #[test]
    fn random_bytes_never_panic() {
        // xorshift: the same bytes every run.
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let heads: [&[u8]; 6] = [
            b"\x89PNG\r\n\x1a\n",
            &[0xFF, 0xD8, 0xFF],
            b"GIF89a",
            b"BM",
            b"II*\0",
            b"RIFF\0\0\0\0WEBP",
        ];
        for round in 0..6000usize {
            let len = (next() % 96) as usize;
            let mut bytes: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            // Most rounds start like a real picture so the parsers get deep.
            if round % 8 != 0 {
                let head = heads[round % heads.len()];
                let mut spliced = head.to_vec();
                spliced.extend_from_slice(&bytes);
                bytes = spliced;
            }
            let _ = identify(&bytes);
            let _ = declare(&bytes);
        }
        // Large values everywhere.
        for head in heads {
            let mut bytes = head.to_vec();
            bytes.resize(128, 0xFF);
            let _ = declare(&bytes);
            bytes.resize(128, 0x00);
            let _ = declare(&bytes);
        }
    }
}
