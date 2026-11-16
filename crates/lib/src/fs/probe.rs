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

//! The probe registry: names what a source contains from its bytes alone,
//! and mounts the contents that have a reader.

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::fs::{FileSystem, hfs, hfsplus};
use crate::io::block::BlockSource;

/// How many bytes from the start of a source the probes see. ISO 9660's
/// descriptor at byte 32769 and the UDF recognition sequence are inside it.
pub const PROBE_BYTES: usize = 64 * 1024;

/// What a disk or partition contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Content {
    /// An APFS container.
    Apfs,
    /// An HFS+ volume.
    HfsPlus,
    /// An HFSX (case-sensitive HFS+) volume.
    Hfsx,
    /// An HFS+ volume inside an HFS wrapper.
    HfsWrapped,
    /// A classic HFS volume.
    Hfs,
    /// A Macintosh File System volume (400K floppies, before HFS). Detected
    /// and reported; there is no reader.
    Mfs,
    /// A Core Storage physical volume (Fusion Drive, FileVault 2 on HFS+).
    CoreStorage,
    /// An NTFS volume.
    Ntfs,
    /// An exFAT volume.
    ExFat,
    /// A FAT12, FAT16 or FAT32 volume.
    Fat,
    /// An ext2, ext3 or ext4 volume.
    Ext,
    /// A UDF volume.
    Udf,
    /// An ISO 9660 volume.
    Iso9660,
    /// Nothing but zeros (or no bytes at all).
    Empty,
    /// Bytes that no probe recognises.
    Unknown,
}

impl Content {
    /// The name the sidebar shows. A wrapped HFS+ volume shows as "HFS+".
    pub fn format_name(self) -> &'static str {
        match self {
            Self::Apfs => "APFS",
            Self::HfsPlus | Self::HfsWrapped => "HFS+",
            Self::Hfsx => "HFSX",
            Self::Hfs => "HFS",
            Self::Mfs => "MFS",
            Self::CoreStorage => "Core Storage",
            Self::Ntfs => "NTFS",
            Self::ExFat => "exFAT",
            Self::Fat => "FAT",
            Self::Ext => "ext",
            Self::Udf => "UDF",
            Self::Iso9660 => "ISO 9660",
            Self::Empty => "Empty",
            Self::Unknown => "Unknown",
        }
    }
}

/// One probe: a pure function over the first [`PROBE_BYTES`] of a source (or
/// fewer, if the source is smaller) and the source's length.
///
/// A check returns `None` when the bytes do not match or the buffer is too
/// short to hold the bytes it needs.
pub struct Probe {
    /// A short name for logs and tests.
    pub name: &'static str,
    /// The check itself.
    pub check: fn(&[u8], u64) -> Option<Content>,
}

/// Every probe, most specific first. The first match wins.
///
/// APFS, HFS+ and HFS come before ISO 9660 and UDF so that a hybrid Mac CD
/// (HFS at byte 1024, ISO 9660 at byte 32769) is named for its HFS side;
/// UDF comes before ISO 9660 because a UDF disc usually carries both.
pub static PROBES: &[Probe] = &[
    Probe {
        name: "apfs",
        check: check_apfs,
    },
    Probe {
        name: "hfs-plus",
        check: check_hfs_plus,
    },
    Probe {
        name: "hfs-wrapped",
        check: check_hfs_wrapped,
    },
    Probe {
        name: "hfs",
        check: check_hfs,
    },
    Probe {
        name: "mfs",
        check: check_mfs,
    },
    Probe {
        name: "core-storage",
        check: check_core_storage,
    },
    Probe {
        name: "ntfs",
        check: check_ntfs,
    },
    Probe {
        name: "exfat",
        check: check_exfat,
    },
    Probe {
        name: "fat",
        check: check_fat,
    },
    Probe {
        name: "ext",
        check: check_ext,
    },
    Probe {
        name: "udf",
        check: check_udf,
    },
    Probe {
        name: "iso9660",
        check: check_iso9660,
    },
];

/// Reads the first [`PROBE_BYTES`] of `source` (one read) and names its
/// content.
///
/// An empty source or an all-zero buffer is [`Content::Empty`]; bytes that
/// no probe recognises are [`Content::Unknown`]; a read failure is an error.
pub fn probe(source: &dyn BlockSource) -> Result<Content> {
    let len = source.len();
    let want = usize::try_from(len).unwrap_or(usize::MAX).min(PROBE_BYTES);
    let mut head = vec![0u8; want];
    source.read_at(0, &mut head)?;
    Ok(probe_bytes(&head, len))
}

/// The pure part of [`probe`], for callers that already hold the bytes.
/// `head` is the start of the source and `source_len` its full length.
pub fn probe_bytes(head: &[u8], source_len: u64) -> Content {
    if head.iter().all(|&b| b == 0) {
        return Content::Empty;
    }
    PROBES
        .iter()
        .find_map(|probe| (probe.check)(head, source_len))
        .unwrap_or(Content::Unknown)
}

/// What mounting a source gave.
///
/// Most formats put one volume in a partition. A container format (APFS)
/// puts several in one, sharing its space; they come back together so the
/// app can show them as one segment of the disk.
#[non_exhaustive]
pub enum Mount {
    /// One volume that has the source to itself.
    Volume(Arc<dyn FileSystem>),
    /// Several volumes sharing the source, in the container's own order.
    /// May be empty: a container with no volumes.
    Container(Vec<ContainerVolume>),
}

impl Mount {
    /// The filesystem of a single-volume mount. `None` for a container.
    pub fn into_volume(self) -> Option<Arc<dyn FileSystem>> {
        match self {
            Self::Volume(fs) => Some(fs),
            Self::Container(_) => None,
        }
    }
}

/// One volume of a container.
#[non_exhaustive]
pub struct ContainerVolume {
    /// The volume's name as stored. May be empty.
    pub name: String,
    /// What the volume is for, when the container says.
    pub role: Option<VolumeRole>,
    /// Whether it can be read.
    pub state: VolumeState,
}

impl ContainerVolume {
    /// A volume called `name`. The struct is `#[non_exhaustive]`, so this is
    /// how a format (or a test outside the engine) builds one.
    pub fn new(name: impl Into<String>, role: Option<VolumeRole>, state: VolumeState) -> Self {
        Self {
            name: name.into(),
            role,
            state,
        }
    }
}

/// What a volume of a container is for. The app orders and dims sidebar
/// rows by this; it never decides what can be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum VolumeRole {
    /// The operating system (read-only, sealed on current macOS).
    System,
    /// The user's files.
    Data,
    /// Boot support files.
    Preboot,
    /// The recovery system.
    Recovery,
    /// Swap and sleep images.
    Vm,
}

impl VolumeRole {
    /// The role as the sidebar shows it.
    pub fn name(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Data => "Data",
            Self::Preboot => "Preboot",
            Self::Recovery => "Recovery",
            Self::Vm => "VM",
        }
    }
}

/// Whether a volume of a container can be read.
#[non_exhaustive]
pub enum VolumeState {
    /// Mounted.
    Ready(Arc<dyn FileSystem>),
    /// Encrypted; a password opens it.
    Locked(Arc<dyn LockedVolume>),
    /// It could not be mounted. The other volumes of the container are not
    /// affected.
    Failed(Error),
}

/// An encrypted volume waiting for its password.
pub trait LockedVolume: Send + Sync {
    /// The password hint stored on the volume, if any. Answers from memory.
    fn hint(&self) -> Option<String>;

    /// Mounts the volume with `password`. Blocking, and slow by design (key
    /// derivation): call it on a background thread.
    ///
    /// Fails with `Error::WrongPassword` when the password opens no key on
    /// the volume, and `Error::Unsupported` when no password can (keys held
    /// by the hardware of the Mac that wrote it). The implementation keeps
    /// no copy of `password`.
    fn unlock(&self, password: &str) -> Result<Arc<dyn FileSystem>>;
}

/// Mounts `source` as what `content` names (from [`probe`]).
///
/// `Ok(None)` when there is no reader for that content yet. `Err` when
/// there is one and the source cannot be mounted at all; a container whose
/// volumes fail one by one is `Ok` with each failure in its
/// [`VolumeState`].
pub fn mount(source: Arc<dyn BlockSource>, content: Content) -> Result<Option<Mount>> {
    match content {
        Content::HfsPlus | Content::Hfsx | Content::HfsWrapped => {
            Ok(Some(Mount::Volume(Arc::new(hfsplus::mount(source)?))))
        }
        Content::Hfs => Ok(Some(Mount::Volume(Arc::new(hfs::mount(source)?)))),
        Content::Fat => Ok(Some(Mount::Volume(Arc::new(crate::fs::fat::mount(
            source,
        )?)))),
        Content::Apfs
        | Content::Mfs
        | Content::CoreStorage
        | Content::Ntfs
        | Content::ExFat
        | Content::Ext
        | Content::Udf
        | Content::Iso9660
        | Content::Empty
        | Content::Unknown => Ok(None),
    }
}

/// `len` bytes at `offset`, or `None` when the buffer is too short.
fn bytes_at(head: &[u8], offset: usize, len: usize) -> Option<&[u8]> {
    head.get(offset..offset.checked_add(len)?)
}

/// True when `needle` is at `offset`.
fn has(head: &[u8], offset: usize, needle: &[u8]) -> bool {
    bytes_at(head, offset, needle.len()) == Some(needle)
}

fn be16(head: &[u8], offset: usize) -> Option<u16> {
    let b = bytes_at(head, offset, 2)?;
    Some(u16::from_be_bytes(b.try_into().ok()?))
}

fn le16(head: &[u8], offset: usize) -> Option<u16> {
    let b = bytes_at(head, offset, 2)?;
    Some(u16::from_le_bytes(b.try_into().ok()?))
}

/// APFS: `NXSB` right after the 32-byte object header of the container
/// superblock.
fn check_apfs(head: &[u8], _len: u64) -> Option<Content> {
    has(head, 32, b"NXSB").then_some(Content::Apfs)
}

/// HFS+ (`H+`, version 4) and HFSX (`HX`, version 5): the volume header at
/// byte 1024.
fn check_hfs_plus(head: &[u8], _len: u64) -> Option<Content> {
    let signature = be16(head, 1024)?;
    let version = be16(head, 1026)?;
    match (signature, version) {
        (0x482B, 4) => Some(Content::HfsPlus),
        (0x4858, 5) => Some(Content::Hfsx),
        _ => None,
    }
}

/// An HFS master directory block (`BD`) whose `drEmbedSigWord`, at offset
/// 0x7C of the block, is `H+`.
fn check_hfs_wrapped(head: &[u8], _len: u64) -> Option<Content> {
    (be16(head, 1024)? == 0x4244 && be16(head, 1024 + 0x7C)? == 0x482B)
        .then_some(Content::HfsWrapped)
}

/// A classic HFS master directory block: `BD` at byte 1024.
fn check_hfs(head: &[u8], _len: u64) -> Option<Content> {
    (be16(head, 1024)? == 0x4244).then_some(Content::Hfs)
}

/// A Macintosh File System master directory block: `D2 D7` at byte 1024.
fn check_mfs(head: &[u8], _len: u64) -> Option<Content> {
    (be16(head, 1024)? == 0xD2D7).then_some(Content::Mfs)
}

/// Core Storage physical volume header: format version 1 at byte 8, block
/// type 0x0010 at byte 10 (both little-endian) and `CS` at byte 88.
fn check_core_storage(head: &[u8], _len: u64) -> Option<Content> {
    (has(head, 88, b"CS") && le16(head, 8)? == 1 && le16(head, 10)? == 0x0010)
        .then_some(Content::CoreStorage)
}

/// NTFS: the OEM ID of the boot sector.
fn check_ntfs(head: &[u8], _len: u64) -> Option<Content> {
    has(head, 3, b"NTFS    ").then_some(Content::Ntfs)
}

/// exFAT: the `FileSystemName` field of the main boot sector.
fn check_exfat(head: &[u8], _len: u64) -> Option<Content> {
    has(head, 3, b"EXFAT   ").then_some(Content::ExFat)
}

/// FAT12/16/32: boot signature, a filesystem type label and a plausible
/// bytes-per-sector value. The label is informational and not trustworthy
/// on its own, so the boot signature and the BPB field are required too.
fn check_fat(head: &[u8], _len: u64) -> Option<Content> {
    if !has(head, 510, &[0x55, 0xAA]) {
        return None;
    }
    let label = has(head, 54, b"FAT") || has(head, 82, b"FAT32");
    let sector = matches!(le16(head, 11)?, 512 | 1024 | 2048 | 4096);
    (label && sector).then_some(Content::Fat)
}

/// ext2/3/4: `s_magic` at offset 56 of the superblock, which starts at 1024.
fn check_ext(head: &[u8], _len: u64) -> Option<Content> {
    (le16(head, 1024 + 56)? == 0xEF53).then_some(Content::Ext)
}

/// UDF: a Volume Recognition Sequence of 2048-byte descriptors from byte
/// 32768, with `NSR02` or `NSR03` as the identifier (byte 1) of one of them.
fn check_udf(head: &[u8], _len: u64) -> Option<Content> {
    (0..8usize)
        .filter_map(|k| k.checked_mul(2048)?.checked_add(32768)?.checked_add(1))
        .any(|at| has(head, at, b"NSR02") || has(head, at, b"NSR03"))
        .then_some(Content::Udf)
}

/// ISO 9660: `CD001` at byte 1 of the first volume descriptor (byte 32768).
fn check_iso9660(head: &[u8], _len: u64) -> Option<Content> {
    has(head, 32769, b"CD001").then_some(Content::Iso9660)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{Error, IoContext};
    use crate::io::block::MemSource;

    /// A zero buffer holding each `bytes` at its `offset`, just long enough.
    fn fixture(parts: &[(usize, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        for (offset, bytes) in parts {
            put(&mut buf, *offset, bytes);
        }
        buf
    }

    fn put(buf: &mut Vec<u8>, offset: usize, bytes: &[u8]) {
        let end = offset + bytes.len();
        if buf.len() < end {
            buf.resize(end, 0);
        }
        buf[offset..end].copy_from_slice(bytes);
    }

    fn fat_fixture() -> Vec<u8> {
        fixture(&[(11, &[0x00, 0x02]), (54, b"FAT"), (510, &[0x55, 0xAA])])
    }

    /// Smallest buffer per content, and the probe that must claim it.
    fn fixtures() -> Vec<(Content, &'static str, Vec<u8>)> {
        vec![
            (Content::Apfs, "apfs", fixture(&[(32, b"NXSB")])),
            (
                Content::HfsPlus,
                "hfs-plus",
                fixture(&[(1024, b"H+"), (1026, &[0, 4])]),
            ),
            (
                Content::Hfsx,
                "hfs-plus",
                fixture(&[(1024, b"HX"), (1026, &[0, 5])]),
            ),
            (
                Content::HfsWrapped,
                "hfs-wrapped",
                fixture(&[(1024, b"BD"), (1148, b"H+")]),
            ),
            (Content::Hfs, "hfs", fixture(&[(1024, b"BD")])),
            (Content::Mfs, "mfs", fixture(&[(1024, &[0xD2, 0xD7])])),
            (
                Content::CoreStorage,
                "core-storage",
                fixture(&[(8, &[1, 0, 0x10, 0]), (88, b"CS")]),
            ),
            (Content::Ntfs, "ntfs", fixture(&[(3, b"NTFS    ")])),
            (Content::ExFat, "exfat", fixture(&[(3, b"EXFAT   ")])),
            (Content::Fat, "fat", fat_fixture()),
            (Content::Ext, "ext", fixture(&[(1080, &[0x53, 0xEF])])),
            (Content::Udf, "udf", fixture(&[(32769, b"NSR02")])),
            (Content::Iso9660, "iso9660", fixture(&[(32769, b"CD001")])),
        ]
    }

    #[test]
    fn one_fixture_per_content() {
        for (content, name, buf) in fixtures() {
            assert_eq!(probe_bytes(&buf, buf.len() as u64), content, "{name}");
            let probe = PROBES.iter().find(|p| p.name == name).unwrap();
            assert_eq!((probe.check)(&buf, 0), Some(content), "{name}");
        }
    }

    #[test]
    fn every_probe_has_a_fixture_and_a_unique_name() {
        for probe in PROBES {
            assert!(fixtures().iter().any(|(_, n, _)| *n == probe.name));
            assert_eq!(PROBES.iter().filter(|p| p.name == probe.name).count(), 1);
        }
    }

    #[test]
    fn too_short_buffers_return_none() {
        for (_, name, buf) in fixtures() {
            let probe = PROBES.iter().find(|p| p.name == name).unwrap();
            let short = &buf[..buf.len() - 1];
            assert_eq!((probe.check)(short, 0), None, "{name}");
            assert_eq!((probe.check)(&[], 0), None, "{name}");
        }
    }

    #[test]
    fn near_misses_do_not_match() {
        // HFS+ and HFSX with the other's version.
        let buf = fixture(&[(1024, b"H+"), (1026, &[0, 5])]);
        assert_eq!(check_hfs_plus(&buf, 0), None);
        let buf = fixture(&[(1024, b"HX"), (1026, &[0, 4])]);
        assert_eq!(check_hfs_plus(&buf, 0), None);
        // A wrapper without the embedded signature is plain HFS.
        let buf = fixture(&[(1024, b"BD"), (1148, b"XX")]);
        assert_eq!(probe_bytes(&buf, 0), Content::Hfs);
        // "CS" alone is not Core Storage.
        assert_eq!(check_core_storage(&fixture(&[(88, b"CS")]), 0), None);
        // FAT: bad bytes per sector, missing boot signature, no label.
        let mut buf = fat_fixture();
        put(&mut buf, 11, &[0x01, 0x02]);
        assert_eq!(check_fat(&buf, 0), None);
        let mut buf = fat_fixture();
        put(&mut buf, 510, &[0, 0]);
        assert_eq!(check_fat(&buf, 0), None);
        let mut buf = fat_fixture();
        put(&mut buf, 54, b"   ");
        assert_eq!(check_fat(&buf, 0), None);
        // UDF: the descriptor is only BEA01.
        assert_eq!(check_udf(&fixture(&[(32769, b"BEA01")]), 0), None);
    }

    #[test]
    fn fat32_label_and_other_sector_sizes() {
        let buf = fixture(&[(11, &[0x00, 0x02]), (82, b"FAT32   "), (510, &[0x55, 0xAA])]);
        assert_eq!(check_fat(&buf, 0), Some(Content::Fat));
        for sector in [[0x00, 0x04], [0x00, 0x08], [0x00, 0x10]] {
            let mut buf = fat_fixture();
            put(&mut buf, 11, &sector);
            assert_eq!(check_fat(&buf, 0), Some(Content::Fat));
        }
    }

    #[test]
    fn udf_descriptor_can_be_any_of_the_first_eight() {
        for k in 0..8usize {
            let at = 32768 + k * 2048 + 1;
            assert_eq!(
                probe_bytes(&fixture(&[(at, b"NSR03")]), 0),
                Content::Udf,
                "k={k}"
            );
        }
        assert_eq!(
            probe_bytes(&fixture(&[(32768 + 8 * 2048 + 1, b"NSR03")]), 0),
            Content::Unknown
        );
    }

    #[test]
    fn udf_beats_iso9660() {
        let buf = fixture(&[(32769, b"CD001"), (32768 + 2048 * 2 + 1, b"NSR02")]);
        assert_eq!(probe_bytes(&buf, 0), Content::Udf);
    }

    #[test]
    fn hybrid_mac_cd_is_named_for_its_hfs_side() {
        let hfs = fixture(&[(1024, b"BD"), (32769, b"CD001")]);
        assert_eq!(probe_bytes(&hfs, 0), Content::Hfs);
        let hfsplus = fixture(&[(1024, b"H+"), (1026, &[0, 4]), (32769, b"CD001")]);
        assert_eq!(probe_bytes(&hfsplus, 0), Content::HfsPlus);
        let wrapped = fixture(&[(1024, b"BD"), (1148, b"H+"), (32769, b"CD001")]);
        assert_eq!(probe_bytes(&wrapped, 0), Content::HfsWrapped);
    }

    #[test]
    fn empty_and_unknown() {
        assert_eq!(probe_bytes(&[], 0), Content::Empty);
        assert_eq!(
            probe_bytes(&vec![0u8; PROBE_BYTES], 1 << 30),
            Content::Empty
        );
        let noise: Vec<u8> = (0..PROBE_BYTES)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8 | 0x01)
            .collect();
        assert_eq!(probe_bytes(&noise, 0), Content::Unknown);
        // Non-zero but unrecognised.
        assert_eq!(probe_bytes(&[0, 0, 1], 3), Content::Unknown);
    }

    #[test]
    fn format_names() {
        let names = [
            (Content::Apfs, "APFS"),
            (Content::HfsPlus, "HFS+"),
            (Content::Hfsx, "HFSX"),
            (Content::HfsWrapped, "HFS+"),
            (Content::Hfs, "HFS"),
            (Content::Mfs, "MFS"),
            (Content::CoreStorage, "Core Storage"),
            (Content::Ntfs, "NTFS"),
            (Content::ExFat, "exFAT"),
            (Content::Fat, "FAT"),
            (Content::Ext, "ext"),
            (Content::Udf, "UDF"),
            (Content::Iso9660, "ISO 9660"),
            (Content::Empty, "Empty"),
            (Content::Unknown, "Unknown"),
        ];
        for (content, name) in names {
            assert_eq!(content.format_name(), name);
        }
    }

    #[test]
    fn probe_reads_a_source_smaller_than_the_probe_buffer() {
        let mut bytes = vec![0u8; 4096];
        bytes[1024..1026].copy_from_slice(b"H+");
        bytes[1026..1028].copy_from_slice(&[0, 4]);
        let source = MemSource::new(bytes);
        assert_eq!(probe(&source).unwrap(), Content::HfsPlus);
    }

    #[test]
    fn probe_reads_only_the_first_probe_bytes() {
        let mut bytes = vec![0u8; PROBE_BYTES * 2];
        bytes[PROBE_BYTES + 32..PROBE_BYTES + 36].copy_from_slice(b"NXSB");
        assert_eq!(probe(&MemSource::new(bytes)).unwrap(), Content::Empty);
    }

    #[test]
    fn probe_of_an_empty_source_is_empty() {
        assert_eq!(probe(&MemSource::new(Vec::new())).unwrap(), Content::Empty);
    }

    struct Failing;

    impl BlockSource for Failing {
        fn len(&self) -> u64 {
            4096
        }
        fn sector_size(&self) -> u32 {
            512
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
            Err(Error::io(
                IoContext::Read {
                    offset,
                    len: buf.len() as u64,
                },
                std::io::Error::other("boom"),
            ))
        }
    }

    #[test]
    fn probe_passes_io_errors_through() {
        assert!(matches!(probe(&Failing), Err(Error::Io { .. })));
    }
}
