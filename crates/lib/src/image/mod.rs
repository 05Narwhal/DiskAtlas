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

//! Disk image formats. Each one yields a `BlockSource`.
//!
//! [`open`] sniffs the format from the content, never the file name, and
//! opens the image read-only. Raw, UDIF, sparse image and sparse bundle
//! have readers; DiskCopy 4.2 is recognised so it can be reported by name.

use std::io;
use std::path::Path;
use std::sync::Arc;

use crate::error::{Error, IoContext, Result};
use crate::io::block::BlockSource;
use crate::io::file::FileSource;

pub mod dc42;
pub mod plist;
pub mod raw;
pub mod sparsebundle;
pub mod sparseimage;
pub mod udif;

/// A disk image container format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ImageFormat {
    /// The file is the disk.
    Raw,
    /// An Apple disk image (`.dmg`) with a `koly` trailer.
    Udif,
    /// A folder of band files with an `Info.plist`.
    SparseBundle,
    /// A single-file sparse image (`.sparseimage`).
    SparseImage,
    /// A DiskCopy 4.2 floppy image.
    DiskCopy42,
}

impl ImageFormat {
    /// The name shown to the user.
    pub fn name(self) -> &'static str {
        match self {
            Self::Raw => "Raw disk image",
            Self::Udif => "Apple disk image (UDIF)",
            Self::SparseBundle => "Sparse bundle",
            Self::SparseImage => "Sparse image",
            Self::DiskCopy42 => "DiskCopy 4.2",
        }
    }
}

/// An opened disk image: the format it was stored in and the decoded disk.
pub struct Image {
    /// The container format.
    pub format: ImageFormat,
    /// The disk the image presents.
    pub source: Arc<dyn BlockSource>,
}

/// Size of the UDIF `koly` trailer.
const UDIF_TRAILER: usize = 512;
/// Size of the DiskCopy 4.2 header.
const DC42_HEADER: usize = 84;

/// Detects the format of the image at `path` from its content.
///
/// A folder with `Info.plist` and a `bands` folder is a sparse bundle; any
/// other folder is an error. A file is sniffed by its signatures, and is
/// raw when none matches. Encrypted images fail with
/// [`Error::Unsupported`].
pub fn detect(path: &Path) -> Result<ImageFormat> {
    if let Some(format) = detect_directory(path)? {
        return Ok(format);
    }
    let source = FileSource::open(path)?;
    sniff(&source)
}

/// Detects the format of the image at `path` and opens it.
///
/// Formats whose reader does not exist yet fail with
/// [`Error::Unsupported`], naming the format. Nothing is opened for writing.
pub fn open(path: &Path) -> Result<Image> {
    let format = detect(path)?;
    let source = match format {
        ImageFormat::Raw => raw::open(path)?,
        ImageFormat::Udif => udif::open(path)?,
        ImageFormat::SparseImage => sparseimage::open(path)?,
        ImageFormat::SparseBundle => sparsebundle::open(path)?,
        other => return Err(Error::unsupported(format!("{} files", other.name()))),
    };
    Ok(Image { format, source })
}

/// `Some(SparseBundle)` for a sparse bundle folder, `None` for a file, an
/// error for any other folder or a path that cannot be examined.
fn detect_directory(path: &Path) -> Result<Option<ImageFormat>> {
    let context = || IoContext::Path {
        op: "opening",
        path: path.to_path_buf(),
    };
    let meta = std::fs::metadata(path).map_err(|e| Error::io(context(), e))?;
    if !meta.is_dir() {
        return Ok(None);
    }
    if path.join("Info.plist").is_file() && path.join("bands").is_dir() {
        return Ok(Some(ImageFormat::SparseBundle));
    }
    Err(Error::io(
        context(),
        io::Error::from(io::ErrorKind::IsADirectory),
    ))
}

/// Reads `len` bytes at `offset`, or `None` when the source is too short.
fn read_range(source: &dyn BlockSource, offset: u64, len: usize) -> Result<Option<Vec<u8>>> {
    let wanted = u64::try_from(len).unwrap_or(u64::MAX);
    match offset.checked_add(wanted) {
        Some(end) if end <= source.len() => {}
        _ => return Ok(None),
    }
    let mut buf = vec![0u8; len];
    source.read_at(offset, &mut buf)?;
    Ok(Some(buf))
}

/// Names the format of a file from its first and last bytes.
fn sniff(source: &dyn BlockSource) -> Result<ImageFormat> {
    let len = source.len();

    // UDIF: the last 512 bytes start with "koly". Files shorter than the
    // trailer cannot be UDIF.
    if let Some(start) = len.checked_sub(UDIF_TRAILER as u64)
        && let Some(trailer) = read_range(source, start, UDIF_TRAILER)?
        && trailer.starts_with(b"koly")
    {
        return Ok(ImageFormat::Udif);
    }

    // Encrypted images: "encrcdsa" starts the file, or "cdsaencr" ends it.
    if let Some(head) = read_range(source, 0, 8)?
        && head == b"encrcdsa"
    {
        return Err(encrypted());
    }
    if let Some(start) = len.checked_sub(8)
        && let Some(tail) = read_range(source, start, 8)?
        && tail == b"cdsaencr"
    {
        return Err(encrypted());
    }

    let head_len = DC42_HEADER.min(usize::try_from(len).unwrap_or(DC42_HEADER));
    let head = read_range(source, 0, head_len)?.unwrap_or_default();

    if head.starts_with(b"sprs") {
        return Ok(ImageFormat::SparseImage);
    }
    if is_dc42(&head, len) {
        return Ok(ImageFormat::DiskCopy42);
    }
    Ok(ImageFormat::Raw)
}

fn encrypted() -> Error {
    Error::unsupported("encrypted disk images")
}

/// DiskCopy 4.2: a Pascal name length of 1 to 63 at byte 0, the "private"
/// word 0x0100 (big-endian) at byte 82, and a data size (big-endian u32 at
/// byte 64) that fits in the file after the 84-byte header.
fn is_dc42(head: &[u8], file_len: u64) -> bool {
    let (Some(&name_len), Some(private), Some(data_size)) = (
        head.first(),
        head.get(82..84).and_then(|b| <[u8; 2]>::try_from(b).ok()),
        head.get(64..68).and_then(|b| <[u8; 4]>::try_from(b).ok()),
    ) else {
        return false;
    };
    let Some(room) = file_len.checked_sub(DC42_HEADER as u64) else {
        return false;
    };
    (1..=63).contains(&name_len)
        && u16::from_be_bytes(private) == 0x0100
        && u64::from(u32::from_be_bytes(data_size)) <= room
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A unique path under the system temp directory, removed on drop
    /// whether it holds a file or a folder.
    struct Temp(PathBuf);

    impl Temp {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            Self(
                std::env::temp_dir()
                    .join(format!("disk-atlas-image-test-{}-{n}", std::process::id())),
            )
        }

        fn file(bytes: &[u8]) -> Self {
            let temp = Self::new();
            std::fs::write(&temp.0, bytes).unwrap();
            temp
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn zeros_with(len: usize, parts: &[(usize, &[u8])]) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        for (offset, bytes) in parts {
            buf[*offset..*offset + bytes.len()].copy_from_slice(bytes);
        }
        buf
    }

    fn dc42_header(data_size: u32) -> Vec<u8> {
        let mut buf = vec![0u8; DC42_HEADER];
        buf[0] = 4;
        buf[1..5].copy_from_slice(b"Disk");
        buf[64..68].copy_from_slice(&data_size.to_be_bytes());
        buf[82..84].copy_from_slice(&[0x01, 0x00]);
        buf
    }

    fn is_unsupported(err: &Error, needle: &str) -> bool {
        matches!(err, Error::Unsupported(text) if text.contains(needle))
    }

    #[test]
    fn raw_file_is_raw_and_reads_back() {
        let data: Vec<u8> = (0..3000usize).map(|i| (i % 253) as u8).collect();
        let tmp = Temp::file(&data);
        assert_eq!(detect(&tmp.0).unwrap(), ImageFormat::Raw);
        let image = open(&tmp.0).unwrap();
        assert_eq!(image.format, ImageFormat::Raw);
        assert_eq!(image.source.len(), 3000);
        let mut buf = vec![0u8; 100];
        image.source.read_at(1234, &mut buf).unwrap();
        assert_eq!(buf, data[1234..1334]);
    }

    #[test]
    fn empty_and_tiny_files_are_raw() {
        for len in [0usize, 1, 4, 7, 8, 83, 84, 511] {
            let tmp = Temp::file(&vec![0x6Bu8; len]);
            assert_eq!(detect(&tmp.0).unwrap(), ImageFormat::Raw, "len {len}");
        }
        // "koly" at the start of a file shorter than the trailer is not UDIF.
        let tmp = Temp::file(b"koly");
        assert_eq!(detect(&tmp.0).unwrap(), ImageFormat::Raw);
    }

    #[test]
    fn koly_trailer_is_udif() {
        let bytes = zeros_with(2048, &[(2048 - 512, b"koly")]);
        let tmp = Temp::file(&bytes);
        assert_eq!(detect(&tmp.0).unwrap(), ImageFormat::Udif);
        // A bare signature with every other field zero is not a usable image.
        assert!(open(&tmp.0).is_err());
        // A file that is exactly the trailer.
        let tmp = Temp::file(&zeros_with(512, &[(0, b"koly")]));
        assert_eq!(detect(&tmp.0).unwrap(), ImageFormat::Udif);
    }

    #[test]
    fn koly_not_at_the_trailer_start_is_raw() {
        let tmp = Temp::file(&zeros_with(2048, &[(2048 - 511, b"koly")]));
        assert_eq!(detect(&tmp.0).unwrap(), ImageFormat::Raw);
    }

    #[test]
    fn encrypted_images_are_unsupported() {
        let tmp = Temp::file(&zeros_with(1024, &[(0, b"encrcdsa")]));
        let err = detect(&tmp.0).unwrap_err();
        assert!(is_unsupported(&err, "encrypted disk images"), "{err}");
        let err = open(&tmp.0).err().unwrap();
        assert!(is_unsupported(&err, "encrypted disk images"), "{err}");

        let tmp = Temp::file(&zeros_with(1024, &[(1016, b"cdsaencr")]));
        let err = open(&tmp.0).err().unwrap();
        assert!(is_unsupported(&err, "encrypted disk images"), "{err}");
    }

    #[test]
    fn sparse_image_header() {
        let tmp = Temp::file(&zeros_with(4096, &[(0, b"sprs")]));
        assert_eq!(detect(&tmp.0).unwrap(), ImageFormat::SparseImage);
        // A bare signature with every other field zero is not a usable image.
        assert!(open(&tmp.0).is_err());
        // The bare signature is enough: no panic on a 4-byte file.
        let tmp = Temp::file(b"sprs");
        assert_eq!(detect(&tmp.0).unwrap(), ImageFormat::SparseImage);
    }

    #[test]
    fn diskcopy_42_header() {
        let mut bytes = dc42_header(1024);
        bytes.resize(DC42_HEADER + 1024, 0);
        let tmp = Temp::file(&bytes);
        assert_eq!(detect(&tmp.0).unwrap(), ImageFormat::DiskCopy42);
        let err = open(&tmp.0).err().unwrap();
        assert!(is_unsupported(&err, "DiskCopy 4.2 files"), "{err}");
    }

    #[test]
    fn diskcopy_42_near_misses_are_raw() {
        let sized = |mut bytes: Vec<u8>| {
            bytes.resize(DC42_HEADER + 1024, 0);
            bytes
        };
        // Data size larger than the rest of the file.
        let tmp = Temp::file(&sized(dc42_header(1025)));
        assert_eq!(detect(&tmp.0).unwrap(), ImageFormat::Raw);
        let tmp = Temp::file(&sized(dc42_header(u32::MAX)));
        assert_eq!(detect(&tmp.0).unwrap(), ImageFormat::Raw);
        // Name length 0 and 64.
        for name_len in [0u8, 64, 255] {
            let mut bytes = sized(dc42_header(1024));
            bytes[0] = name_len;
            let tmp = Temp::file(&bytes);
            assert_eq!(detect(&tmp.0).unwrap(), ImageFormat::Raw, "{name_len}");
        }
        // Wrong private word.
        let mut bytes = sized(dc42_header(1024));
        bytes[83] = 1;
        let tmp = Temp::file(&bytes);
        assert_eq!(detect(&tmp.0).unwrap(), ImageFormat::Raw);
        // Header only, nothing after it: data size 0 fits, 1 does not.
        let tmp = Temp::file(&dc42_header(0));
        assert_eq!(detect(&tmp.0).unwrap(), ImageFormat::DiskCopy42);
        let tmp = Temp::file(&dc42_header(1));
        assert_eq!(detect(&tmp.0).unwrap(), ImageFormat::Raw);
    }

    #[test]
    fn sparse_bundle_folder() {
        let tmp = Temp::new();
        std::fs::create_dir_all(tmp.0.join("bands")).unwrap();
        std::fs::write(tmp.0.join("Info.plist"), b"<plist/>").unwrap();
        assert_eq!(detect(&tmp.0).unwrap(), ImageFormat::SparseBundle);
        // An empty property list names no band size.
        assert!(open(&tmp.0).is_err());
    }

    #[test]
    fn plain_and_partial_folders_are_errors() {
        let tmp = Temp::new();
        std::fs::create_dir_all(&tmp.0).unwrap();
        for result in [detect(&tmp.0), open(&tmp.0).map(|i| i.format)] {
            match result.unwrap_err() {
                Error::Io {
                    context: IoContext::Path { op, .. },
                    source,
                } => {
                    assert_eq!(op, "opening");
                    assert_eq!(source.kind(), io::ErrorKind::IsADirectory);
                }
                other => panic!("expected an opening error, got {other:?}"),
            }
        }
        // Info.plist without bands, and bands without Info.plist.
        std::fs::write(tmp.0.join("Info.plist"), b"x").unwrap();
        assert!(detect(&tmp.0).is_err());
        std::fs::remove_file(tmp.0.join("Info.plist")).unwrap();
        std::fs::create_dir(tmp.0.join("bands")).unwrap();
        assert!(detect(&tmp.0).is_err());
    }

    #[test]
    fn missing_path_is_an_error() {
        let tmp = Temp::new();
        match detect(&tmp.0).unwrap_err() {
            Error::Io { source, .. } => assert_eq!(source.kind(), io::ErrorKind::NotFound),
            other => panic!("expected NotFound, got {other:?}"),
        }
        assert!(open(&tmp.0).is_err());
    }

    #[test]
    fn format_names() {
        assert_eq!(ImageFormat::Raw.name(), "Raw disk image");
        assert_eq!(ImageFormat::Udif.name(), "Apple disk image (UDIF)");
        assert_eq!(ImageFormat::SparseBundle.name(), "Sparse bundle");
        assert_eq!(ImageFormat::SparseImage.name(), "Sparse image");
        assert_eq!(ImageFormat::DiskCopy42.name(), "DiskCopy 4.2");
    }
}
