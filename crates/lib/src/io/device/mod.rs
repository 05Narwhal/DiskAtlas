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

//! Physical disk enumeration and raw read access.
//!
//! [`list`] names the disks the system reports and needs no administrator
//! rights. [`open`] opens one for reading, read-only; without the rights to
//! do so it fails with
//! [`Error::AccessDenied`](crate::error::Error::AccessDenied), which the app
//! shows as "needs elevation". Only Windows is implemented so far; elsewhere
//! `list` is empty and `open` is unsupported.

use std::io;
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::io::block::BlockSource;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

/// A physical disk the system reports.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DeviceInfo {
    /// The path [`open`] takes, such as `\\.\PhysicalDrive3`.
    pub path: PathBuf,
    /// Vendor and product, trimmed; `None` if the device gives neither.
    pub model: Option<String>,
    /// Size in bytes.
    pub size: u64,
    /// Logical sector size in bytes: a power of two from 512 to 65536.
    pub sector_size: u32,
    /// Removable media, or attached over USB.
    pub removable: bool,
}

/// Every physical disk, in device-number order. Needs no administrator rights.
///
/// A disk that cannot be queried, has no media, or reports an unusable
/// sector size is left out rather than failing the whole list.
pub fn list() -> Result<Vec<DeviceInfo>> {
    #[cfg(target_os = "windows")]
    {
        windows::list()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(Vec::new())
    }
}

/// A host volume (a drive letter on Windows) and the physical disk it lives on.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HostVolume {
    /// The volume root, such as `C:\`.
    pub root: PathBuf,
    /// The [`DeviceInfo::path`] of the disk holding it, such as
    /// `\\.\PhysicalDrive0`.
    pub disk: PathBuf,
}

/// Every host drive letter whose volume sits on one physical disk, in letter
/// order. Needs no administrator rights.
///
/// A letter that cannot be queried (network drive, empty card reader, a
/// volume spanning several disks) is left out. Empty on other platforms.
pub fn host_volumes() -> Result<Vec<HostVolume>> {
    #[cfg(target_os = "windows")]
    {
        windows::host_volumes()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(Vec::new())
    }
}

/// Opens a disk for reading. Without rights this fails with
/// [`Error::AccessDenied`](crate::error::Error::AccessDenied).
pub fn open(path: &Path) -> Result<DeviceSource> {
    #[cfg(target_os = "windows")]
    {
        windows::open(path).map(|inner| DeviceSource { inner })
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        Err(crate::error::Error::unsupported(
            "physical disks on this platform",
        ))
    }
}

/// A physical disk opened read-only, as a [`BlockSource`].
///
/// Reads of any byte range are allowed; ranges that do not start and end on
/// a sector boundary go through a bounce buffer. Wrap it in a
/// [`CachedSource`](super::cache::CachedSource) for repeated reads.
#[derive(Debug)]
pub struct DeviceSource {
    inner: Inner,
}

#[cfg(target_os = "windows")]
type Inner = windows::Device;

/// No device can be opened on this platform yet, so no value exists.
#[cfg(not(target_os = "windows"))]
#[derive(Debug)]
enum Inner {}

impl BlockSource for DeviceSource {
    fn len(&self) -> u64 {
        #[cfg(target_os = "windows")]
        {
            self.inner.len()
        }
        #[cfg(not(target_os = "windows"))]
        {
            match self.inner {}
        }
    }

    fn sector_size(&self) -> u32 {
        #[cfg(target_os = "windows")]
        {
            self.inner.sector_size()
        }
        #[cfg(not(target_os = "windows"))]
        {
            match self.inner {}
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        #[cfg(target_os = "windows")]
        {
            self.inner.read_at(offset, buf)
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (offset, buf);
            match self.inner {}
        }
    }
}

/// True for a sector size the block layer accepts: a power of two from 512
/// to 65536.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn is_legal_sector_size(size: u32) -> bool {
    size.is_power_of_two() && (512..=65536).contains(&size)
}

/// Fills `buf` with the bytes at `offset` from a device that only accepts
/// reads whose offset and length are multiples of `sector_size`.
///
/// `read(aligned_offset, chunk)` must fill `chunk` or fail; every call it
/// receives is sector-aligned in offset and length. An aligned request goes
/// straight into `buf`; any other is widened to the covering sectors, read
/// into a bounce buffer, and the requested bytes copied out. The caller
/// keeps the range inside a device whose size is a multiple of the sector
/// size, so the widened range never reaches past the end. Errors from
/// `read` pass through unchanged.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn read_aligned<F>(offset: u64, buf: &mut [u8], sector_size: u32, mut read: F) -> io::Result<()>
where
    F: FnMut(u64, &mut [u8]) -> io::Result<()>,
{
    if buf.is_empty() {
        return Ok(());
    }
    let sector = u64::from(sector_size);
    let zero = || io::Error::new(io::ErrorKind::InvalidInput, "sector size is zero");
    let too_large = || io::Error::new(io::ErrorKind::InvalidInput, "read range overflows");
    let len = u64::try_from(buf.len()).map_err(|_| too_large())?;
    let end = offset.checked_add(len).ok_or_else(too_large)?;
    let head = offset.checked_rem(sector).ok_or_else(zero)?;
    let tail = end.checked_rem(sector).ok_or_else(zero)?;
    if head == 0 && tail == 0 {
        return read(offset, buf);
    }

    // Widen to whole sectors: [start, end_aligned).
    let start = offset.checked_sub(head).ok_or_else(too_large)?;
    let end_aligned = if tail == 0 {
        end
    } else {
        sector
            .checked_sub(tail)
            .and_then(|pad| end.checked_add(pad))
            .ok_or_else(too_large)?
    };
    let cover = end_aligned
        .checked_sub(start)
        .and_then(|c| usize::try_from(c).ok())
        .ok_or_else(too_large)?;
    let mut bounce = vec![0u8; cover];
    read(start, &mut bounce)?;

    let from = usize::try_from(head).map_err(|_| too_large())?;
    let src = from
        .checked_add(buf.len())
        .and_then(|to| bounce.get(from..to))
        .ok_or_else(too_large)?;
    buf.copy_from_slice(src);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use std::cell::Cell;

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    /// Reads `len` bytes at `offset` through `read_aligned` from a Vec-backed
    /// disk whose reader asserts every call is aligned and in range. Returns
    /// the bytes and the calls made, as (offset, length).
    fn read_through(
        disk: &[u8],
        sector: u32,
        offset: u64,
        len: usize,
    ) -> (Vec<u8>, Vec<(u64, usize)>) {
        let calls = Cell::new(Vec::new());
        let mut buf = vec![0xAAu8; len];
        read_aligned(offset, &mut buf, sector, |at, chunk| {
            let s = u64::from(sector);
            assert_eq!(at % s, 0, "unaligned offset {at}");
            assert_eq!(
                chunk.len() as u64 % s,
                0,
                "unaligned length {}",
                chunk.len()
            );
            assert!(!chunk.is_empty());
            let start = usize::try_from(at).unwrap();
            let end = start + chunk.len();
            assert!(
                end <= disk.len(),
                "read past the device end: {start}..{end}"
            );
            chunk.copy_from_slice(&disk[start..end]);
            let mut c = calls.take();
            c.push((at, chunk.len()));
            calls.set(c);
            Ok(())
        })
        .unwrap();
        (buf, calls.take())
    }

    fn check(disk: &[u8], sector: u32, offset: u64, len: usize) -> Vec<(u64, usize)> {
        let (got, calls) = read_through(disk, sector, offset, len);
        let o = usize::try_from(offset).unwrap();
        assert!(got == disk[o..o + len], "{offset} {len}");
        calls
    }

    #[test]
    fn aligned_read_goes_straight_through() {
        let disk = pattern(16 * 512);
        assert_eq!(check(&disk, 512, 1024, 2048), vec![(1024, 2048)]);
        let disk = pattern(8 * 4096);
        assert_eq!(check(&disk, 4096, 0, 8 * 4096), vec![(0, 8 * 4096)]);
    }

    #[test]
    fn unaligned_start() {
        let disk = pattern(16 * 512);
        assert_eq!(check(&disk, 512, 100, 924), vec![(0, 1024)]);
    }

    #[test]
    fn unaligned_end() {
        let disk = pattern(16 * 512);
        assert_eq!(check(&disk, 512, 512, 100), vec![(512, 512)]);
    }

    #[test]
    fn unaligned_both() {
        let disk = pattern(16 * 512);
        assert_eq!(check(&disk, 512, 100, 1000), vec![(0, 1536)]);
        let disk = pattern(4 * 4096);
        assert_eq!(check(&disk, 4096, 4000, 200), vec![(0, 8192)]);
    }

    #[test]
    fn inside_one_sector() {
        let disk = pattern(16 * 512);
        assert_eq!(check(&disk, 512, 600, 10), vec![(512, 512)]);
        assert_eq!(check(&disk, 512, 513, 510), vec![(512, 512)]);
    }

    #[test]
    fn spans_many_sectors() {
        let disk = pattern(64 * 512);
        assert_eq!(check(&disk, 512, 300, 512 * 20), vec![(0, 512 * 21)]);
    }

    #[test]
    fn ends_exactly_at_device_end() {
        let disk = pattern(8 * 512);
        let size = disk.len();
        assert_eq!(check(&disk, 512, size as u64 - 100, 100), vec![(3584, 512)]);
        assert_eq!(check(&disk, 512, 1, size - 1), vec![(0, size)]);
        assert_eq!(check(&disk, 512, 0, size), vec![(0, size)]);
    }

    #[test]
    fn empty_read_makes_no_call() {
        let mut buf = [0u8; 0];
        read_aligned(100, &mut buf, 512, |_, _| panic!("no call expected")).unwrap();
    }

    #[test]
    fn reader_error_passes_through() {
        for (offset, len) in [(0u64, 512usize), (100, 10)] {
            let mut buf = vec![0xAAu8; len];
            let err = read_aligned(offset, &mut buf, 512, |_, _| {
                Err(io::Error::new(io::ErrorKind::TimedOut, "boom"))
            })
            .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::TimedOut);
            assert_eq!(err.to_string(), "boom");
        }
        // A failed bounce read leaves the caller's buffer untouched.
        let mut buf = [0xAAu8; 10];
        let _ = read_aligned(100, &mut buf, 512, |_, chunk| {
            chunk.fill(1);
            Err(io::Error::other("boom"))
        });
        assert_eq!(buf, [0xAA; 10]);
    }

    #[test]
    fn overflowing_range_fails() {
        let mut buf = [0u8; 10];
        let err = read_aligned(u64::MAX - 5, &mut buf, 512, |_, _| Ok(())).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        let err = read_aligned(0, &mut buf, 0, |_, _| Ok(())).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn legal_sector_sizes() {
        for s in [512u32, 1024, 2048, 4096, 8192, 65536] {
            assert!(is_legal_sector_size(s), "{s}");
        }
        for s in [0u32, 1, 256, 511, 513, 520, 3000, 131_072, u32::MAX] {
            assert!(!is_legal_sector_size(s), "{s}");
        }
    }

    #[test]
    fn device_source_is_send_sync() {
        fn assert_bounds<T: Send + Sync + 'static>() {}
        assert_bounds::<DeviceSource>();
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn unsupported_elsewhere() {
        assert!(list().unwrap().is_empty());
        assert!(host_volumes().unwrap().is_empty());
        let err = open(Path::new("/dev/disk0")).unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)), "{err:?}");
    }

    /// Maps this machine's drive letters to disks. Run with `--nocapture` to
    /// see the map. Each disk is either one `list` reports or one it leaves
    /// out (no media, unusable sector size); the test prints which, and
    /// asserts nothing about this machine beyond the shape of the answer.
    #[cfg(target_os = "windows")]
    #[test]
    #[allow(clippy::print_stderr)]
    fn maps_real_drive_letters_to_disks() {
        let volumes = host_volumes().unwrap();
        let disks = list().unwrap();
        for v in &volumes {
            let listed = disks.iter().find(|d| d.path == v.disk);
            match listed {
                Some(d) => eprintln!(
                    "{} -> {} (listed: {})",
                    v.root.display(),
                    v.disk.display(),
                    d.model.as_deref().unwrap_or("no model")
                ),
                None => eprintln!(
                    "{} -> {} (not in list())",
                    v.root.display(),
                    v.disk.display()
                ),
            }
            let root = v.root.to_str().unwrap();
            assert!(
                root.len() == 3 && root.ends_with(":\\") && root.as_bytes()[0].is_ascii_uppercase(),
                "{v:?}"
            );
            let number = v
                .disk
                .to_str()
                .and_then(|p| p.strip_prefix(r"\\.\PhysicalDrive"))
                .unwrap_or_else(|| panic!("{v:?}"));
            assert!(number.parse::<u32>().is_ok(), "{v:?}");
        }
        assert!(
            volumes.windows(2).all(|w| w[0].root < w[1].root),
            "letter order, no repeats"
        );
    }

    /// Lists this machine's disks and opens the first. Run with
    /// `--nocapture` to see the list.
    #[cfg(target_os = "windows")]
    #[test]
    #[allow(clippy::print_stderr)]
    fn lists_and_opens_real_disks() {
        let disks = list().unwrap();
        for d in &disks {
            eprintln!("{d:?}");
        }
        assert!(!disks.is_empty(), "no physical disks listed");
        for d in &disks {
            assert!(d.size > 0, "{d:?}");
            assert!(is_legal_sector_size(d.sector_size), "{d:?}");
            assert_eq!(d.size % u64::from(d.sector_size), 0, "{d:?}");
        }
        let first = &disks[0];
        match open(&first.path) {
            Err(Error::AccessDenied { .. }) => {
                eprintln!(
                    "open {}: access denied (not elevated)",
                    first.path.display()
                );
            }
            Err(other) => panic!("open {}: {other:?}", first.path.display()),
            Ok(src) => {
                eprintln!("open {}: ok (elevated)", first.path.display());
                assert_eq!(src.len(), first.size);
                assert_eq!(src.sector_size(), first.sector_size);
                let sector = first.sector_size as usize;
                let mut s0 = vec![0u8; sector];
                src.read_at(0, &mut s0).unwrap();
                let mut whole = vec![0u8; 3 * 4096];
                src.read_at(0, &mut whole).unwrap();
                assert_eq!(s0[..], whole[..sector]);
                let mut part = vec![0u8; 4096];
                src.read_at(1000, &mut part).unwrap();
                assert_eq!(part[..], whole[1000..5096]);
                let mut tail = vec![0u8; 100];
                src.read_at(src.len() - 100, &mut tail).unwrap();
                assert!(src.read_at(src.len() - 99, &mut tail).is_err());
            }
        }
    }
}
