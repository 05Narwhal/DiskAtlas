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

//! Windows: `\\.\PhysicalDriveN`.
//!
//! Listing opens each drive with access 0 (query only, no administrator
//! rights needed) and asks for its geometry and its storage descriptor.
//! Reading opens the drive with read access only and uses positional reads.
//! The only FFI is `DeviceIoControl`, in [`ioctl`]. Its output lands in a
//! byte buffer and is parsed field by field at offsets taken from the
//! `windows-sys` structs, so no driver output is ever reinterpreted as a
//! Rust value (the descriptor holds a `bool`, which a byte copy could make
//! invalid).

use std::fs::{File, OpenOptions};
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Storage::FileSystem::{BusTypeUsb, FILE_DEVICE_DISK, STORAGE_BUS_TYPE};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DISK_GEOMETRY, DISK_GEOMETRY_EX, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
    IOCTL_STORAGE_GET_DEVICE_NUMBER, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
    STORAGE_DEVICE_DESCRIPTOR, STORAGE_DEVICE_NUMBER, STORAGE_PROPERTY_QUERY,
    StorageDeviceProperty,
};

use super::{DeviceInfo, HostVolume, is_legal_sector_size, read_aligned};
use crate::error::{Error, IoContext, Result};
use crate::io::block::check_range;

/// `\\.\PhysicalDrive0` to `\\.\PhysicalDrive63`.
const MAX_DRIVES: u32 = 64;

/// Output buffer for the geometry query. The full answer (geometry, size,
/// partition and detection information) is about 100 bytes.
const GEOMETRY_BUF: usize = 256;

/// Output buffer for the storage descriptor: the fixed header plus the
/// vendor, product, revision and serial strings it points at.
const DESCRIPTOR_BUF: usize = 1024;

/// Largest single `seek_read`: a multiple of every legal sector size, so a
/// long read stays aligned when split.
const MAX_READ: usize = 16 << 20;

/// Every physical disk, in device-number order.
pub(super) fn list() -> Result<Vec<DeviceInfo>> {
    let mut disks = Vec::new();
    for n in 0..MAX_DRIVES {
        let path = PathBuf::from(format!(r"\\.\PhysicalDrive{n}"));
        // "Not found" means there is no such drive; any other failure means
        // this drive cannot be described. Either way it is left out.
        if let Ok(Some(info)) = query(path) {
            disks.push(info);
        }
    }
    Ok(disks)
}

/// Every drive letter whose volume sits on one physical disk, in letter
/// order.
///
/// Each letter's volume is opened with access 0 (query only, no data
/// access, no administrator rights) and asked for its device number. A
/// letter is left out when the volume cannot be opened (no such letter),
/// the query fails (a network drive, a volume spanning several disks), the
/// answer is not a disk (a CD or DVD drive), or its root cannot be read (an
/// empty card reader).
pub(super) fn host_volumes() -> Result<Vec<HostVolume>> {
    let mut volumes = Vec::new();
    for letter in 'A'..='Z' {
        let Ok(Some(number)) = volume_disk_number(letter) else {
            continue;
        };
        let root = PathBuf::from(format!("{letter}:\\"));
        if std::fs::metadata(&root).is_err() {
            continue;
        }
        volumes.push(HostVolume {
            root,
            disk: physical_drive_path(number),
        });
    }
    Ok(volumes)
}

/// The number of the physical disk under drive `letter`, or `None` when the
/// volume is not on a disk.
fn volume_disk_number(letter: char) -> io::Result<Option<u32>> {
    let file = OpenOptions::new()
        .access_mode(0)
        .open(format!(r"\\.\{letter}:"))?;
    let mut out = [0u8; size_of::<STORAGE_DEVICE_NUMBER>()];
    let n = ioctl(&file, IOCTL_STORAGE_GET_DEVICE_NUMBER, &[], &mut out)?;
    parse_device_number(out.get(..n).unwrap_or(&[]))
}

/// Parses the bytes of a `STORAGE_DEVICE_NUMBER` the driver returned.
///
/// Fails when the answer is too short to hold the type and number; returns
/// `None` when the device is not a disk. The partition number is not read.
fn parse_device_number(bytes: &[u8]) -> io::Result<Option<u32>> {
    const TYPE: usize = offset_of!(STORAGE_DEVICE_NUMBER, DeviceType);
    const NUMBER: usize = offset_of!(STORAGE_DEVICE_NUMBER, DeviceNumber);
    let short = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "storage device number: answer too short",
        )
    };
    let device_type = u32_at(bytes, TYPE).ok_or_else(short)?;
    let number = u32_at(bytes, NUMBER).ok_or_else(short)?;
    Ok((device_type == FILE_DEVICE_DISK).then_some(number))
}

/// The path [`list`] gives disk `number`, such as `\\.\PhysicalDrive0`.
fn physical_drive_path(number: u32) -> PathBuf {
    PathBuf::from(format!(r"\\.\PhysicalDrive{number}"))
}

/// Describes one drive, or `None` when it has no media or an unusable
/// sector size.
fn query(path: PathBuf) -> io::Result<Option<DeviceInfo>> {
    let file = OpenOptions::new().access_mode(0).open(&path)?;
    let Some(geometry) = geometry(&file)? else {
        return Ok(None);
    };
    if geometry.size == 0 {
        return Ok(None);
    }
    // A drive that cannot name itself is still a readable disk: list it
    // without a model rather than hide it.
    let descriptor = descriptor(&file).unwrap_or(Descriptor {
        model: None,
        removable: false,
    });
    Ok(Some(DeviceInfo {
        path,
        model: descriptor.model,
        size: geometry.size,
        sector_size: geometry.sector_size,
        removable: descriptor.removable,
    }))
}

/// Opens a drive with read access only.
pub(super) fn open(path: &Path) -> Result<Device> {
    let context = || IoContext::Path {
        op: "opening",
        path: path.to_path_buf(),
    };
    let file = File::open(path).map_err(|e| Error::io(context(), e))?;
    let geometry = geometry(&file)
        .map_err(|e| Error::io(context(), e))?
        .ok_or_else(|| {
            Error::io(
                context(),
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the drive reports a sector size that is not a power of two from 512 to 65536",
                ),
            )
        })?;
    Ok(Device {
        file,
        len: geometry.size,
        sector_size: geometry.sector_size,
    })
}

/// An open physical drive. The handle has read access only.
#[derive(Debug)]
pub(super) struct Device {
    file: File,
    len: u64,
    sector_size: u32,
}

impl Device {
    pub(super) fn len(&self) -> u64 {
        self.len
    }

    pub(super) fn sector_size(&self) -> u32 {
        self.sector_size
    }

    pub(super) fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        check_range(offset, buf.len(), self.len)?;
        if buf.is_empty() {
            return Ok(());
        }
        let len = u64::try_from(buf.len()).unwrap_or(u64::MAX);
        read_aligned(offset, buf, self.sector_size, |at, chunk| {
            read_exact_at(&self.file, chunk, at)
        })
        .map_err(|e| Error::io(IoContext::Read { offset, len }, e))
    }
}

/// Fills `buf` from `offset`, or fails. A device that ends early fails with
/// `UnexpectedEof`. `offset` and `buf.len()` are sector-aligned; each pass
/// asks for at most [`MAX_READ`] bytes so the request stays aligned.
///
/// `seek_read` also moves the handle's cursor; nothing here uses the cursor.
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    // Every successful pass shortens `buf` by at least one byte; a 0-byte
    // read ends the loop with an error.
    while !buf.is_empty() {
        let want = buf.len().min(MAX_READ);
        let attempt = match buf.get_mut(..want) {
            Some(part) => file.seek_read(part, offset),
            None => return Err(io::Error::other("read length out of range")),
        };
        match attempt {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "device ended before the read was filled",
                ));
            }
            Ok(n) => {
                let advanced = u64::try_from(n).ok();
                let rest = std::mem::take(&mut buf).get_mut(n..);
                match (rest, advanced.and_then(|a| offset.checked_add(a))) {
                    (Some(rest), Some(next)) => {
                        buf = rest;
                        offset = next;
                    }
                    _ => return Err(io::Error::other("read returned more bytes than asked")),
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Size and logical sector size of a drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Geometry {
    /// Bytes, rounded down to a whole number of sectors.
    size: u64,
    sector_size: u32,
}

/// The drive's geometry, or `None` when its sector size is unusable.
fn geometry(file: &File) -> io::Result<Option<Geometry>> {
    let mut out = [0u8; GEOMETRY_BUF];
    let n = ioctl(file, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], &mut out)?;
    parse_geometry(out.get(..n).unwrap_or(&[]))
}

/// Parses the start of a `DISK_GEOMETRY_EX` the driver returned.
///
/// Fails when the answer is too short to hold the size or the size is
/// negative; returns `None` for a sector size the block layer cannot use.
/// The size is rounded down to whole sectors: a partial last sector cannot
/// be read from a raw device, and a sector-multiple size keeps every
/// widened read inside the device.
fn parse_geometry(bytes: &[u8]) -> io::Result<Option<Geometry>> {
    const SECTOR: usize =
        offset_of!(DISK_GEOMETRY_EX, Geometry) + offset_of!(DISK_GEOMETRY, BytesPerSector);
    const SIZE: usize = offset_of!(DISK_GEOMETRY_EX, DiskSize);
    let bad = |what: &str| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("drive geometry: {what}"),
        )
    };
    let sector_size = u32_at(bytes, SECTOR).ok_or_else(|| bad("answer too short"))?;
    let size = i64_at(bytes, SIZE).ok_or_else(|| bad("answer too short"))?;
    let size = u64::try_from(size).map_err(|_| bad("negative disk size"))?;
    if !is_legal_sector_size(sector_size) {
        return Ok(None);
    }
    let size = size
        .checked_rem(u64::from(sector_size))
        .and_then(|partial| size.checked_sub(partial))
        .ok_or_else(|| bad("sector size is zero"))?;
    Ok(Some(Geometry { size, sector_size }))
}

/// What the storage descriptor says about a drive.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Descriptor {
    model: Option<String>,
    removable: bool,
}

/// The drive's `STORAGE_DEVICE_DESCRIPTOR`, parsed.
fn descriptor(file: &File) -> io::Result<Descriptor> {
    let mut query = [0u8; size_of::<STORAGE_PROPERTY_QUERY>()];
    put_i32(
        &mut query,
        offset_of!(STORAGE_PROPERTY_QUERY, PropertyId),
        StorageDeviceProperty,
    )?;
    put_i32(
        &mut query,
        offset_of!(STORAGE_PROPERTY_QUERY, QueryType),
        PropertyStandardQuery,
    )?;
    let mut out = [0u8; DESCRIPTOR_BUF];
    let n = ioctl(file, IOCTL_STORAGE_QUERY_PROPERTY, &query, &mut out)?;
    parse_descriptor(out.get(..n).unwrap_or(&[]))
}

/// Parses the bytes of a `STORAGE_DEVICE_DESCRIPTOR` the driver returned.
///
/// Every string offset is untrusted: 0 means absent, and any other value
/// must point past the fixed header and inside `bytes`. A string ends at its
/// first NUL or at the end of `bytes`. Fails when `bytes` is too short to
/// hold the fields read here.
fn parse_descriptor(bytes: &[u8]) -> io::Result<Descriptor> {
    const REMOVABLE: usize = offset_of!(STORAGE_DEVICE_DESCRIPTOR, RemovableMedia);
    const VENDOR: usize = offset_of!(STORAGE_DEVICE_DESCRIPTOR, VendorIdOffset);
    const PRODUCT: usize = offset_of!(STORAGE_DEVICE_DESCRIPTOR, ProductIdOffset);
    const BUS: usize = offset_of!(STORAGE_DEVICE_DESCRIPTOR, BusType);
    const _: () = assert!(size_of::<STORAGE_BUS_TYPE>() == 4);
    let short = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "storage descriptor: answer too short",
        )
    };
    let removable_media = bytes.get(REMOVABLE).ok_or_else(short)? != &0;
    let vendor = u32_at(bytes, VENDOR).ok_or_else(short)?;
    let product = u32_at(bytes, PRODUCT).ok_or_else(short)?;
    let bus = i32_at(bytes, BUS).ok_or_else(short)?;

    let parts: Vec<String> = [vendor, product]
        .into_iter()
        .filter_map(|offset| string_at(bytes, offset))
        .collect();
    let model = (!parts.is_empty()).then(|| parts.join(" "));
    Ok(Descriptor {
        model,
        removable: removable_media || bus == BusTypeUsb,
    })
}

/// The trimmed, non-empty string at `offset` in a storage descriptor.
fn string_at(bytes: &[u8], offset: u32) -> Option<String> {
    // Strings follow the fixed fields; an offset into them is garbage.
    const FIRST: usize = offset_of!(STORAGE_DEVICE_DESCRIPTOR, RawDeviceProperties);
    let offset = usize::try_from(offset).ok()?;
    if offset < FIRST {
        return None;
    }
    let rest = bytes.get(offset..)?;
    let text = rest.split(|&b| b == 0).next().unwrap_or(&[]);
    let text = String::from_utf8_lossy(text);
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    Some(u32::from_ne_bytes(bytes.get(at..end)?.try_into().ok()?))
}

fn i32_at(bytes: &[u8], at: usize) -> Option<i32> {
    let end = at.checked_add(4)?;
    Some(i32::from_ne_bytes(bytes.get(at..end)?.try_into().ok()?))
}

fn i64_at(bytes: &[u8], at: usize) -> Option<i64> {
    let end = at.checked_add(8)?;
    Some(i64::from_ne_bytes(bytes.get(at..end)?.try_into().ok()?))
}

fn put_i32(buf: &mut [u8], at: usize, value: i32) -> io::Result<()> {
    at.checked_add(4)
        .and_then(|end| buf.get_mut(at..end))
        .ok_or_else(|| io::Error::other("query field out of range"))?
        .copy_from_slice(&value.to_ne_bytes());
    Ok(())
}

/// Sends `code` to the device behind `file` with `input`, collecting the
/// answer in `output`. Returns how many bytes of `output` the driver filled.
#[allow(unsafe_code)]
fn ioctl(file: &File, code: u32, input: &[u8], output: &mut [u8]) -> io::Result<usize> {
    let too_long = || io::Error::new(io::ErrorKind::InvalidInput, "ioctl buffer too long");
    let in_len = u32::try_from(input.len()).map_err(|_| too_long())?;
    let out_len = u32::try_from(output.len()).map_err(|_| too_long())?;
    let in_ptr = if input.is_empty() {
        std::ptr::null()
    } else {
        input.as_ptr().cast()
    };
    let mut returned: u32 = 0;
    // SAFETY: `file` is an open handle and is borrowed for the whole call, so
    // the handle stays valid. `in_ptr`/`in_len` describe the live, readable
    // `input` slice (or null with length 0), and `output.as_mut_ptr()`/
    // `out_len` describe the live, writable `output` slice, exclusively
    // borrowed for the call; the driver writes at most `out_len` bytes. The
    // output is a byte buffer, so any bytes written are valid `u8`s.
    // `returned` is a live, writable `u32`. The handle was opened without
    // FILE_FLAG_OVERLAPPED, so a null OVERLAPPED makes the call synchronous
    // and nothing is written after it returns.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            code,
            in_ptr,
            in_len,
            output.as_mut_ptr().cast(),
            out_len,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // Never trust the count beyond the buffer actually supplied.
    Ok(usize::try_from(returned)
        .unwrap_or(usize::MAX)
        .min(output.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zeroed buffer the size of the descriptor header plus `extra` bytes,
    /// with the fields this module reads filled in.
    fn descriptor_bytes(
        removable: u8,
        bus: i32,
        vendor: u32,
        product: u32,
        extra: &[u8],
    ) -> Vec<u8> {
        let mut b = vec![0u8; size_of::<STORAGE_DEVICE_DESCRIPTOR>()];
        b[offset_of!(STORAGE_DEVICE_DESCRIPTOR, RemovableMedia)] = removable;
        let at = offset_of!(STORAGE_DEVICE_DESCRIPTOR, VendorIdOffset);
        b[at..at + 4].copy_from_slice(&vendor.to_ne_bytes());
        let at = offset_of!(STORAGE_DEVICE_DESCRIPTOR, ProductIdOffset);
        b[at..at + 4].copy_from_slice(&product.to_ne_bytes());
        let at = offset_of!(STORAGE_DEVICE_DESCRIPTOR, BusType);
        b[at..at + 4].copy_from_slice(&bus.to_ne_bytes());
        b.extend_from_slice(extra);
        b
    }

    const HEADER: u32 = size_of::<STORAGE_DEVICE_DESCRIPTOR>() as u32;
    const BUS_SATA: i32 = 11;

    #[test]
    fn struct_layouts_match_the_documented_offsets() {
        assert_eq!(offset_of!(STORAGE_DEVICE_DESCRIPTOR, RemovableMedia), 10);
        assert_eq!(offset_of!(STORAGE_DEVICE_DESCRIPTOR, VendorIdOffset), 12);
        assert_eq!(offset_of!(STORAGE_DEVICE_DESCRIPTOR, ProductIdOffset), 16);
        assert_eq!(offset_of!(STORAGE_DEVICE_DESCRIPTOR, BusType), 28);
        assert_eq!(
            offset_of!(STORAGE_DEVICE_DESCRIPTOR, RawDeviceProperties),
            36
        );
        assert_eq!(offset_of!(DISK_GEOMETRY_EX, DiskSize), 24);
        assert_eq!(offset_of!(DISK_GEOMETRY, BytesPerSector), 20);
        assert_eq!(size_of::<STORAGE_PROPERTY_QUERY>(), 12);
        assert_eq!(BusTypeUsb, 7);
    }

    #[test]
    fn descriptor_vendor_and_product() {
        let extra = b"WDC     \0My Passport 25E2  \0";
        let b = descriptor_bytes(0, BusTypeUsb, HEADER, HEADER + 9, extra);
        let d = parse_descriptor(&b).unwrap();
        assert_eq!(d.model.as_deref(), Some("WDC My Passport 25E2"));
        assert!(d.removable, "USB counts as removable");
    }

    #[test]
    fn descriptor_product_only_and_fixed_disk() {
        let b = descriptor_bytes(0, BUS_SATA, 0, HEADER, b"Samsung SSD 870\0");
        let d = parse_descriptor(&b).unwrap();
        assert_eq!(d.model.as_deref(), Some("Samsung SSD 870"));
        assert!(!d.removable);
        let b = descriptor_bytes(1, BUS_SATA, 0, 0, &[]);
        let d = parse_descriptor(&b).unwrap();
        assert_eq!(d.model, None);
        assert!(d.removable, "removable media flag");
        let b = descriptor_bytes(2, BUS_SATA, 0, 0, &[]);
        assert!(parse_descriptor(&b).unwrap().removable, "any non-zero byte");
    }

    #[test]
    fn descriptor_hostile_offsets() {
        // Past the end, at the end, inside the header, u32::MAX: all absent.
        let len = HEADER + 4;
        for bad in [len, len + 1, 4, HEADER - 1, u32::MAX] {
            let b = descriptor_bytes(0, BUS_SATA, bad, bad, b"abc\0");
            assert_eq!(parse_descriptor(&b).unwrap().model, None, "{bad}");
        }
        // No NUL: the string runs to the end of the answer.
        let b = descriptor_bytes(0, BUS_SATA, 0, HEADER, b"  Disk");
        assert_eq!(parse_descriptor(&b).unwrap().model.as_deref(), Some("Disk"));
        // Blank and invalid UTF-8 strings.
        let b = descriptor_bytes(0, BUS_SATA, HEADER, HEADER, b"   \0");
        assert_eq!(parse_descriptor(&b).unwrap().model, None);
        let b = descriptor_bytes(0, BUS_SATA, 0, HEADER, b"A\xFFB\0");
        assert_eq!(
            parse_descriptor(&b).unwrap().model.as_deref(),
            Some("A\u{FFFD}B")
        );
    }

    #[test]
    fn descriptor_too_short_fails() {
        let b = descriptor_bytes(0, BusTypeUsb, 0, 0, &[]);
        let need = offset_of!(STORAGE_DEVICE_DESCRIPTOR, BusType) + 4;
        assert!(parse_descriptor(&b[..need]).is_ok());
        for cut in [0, 8, 11, 15, need - 1] {
            let err = parse_descriptor(&b[..cut]).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{cut}");
        }
    }

    fn geometry_bytes(sector: u32, size: i64) -> Vec<u8> {
        let mut b = vec![0u8; size_of::<DISK_GEOMETRY_EX>()];
        let at = offset_of!(DISK_GEOMETRY_EX, Geometry) + offset_of!(DISK_GEOMETRY, BytesPerSector);
        b[at..at + 4].copy_from_slice(&sector.to_ne_bytes());
        let at = offset_of!(DISK_GEOMETRY_EX, DiskSize);
        b[at..at + 8].copy_from_slice(&size.to_ne_bytes());
        b
    }

    #[test]
    fn geometry_parses_and_rounds_down() {
        let g = parse_geometry(&geometry_bytes(512, 1 << 30)).unwrap();
        assert_eq!(
            g,
            Some(Geometry {
                size: 1 << 30,
                sector_size: 512
            })
        );
        let g = parse_geometry(&geometry_bytes(4096, 10_000))
            .unwrap()
            .unwrap();
        assert_eq!(g.size, 8192);
        let g = parse_geometry(&geometry_bytes(512, 0)).unwrap().unwrap();
        assert_eq!(g.size, 0);
    }

    #[test]
    fn geometry_rejects_bad_answers() {
        for sector in [0u32, 1, 520, 131_072] {
            assert_eq!(
                parse_geometry(&geometry_bytes(sector, 1 << 20)).unwrap(),
                None
            );
        }
        let err = parse_geometry(&geometry_bytes(512, -1)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let b = geometry_bytes(512, 1 << 20);
        let need = offset_of!(DISK_GEOMETRY_EX, DiskSize) + 8;
        assert!(parse_geometry(&b[..need]).is_ok());
        for cut in [0, 20, 24, need - 1] {
            let err = parse_geometry(&b[..cut]).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{cut}");
        }
    }

    fn device_number_bytes(device_type: u32, number: u32, partition: u32) -> Vec<u8> {
        let mut b = vec![0u8; size_of::<STORAGE_DEVICE_NUMBER>()];
        let at = offset_of!(STORAGE_DEVICE_NUMBER, DeviceType);
        b[at..at + 4].copy_from_slice(&device_type.to_ne_bytes());
        let at = offset_of!(STORAGE_DEVICE_NUMBER, DeviceNumber);
        b[at..at + 4].copy_from_slice(&number.to_ne_bytes());
        let at = offset_of!(STORAGE_DEVICE_NUMBER, PartitionNumber);
        b[at..at + 4].copy_from_slice(&partition.to_ne_bytes());
        b
    }

    /// `FILE_DEVICE_CD_ROM` and `FILE_DEVICE_NETWORK_FILE_SYSTEM`.
    const CD_ROM: u32 = 2;
    const NETWORK_FS: u32 = 0x14;

    #[test]
    fn device_number_layout_matches_the_documented_offsets() {
        assert_eq!(offset_of!(STORAGE_DEVICE_NUMBER, DeviceType), 0);
        assert_eq!(offset_of!(STORAGE_DEVICE_NUMBER, DeviceNumber), 4);
        assert_eq!(offset_of!(STORAGE_DEVICE_NUMBER, PartitionNumber), 8);
        assert_eq!(size_of::<STORAGE_DEVICE_NUMBER>(), 12);
        assert_eq!(FILE_DEVICE_DISK, 7);
        assert_eq!(IOCTL_STORAGE_GET_DEVICE_NUMBER, 0x002D_1080);
    }

    #[test]
    fn device_number_of_a_disk() {
        let b = device_number_bytes(FILE_DEVICE_DISK, 3, 2);
        assert_eq!(parse_device_number(&b).unwrap(), Some(3));
        let b = device_number_bytes(FILE_DEVICE_DISK, 0, 0);
        assert_eq!(parse_device_number(&b).unwrap(), Some(0));
        // The partition number is not needed: an answer cut before it parses.
        assert_eq!(parse_device_number(&b[..8]).unwrap(), Some(0));
        // Trailing bytes are ignored.
        let mut b = device_number_bytes(FILE_DEVICE_DISK, u32::MAX, u32::MAX);
        b.extend_from_slice(&[0xFF; 8]);
        assert_eq!(parse_device_number(&b).unwrap(), Some(u32::MAX));
    }

    #[test]
    fn device_number_of_other_devices_is_none() {
        for t in [0, 1, CD_ROM, 6, 8, NETWORK_FS, u32::MAX] {
            let b = device_number_bytes(t, 1, 1);
            assert_eq!(parse_device_number(&b).unwrap(), None, "{t}");
        }
    }

    #[test]
    fn device_number_too_short_fails() {
        let b = device_number_bytes(FILE_DEVICE_DISK, 1, 1);
        for cut in [0, 1, 4, 7] {
            let err = parse_device_number(&b[..cut]).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{cut}");
        }
    }

    #[test]
    fn physical_drive_paths() {
        assert_eq!(physical_drive_path(0), Path::new(r"\\.\PhysicalDrive0"));
        assert_eq!(physical_drive_path(17), Path::new(r"\\.\PhysicalDrive17"));
        assert_eq!(
            physical_drive_path(u32::MAX),
            Path::new(r"\\.\PhysicalDrive4294967295")
        );
    }

    #[test]
    fn open_missing_drive_is_an_opening_error() {
        let path = Path::new(r"\\.\PhysicalDrive999");
        match open(path).unwrap_err() {
            Error::Io {
                context: IoContext::Path { op, path: p },
                ..
            }
            | Error::AccessDenied {
                context: IoContext::Path { op, path: p },
            } => {
                assert_eq!(op, "opening");
                assert_eq!(p, path);
            }
            other => panic!("expected an opening error, got {other:?}"),
        }
    }
}
