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

//! Whole-stack tests: a disk image container, a partition map and a volume
//! together, and deterministic damage sweeps over each layer.
//!
//! The HFS+ volume comes from the HFS+ test builder. It is stored as a raw
//! file, a UDIF file, a sparse image and a sparse bundle (each written to a
//! temporary path and opened with [`crate::image::open`]), in an Apple
//! Partition Map disk and a GPT disk. Each combination is opened, laid out,
//! probed, mounted and walked, and every file's content is compared with
//! what was written.
//!
//! The classic HFS volume comes from the classic HFS test builder. It is
//! stored bare and in an Apple Partition Map disk (in each of those
//! containers), and bare inside a DiskCopy 4.2 floppy image. It goes through
//! the same layers, and both forks of every file are compared with the
//! builder's manifest.
//!
//! The sweeps overwrite or cut off bytes at fixed pseudo-random places and
//! feed the result through the same readers: HFS+ and classic HFS volumes,
//! the image containers (UDIF, sparse image, DiskCopy 4.2) and the partition
//! maps. Any step may fail; none may panic or run without end. The walk and
//! the reads are bounded, so a structure that loops ends at a bound, which
//! is not a failure.

use std::collections::BTreeMap;
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use crate::fs::hfs;
use crate::fs::hfs::test_volume::{Built, Manifest, standard_volume};
use crate::fs::hfsplus::compression::fixtures::{content, header, pieces, zlib, zlib_fork};
use crate::fs::hfsplus::links::FILE_PRIVATE_FOLDER;
use crate::fs::hfsplus::test_volume::{FileSpec, ROOT, VolumeBuilder, XattrValue};
use crate::fs::hfsplus::{self, HfsPlus};
use crate::fs::probe::{self, Content};
use crate::fs::{DirBatch, DirProgress, FileSystem, JournalState, NodeId, NodeKind, Stream};
use crate::image::dc42;
use crate::image::sparsebundle::tests::plist_xml;
use crate::image::sparseimage::SparseImageSource;
use crate::image::udif::UdifSource;
use crate::image::udif::tests::{Block, ChunkSpec, build as build_udif};
use crate::image::{self, ImageFormat};
use crate::io::block::{BlockSource, MemSource};
use crate::partition::{self, Guid, TypeHint, gpt};

// ---- temporary paths ------------------------------------------------------

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A unique path under the system temp directory, removed on drop whether it
/// holds a file or a folder.
struct Temp(PathBuf);

impl Temp {
    fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        Self(std::env::temp_dir().join(format!("disk-atlas-e2e-{}-{n}-{tag}", std::process::id())))
    }

    fn file(tag: &str, bytes: &[u8]) -> Self {
        let temp = Self::new(tag);
        fs::write(&temp.0, bytes).unwrap();
        temp
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        let _ = fs::remove_dir_all(&self.0);
    }
}

// ---- the volume -----------------------------------------------------------

/// What a path in the test volume should be.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Expect {
    Dir,
    File(Vec<u8>),
    Link(String),
}

/// The built volume and what a walk of it should find.
struct Fixture {
    volume: Vec<u8>,
    expect: BTreeMap<String, Expect>,
}

const VOLUME_NAME: &str = "E2E Volume";

/// A volume with nested folders, plain files, empty files, a file that is
/// all zeros, two compressed files (one inline, one in the resource fork), a
/// hard link used twice, a symbolic link and an extended attribute.
fn fixture() -> Fixture {
    let mut expect = BTreeMap::new();
    let mut v = VolumeBuilder::new(VOLUME_NAME);
    let with_content = |bytes: &[u8]| FileSpec {
        content: Some(bytes.to_vec()),
        ..FileSpec::regular(0)
    };

    let readme = b"Disk Atlas end to end".to_vec();
    v.file(ROOT, "readme.txt", 16, with_content(&readme));
    expect.insert("readme.txt".to_string(), Expect::File(readme));
    v.xattr(16, "user.note", XattrValue::Inline(b"hello".to_vec()));

    v.folder(ROOT, "Docs", 20).folder(20, "Sub", 21);
    v.folder(ROOT, "Photos", 22).folder(ROOT, "Empty", 23);
    for dir in ["Docs", "Docs/Sub", "Photos", "Empty"] {
        expect.insert(dir.to_string(), Expect::Dir);
    }

    let notes = content(5000);
    v.file(20, "notes.txt", 24, with_content(&notes));
    expect.insert("Docs/notes.txt".to_string(), Expect::File(notes));
    let big = content(150_000);
    v.file(21, "big.bin", 25, with_content(&big));
    expect.insert("Docs/Sub/big.bin".to_string(), Expect::File(big));
    let zeros = vec![0u8; 70_000];
    v.file(22, "zeros.dat", 26, with_content(&zeros));
    expect.insert("Photos/zeros.dat".to_string(), Expect::File(zeros));
    v.file(22, "small.dat", 27, with_content(&[0x5A]));
    expect.insert("Photos/small.dat".to_string(), Expect::File(vec![0x5A]));
    v.file(ROOT, "empty.dat", 28, FileSpec::regular(0));
    expect.insert("empty.dat".to_string(), Expect::File(Vec::new()));

    // A compressed file with its content in the attribute (type 3, zlib).
    let inline = content(3000);
    let mut attr = header(3, inline.len() as u64);
    attr.extend_from_slice(&zlib(&inline));
    v.compressed_file(ROOT, "inline.txt", 29, &attr, None);
    expect.insert("inline.txt".to_string(), Expect::File(inline));
    // One with two chunks in the resource fork (type 4, zlib).
    let forked = content(70 * 1024);
    let chunks: Vec<Vec<u8>> = pieces(&forked).into_iter().map(zlib).collect();
    v.compressed_file(
        ROOT,
        "forked.bin",
        30,
        &header(4, forked.len() as u64),
        Some(&zlib_fork(&chunks)),
    );
    expect.insert("forked.bin".to_string(), Expect::File(forked));

    // A hard link target in the private folder, and two links to it.
    let shared = content(777);
    v.folder_units(ROOT, &FILE_PRIVATE_FOLDER, 40, 0x5000).file(
        40,
        "iNode100",
        41,
        with_content(&shared),
    );
    v.file(ROOT, "hard.txt", 50, FileSpec::file_link(100)).file(
        20,
        "hard2.txt",
        51,
        FileSpec::file_link(100),
    );
    expect.insert("hard.txt".to_string(), Expect::File(shared.clone()));
    expect.insert("Docs/hard2.txt".to_string(), Expect::File(shared));

    v.file(ROOT, "link", 52, FileSpec::symlink(b"Docs/notes.txt"));
    expect.insert("link".to_string(), Expect::Link("Docs/notes.txt".into()));

    Fixture {
        volume: v.build(),
        expect,
    }
}

// ---- walking a mounted volume ----------------------------------------------

/// What a walk found at one path.
#[derive(Debug, Clone, Default)]
struct Found {
    kind: Option<NodeKind>,
    size: Option<u64>,
    data: Option<Vec<u8>>,
    /// The resource fork, read only when the limits ask for forks.
    rsrc: Option<Vec<u8>>,
    target: Option<String>,
    xattrs: Vec<String>,
}

/// Bounds on a walk, so a damaged volume cannot make it run for long.
#[derive(Debug, Clone, Copy)]
struct Limits {
    /// Entries visited.
    entries: usize,
    /// Folder depth.
    depth: usize,
    /// Data bytes read per file.
    per_file: u64,
    /// Data bytes read in all.
    total: u64,
    /// Keep the data read (only worth it for clean volumes).
    keep: bool,
    /// Also read every resource fork (volumes that have them).
    forks: bool,
}

impl Limits {
    /// For volumes that must read back whole.
    fn clean() -> Self {
        Self {
            entries: 1 << 20,
            depth: 64,
            per_file: u64::MAX,
            total: u64::MAX,
            keep: true,
            forks: false,
        }
    }

    /// For damaged volumes: read what is there, but not forever.
    fn damaged() -> Self {
        Self {
            entries: 2000,
            depth: 24,
            per_file: 1 << 20,
            total: 4 << 20,
            keep: false,
            forks: false,
        }
    }

    /// The same limits, reading resource forks too.
    fn with_forks(self) -> Self {
        Self {
            forks: true,
            ..self
        }
    }
}

#[derive(Debug, Default)]
struct Walked {
    entries: BTreeMap<String, Found>,
    /// Calls that returned an error.
    errors: usize,
    /// A bound ended the walk early.
    bound_hit: bool,
}

/// Reads up to `cap` bytes of `source` in steps. Returns the bytes when
/// `keep`, and whether a read failed.
fn read_up_to(source: &dyn BlockSource, cap: u64, keep: bool) -> (Vec<u8>, u64, bool) {
    const STEP: usize = 64 * 1024 + 1;
    let end = source.len().min(cap);
    let mut out = Vec::new();
    let mut buf = vec![0u8; STEP];
    let mut pos = 0u64;
    while pos < end {
        let n = usize::try_from(end - pos).map_or(STEP, |left| left.min(STEP));
        if source.read_at(pos, &mut buf[..n]).is_err() {
            return (out, pos, true);
        }
        if keep {
            out.extend_from_slice(&buf[..n]);
        }
        pos += n as u64;
    }
    (out, pos, false)
}

/// Walks the whole tree of `fs`: lists every folder, reads every file's
/// metadata, content and attributes. Every call may fail.
fn walk(fs: &dyn FileSystem, limits: Limits) -> Walked {
    let mut walked = Walked::default();
    let mut read_total = 0u64;
    let mut stack = vec![(fs.root(), String::new(), 0usize)];
    while let Some((dir, path, depth)) = stack.pop() {
        if depth >= limits.depth {
            walked.bound_hit = true;
            continue;
        }
        let mut stream = match fs.read_dir(dir) {
            Ok(stream) => stream,
            Err(_) => {
                walked.errors += 1;
                continue;
            }
        };
        let mut batch = DirBatch::new();
        let mut calls = 0usize;
        loop {
            calls += 1;
            if calls > limits.entries.saturating_add(16) {
                walked.bound_hit = true;
                break;
            }
            batch.clear();
            let progress = stream.next_batch(&mut batch, 64);
            for entry in batch.iter() {
                if walked.entries.len() >= limits.entries {
                    walked.bound_hit = true;
                    return walked;
                }
                let child = if path.is_empty() {
                    entry.name.to_string()
                } else {
                    format!("{path}/{}", entry.name)
                };
                let mut found = Found {
                    kind: Some(entry.kind),
                    size: entry.size,
                    ..Found::default()
                };
                match fs.metadata(entry.node) {
                    Ok(meta) => found.target = meta.link_target,
                    Err(_) => walked.errors += 1,
                }
                match fs.xattrs(entry.node) {
                    Ok(list) => found.xattrs = list.into_iter().map(|x| x.name).collect(),
                    Err(_) => walked.errors += 1,
                }
                if entry.kind == NodeKind::File {
                    match fs.open(entry.node, Stream::Data) {
                        Ok(source) => {
                            let cap = limits.per_file.min(limits.total.saturating_sub(read_total));
                            let (bytes, read, failed) =
                                read_up_to(source.as_ref(), cap, limits.keep);
                            read_total = read_total.saturating_add(read);
                            walked.errors += usize::from(failed);
                            if limits.keep && !failed {
                                found.data = Some(bytes);
                            }
                        }
                        Err(_) => walked.errors += 1,
                    }
                    // A file without a resource fork has no such stream.
                    if limits.forks && entry.resource_size.is_none_or(|n| n > 0) {
                        match fs.open(entry.node, Stream::Resource) {
                            Ok(source) => {
                                let cap =
                                    limits.per_file.min(limits.total.saturating_sub(read_total));
                                let (bytes, read, failed) =
                                    read_up_to(source.as_ref(), cap, limits.keep);
                                read_total = read_total.saturating_add(read);
                                walked.errors += usize::from(failed);
                                if limits.keep && !failed {
                                    found.rsrc = Some(bytes);
                                }
                            }
                            Err(_) => walked.errors += 1,
                        }
                    }
                }
                if entry.is_browsable() && entry.kind == NodeKind::Directory {
                    stack.push((entry.node, child.clone(), depth + 1));
                }
                walked.entries.insert(child, found);
            }
            match progress {
                Ok(DirProgress::More) => {}
                Ok(DirProgress::Done) => break,
                Err(_) => {
                    walked.errors += 1;
                    break;
                }
            }
        }
    }
    walked
}

/// Checks that `walked` is exactly the fixture's tree with the fixture's
/// content.
fn assert_tree(walked: &Walked, expect: &BTreeMap<String, Expect>, what: &str) {
    assert_eq!(walked.errors, 0, "{what}: the walk met errors");
    assert!(!walked.bound_hit, "{what}: a bound ended the walk");
    let found: Vec<&String> = walked.entries.keys().collect();
    let wanted: Vec<&String> = expect.keys().collect();
    assert_eq!(found, wanted, "{what}: the tree differs");
    for (path, want) in expect {
        let got = &walked.entries[path];
        match want {
            Expect::Dir => assert_eq!(got.kind, Some(NodeKind::Directory), "{what}: {path}"),
            Expect::File(bytes) => {
                assert_eq!(got.kind, Some(NodeKind::File), "{what}: {path}");
                assert_eq!(got.size, Some(bytes.len() as u64), "{what}: {path} size");
                assert!(
                    got.data.as_deref() == Some(bytes.as_slice()),
                    "{what}: {path} content differs"
                );
            }
            Expect::Link(target) => {
                assert_eq!(got.kind, Some(NodeKind::Symlink), "{what}: {path}");
                assert_eq!(
                    got.target.as_deref(),
                    Some(target.as_str()),
                    "{what}: {path}"
                );
            }
        }
    }
    assert_eq!(
        walked.entries["readme.txt"].xattrs,
        vec!["user.note".to_string()]
    );
}

// ---- partition maps -----------------------------------------------------------

/// First sector of the volume in the partitioned disks.
const VOLUME_START: usize = 64;

/// An Apple Partition Map disk: the map, the volume as `Apple_HFS`, then an
/// empty `Apple_Free` entry and some free space.
fn apm_disk(volume: &[u8]) -> Vec<u8> {
    let sectors = volume.len() / 512;
    let total = (VOLUME_START + sectors + 64) * 512;
    let mut d = vec![0u8; total];
    d[..2].copy_from_slice(b"ER");
    d[2..4].copy_from_slice(&512u16.to_be_bytes());
    let specs: [(u32, u32, &str, &str); 3] = [
        (1, 63, "Apple", "Apple_partition_map"),
        (
            VOLUME_START as u32,
            sectors as u32,
            "Macintosh HD",
            "Apple_HFS",
        ),
        ((VOLUME_START + sectors) as u32, 64, "", "Apple_Free"),
    ];
    for (i, (start, count, name, kind)) in specs.iter().enumerate() {
        let at = (i + 1) * 512;
        let e = &mut d[at..at + 80];
        e[..2].copy_from_slice(b"PM");
        e[4..8].copy_from_slice(&(specs.len() as u32).to_be_bytes());
        e[8..12].copy_from_slice(&start.to_be_bytes());
        e[12..16].copy_from_slice(&count.to_be_bytes());
        e[16..16 + name.len()].copy_from_slice(name.as_bytes());
        e[48..48 + kind.len()].copy_from_slice(kind.as_bytes());
    }
    d[VOLUME_START * 512..VOLUME_START * 512 + volume.len()].copy_from_slice(volume);
    d
}

/// A GPT disk: the volume as HFS+, then an empty EFI System partition.
fn gpt_disk(volume: &[u8]) -> Vec<u8> {
    let sectors = volume.len() / 512;
    let efi_first = VOLUME_START + sectors;
    let total = efi_first + 64 + 40;
    let mut d = gpt::testutil::build(
        512,
        total,
        &[
            gpt::testutil::Spec {
                type_guid: Guid::HFS_PLUS.0,
                first: VOLUME_START as u64,
                last: (VOLUME_START + sectors - 1) as u64,
                name: "Macintosh HD",
            },
            gpt::testutil::Spec {
                type_guid: Guid::EFI_SYSTEM.0,
                first: efi_first as u64,
                last: (efi_first + 63) as u64,
                name: "EFI",
            },
        ],
    );
    d[VOLUME_START * 512..VOLUME_START * 512 + volume.len()].copy_from_slice(volume);
    d
}

// ---- containers -----------------------------------------------------------

/// A UDIF file presenting `disk`: zero runs as zero chunks, the rest
/// alternately zlib and raw, 32 KiB (64 sectors) at a time.
fn udif_bytes(disk: &[u8]) -> Vec<u8> {
    assert_eq!(disk.len() % 512, 0);
    let sectors = (disk.len() / 512) as u64;
    let mut chunks = Vec::new();
    for (i, piece) in disk.chunks(64 * 512).enumerate() {
        let sector = (i * 64) as u64;
        if piece.iter().all(|b| *b == 0) {
            chunks.push(ChunkSpec::empty(0, sector, (piece.len() / 512) as u64));
        } else if i % 2 == 0 {
            chunks.push(ChunkSpec::zlib(sector, piece.to_vec()));
        } else {
            chunks.push(ChunkSpec::raw(sector, piece.to_vec()));
        }
    }
    chunks.push(ChunkSpec::empty(0xFFFF_FFFF, sectors, 0));
    let built = build_udif(&[Block { first: 0, chunks }], sectors);
    assert!(built.disk == disk, "the UDIF builder changed the disk");
    built.bytes
}

/// The file header and table of a sparse image with `sectors_per_band`.
const SPARSE_HEADER: usize = 4096;
const SPARSE_TABLE: usize = 64;

/// A sparse image file presenting `disk`: bands that are all zero are left
/// out, the rest are stored in reverse order.
fn sparse_image_bytes(disk: &[u8], sectors_per_band: u32) -> Vec<u8> {
    let band_bytes = sectors_per_band as usize * 512;
    let bands: Vec<usize> = (0..disk.len().div_ceil(band_bytes))
        .filter(|b| {
            let start = b * band_bytes;
            disk[start..(start + band_bytes).min(disk.len())]
                .iter()
                .any(|x| *x != 0)
        })
        .collect();
    assert!(bands.len() <= (SPARSE_HEADER - SPARSE_TABLE) / 4);
    let mut out = vec![0u8; SPARSE_HEADER];
    out[..4].copy_from_slice(b"sprs");
    out[4..8].copy_from_slice(&3u32.to_be_bytes());
    out[8..12].copy_from_slice(&sectors_per_band.to_be_bytes());
    out[12..16].copy_from_slice(&1u32.to_be_bytes());
    out[16..20].copy_from_slice(&((disk.len() / 512) as u32).to_be_bytes());
    for (slot, band) in bands.iter().rev().enumerate() {
        let at = SPARSE_TABLE + slot * 4;
        out[at..at + 4].copy_from_slice(&(*band as u32 + 1).to_be_bytes());
        let start = band * band_bytes;
        let end = (start + band_bytes).min(disk.len());
        out.extend_from_slice(&disk[start..end]);
        out.resize(SPARSE_HEADER + (slot + 1) * band_bytes, 0);
    }
    out
}

/// A sparse bundle folder presenting `disk`: Info.plist, and band files for
/// the bands that are not all zero. Every other band file has its trailing
/// zeros cut off, so the reader meets short band files.
fn write_sparse_bundle(root: &Path, disk: &[u8], band_size: usize) {
    fs::create_dir_all(root.join("bands")).unwrap();
    let xml = plist_xml(&[
        (
            "diskimage-bundle-type",
            "<string>com.apple.diskimage.sparsebundle</string>",
        ),
        ("bundle-backingstore-version", "<integer>1</integer>"),
        ("band-size", &format!("<integer>{band_size}</integer>")),
        ("size", &format!("<integer>{}</integer>", disk.len())),
    ]);
    fs::write(root.join("Info.plist"), xml).unwrap();
    for (i, band) in disk.chunks(band_size).enumerate() {
        if band.iter().all(|b| *b == 0) {
            continue;
        }
        let mut band = band;
        if i % 2 == 1 {
            while let [rest @ .., 0] = band {
                band = rest;
            }
        }
        fs::write(root.join("bands").join(format!("{i:x}")), band).unwrap();
    }
}

/// How a disk is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Container {
    Raw,
    Udif,
    SparseImage,
    SparseBundle,
    /// A DiskCopy 4.2 floppy image. Not in [`Container::ALL`]: it holds the
    /// classic HFS volume only, and sits in its own tests.
    DiskCopy42,
}

impl Container {
    /// The containers every volume is stored in.
    const ALL: [Self; 4] = [Self::Raw, Self::Udif, Self::SparseImage, Self::SparseBundle];

    fn format(self) -> ImageFormat {
        match self {
            Self::Raw => ImageFormat::Raw,
            Self::Udif => ImageFormat::Udif,
            Self::SparseImage => ImageFormat::SparseImage,
            Self::SparseBundle => ImageFormat::SparseBundle,
            Self::DiskCopy42 => ImageFormat::DiskCopy42,
        }
    }

    /// Writes `disk` in this container to a temporary path.
    fn write(self, disk: &[u8]) -> Temp {
        match self {
            Self::Raw => Temp::file("raw", disk),
            Self::Udif => Temp::file("udif", &udif_bytes(disk)),
            Self::SparseImage => Temp::file("sparseimage", &sparse_image_bytes(disk, 16)),
            Self::SparseBundle => {
                let temp = Temp::new("sparsebundle");
                write_sparse_bundle(&temp.0, disk, 8192);
                temp
            }
            Self::DiskCopy42 => Temp::file("dc42", &dc42::fixtures::build(b"Atlas HD", disk, &[])),
        }
    }
}

// ---- the stack ------------------------------------------------------------

/// Opens `path`, reads the partition map (or takes the whole disk), probes
/// each partition, mounts the HFS+ ones and returns them with the partition
/// numbers. Checks the layers on the way.
fn mount_image(path: &Path, format: ImageFormat) -> Vec<(String, HfsPlus)> {
    let opened = image::open(path).unwrap();
    assert_eq!(opened.format, format);
    let source = opened.source;
    let mut mounted = Vec::new();
    let mut mount_one = |label: String, volume: Arc<dyn BlockSource>| {
        let content = probe::probe(volume.as_ref()).unwrap();
        if content != Content::HfsPlus {
            // Not a volume: the probe must also say there is nothing to mount.
            assert!(
                probe::mount(volume, content).unwrap().is_none(),
                "{label}: {content:?}"
            );
            return;
        }
        // The registry path and the direct one agree.
        let via_probe = probe::mount(Arc::clone(&volume), content)
            .unwrap()
            .expect("HFS+ has a reader")
            .into_volume()
            .expect("HFS+ mounts as one volume");
        let fs = hfsplus::mount(volume).unwrap();
        assert_eq!(via_probe.info().name, fs.info().name);
        mounted.push((label, fs));
    };
    match partition::read(source.as_ref()).unwrap() {
        Some(map) => {
            assert!(map.damage.is_empty(), "{:?}", map.damage);
            for p in map
                .partitions
                .iter()
                .filter(|p| p.hint != TypeHint::NotData)
            {
                let slice = p.slice(Arc::clone(&source)).unwrap();
                mount_one(format!("partition {}", p.number), Arc::new(slice));
            }
        }
        None => mount_one("whole disk".to_string(), source),
    }
    mounted
}

fn check_stack(disk: &[u8], expect: &BTreeMap<String, Expect>, what: &str) {
    for container in Container::ALL {
        let temp = container.write(disk);
        let mounted = mount_image(&temp.0, container.format());
        assert_eq!(mounted.len(), 1, "{what} in {container:?}");
        let (label, fs) = &mounted[0];
        let what = format!("{what} in {container:?}, {label}");
        assert_eq!(fs.info().name, VOLUME_NAME, "{what}");
        assert_eq!(fs.journal(), &JournalState::Clean, "{what}");
        assert_tree(&walk(fs, Limits::clean()), expect, &what);
        // The same walk twice: the caches change nothing.
        assert_tree(&walk(fs, Limits::clean()), expect, &what);
    }
}

#[test]
fn apm_disk_in_every_container() {
    let f = fixture();
    check_stack(&apm_disk(&f.volume), &f.expect, "APM");
}

#[test]
fn gpt_disk_in_every_container() {
    let f = fixture();
    check_stack(&gpt_disk(&f.volume), &f.expect, "GPT");
}

#[test]
fn unpartitioned_volume_in_a_udif() {
    let f = fixture();
    let temp = Container::Udif.write(&f.volume);
    let opened = image::open(&temp.0).unwrap();
    assert!(partition::read(opened.source.as_ref()).unwrap().is_none());
    let mounted = mount_image(&temp.0, ImageFormat::Udif);
    assert_eq!(mounted.len(), 1);
    assert_eq!(mounted[0].0, "whole disk");
    assert_tree(&walk(&mounted[0].1, Limits::clean()), &f.expect, "UDIF");
}

#[test]
fn the_partition_maps_list_what_was_built() {
    let f = fixture();
    for (disk, scheme, hfs_name) in [
        (apm_disk(&f.volume), partition::Scheme::Apm, "Macintosh HD"),
        (gpt_disk(&f.volume), partition::Scheme::Gpt, "Macintosh HD"),
    ] {
        let source = MemSource::new(disk);
        let map = partition::read(&source).unwrap().unwrap();
        assert_eq!(map.scheme, scheme);
        let hfs = map
            .partitions
            .iter()
            .find(|p| p.hint == TypeHint::Hfs)
            .unwrap();
        assert_eq!(hfs.name.as_deref(), Some(hfs_name));
        assert_eq!(hfs.start, VOLUME_START as u64 * 512);
        assert_eq!(hfs.len, f.volume.len() as u64);
    }
}

#[test]
fn container_sizes_match_the_disk() {
    let f = fixture();
    let disk = apm_disk(&f.volume);
    for container in Container::ALL {
        let temp = container.write(&disk);
        let opened = image::open(&temp.0).unwrap();
        assert_eq!(opened.source.len(), disk.len() as u64, "{container:?}");
        let mut got = vec![0u8; disk.len()];
        opened.source.read_at(0, &mut got).unwrap();
        assert!(got == disk, "{container:?} presents different bytes");
    }
}

/// Prints a line of sweep results, shown with `--nocapture`, and the
/// damage in front of a panic.
#[allow(clippy::print_stderr)]
fn report(line: std::fmt::Arguments<'_>) {
    eprintln!("{line}");
}

// ---- damage sweeps ----------------------------------------------------------

/// A fixed-seed xorshift generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// One overwrite: where, how many bytes, with what.
#[derive(Debug, Clone, Copy)]
struct Damage {
    at: usize,
    len: usize,
    mode: u8,
    seed: u64,
}

impl Damage {
    fn random(rng: &mut Rng, at: usize, size: usize) -> Self {
        Self {
            at,
            len: (1 + rng.below(8)).min(size.saturating_sub(at).max(1)),
            mode: rng.below(6) as u8,
            seed: rng.next(),
        }
    }

    /// Overwrites the bytes. Modes: zeros, 0xFF, 0x80, 0x7F, flipped bits
    /// and random values.
    fn apply(&self, bytes: &mut [u8]) {
        let mut rng = Rng(self.seed | 1);
        for i in 0..self.len {
            let Some(b) = bytes.get_mut(self.at + i) else {
                return;
            };
            *b = match self.mode {
                0 => 0x00,
                1 => 0xFF,
                2 => 0x80,
                3 => 0x7F,
                4 => *b ^ (1 << rng.below(8)),
                _ => rng.next() as u8,
            };
        }
    }
}

/// Prints the damage when a panic unwinds through the check, so the failing
/// input can be rebuilt.
struct Context<'a>(&'a str, Damage);

impl Drop for Context<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            report(format_args!(
                "panic while checking {}: {:?}",
                self.0, self.1
            ));
        }
    }
}

/// Positions to damage in the bytes `start..end` of an image of `size`
/// bytes: one random spot in every `step`th 512-byte sector, then `extra`
/// more random ones.
fn damage_plan_in(
    (start, end): (usize, usize),
    size: usize,
    step: usize,
    extra: usize,
    seed: u64,
) -> Vec<Damage> {
    let mut rng = Rng(seed);
    let mut plan = Vec::new();
    for sector in (start / 512..end.div_ceil(512)).step_by(step) {
        let at = (sector * 512 + rng.below(512)).min(end.saturating_sub(1));
        plan.push(Damage::random(&mut rng, at, size));
    }
    for _ in 0..extra {
        let at = start + rng.below(end - start);
        plan.push(Damage::random(&mut rng, at, size));
    }
    plan
}

/// Positions to damage anywhere in an image of `size` bytes: one random
/// spot in every sector, then `extra` more.
fn damage_plan(size: usize, extra: usize, seed: u64) -> Vec<Damage> {
    damage_plan_in((0, size), size, 1, extra, seed)
}

/// The first byte of the volume's catalog file. Everything from there to the
/// end of the image is B-tree and allocation metadata, and the two blocks
/// before the file data hold the header and the extents tree.
fn metadata_start(volume: &[u8]) -> usize {
    // The catalog fork data is at 272 in the volume header (byte 1024); its
    // first extent starts 16 bytes in.
    let at = 1024 + 272 + 16;
    let block = u32::from_be_bytes(volume[at..at + 4].try_into().unwrap());
    block as usize * hfsplus::test_volume::BLOCK
}

/// Mounts `bytes` as a volume and, if that works, walks it.
fn mount_and_walk(bytes: &[u8]) -> Option<Walked> {
    let source: Arc<dyn BlockSource> = Arc::new(MemSource::new(bytes));
    let fs = hfsplus::mount(source).ok()?;
    let _ = fs.info();
    let _ = fs.journal();
    let _ = fs.allocation_map(64);
    Some(walk(&fs, Limits::damaged()))
}

#[test]
fn damaged_volumes_never_panic() {
    let started = Instant::now();
    let f = fixture();
    let (mut mounted, mut walked_entries, mut with_errors) = (0usize, 0usize, 0usize);
    // Every sector of the header, the extents tree and the catalog, the
    // attributes and the allocation file; every eighth sector of the file
    // data; and extra spots in the metadata, where the interesting
    // failures are.
    let size = f.volume.len();
    let meta = metadata_start(&f.volume);
    assert!(meta > 8192 && meta < size);
    let mut plan = damage_plan_in((0, 8192), size, 1, 100, 0x5EED_0001);
    plan.extend(damage_plan_in((8192, meta), size, 8, 0, 0x5EED_0011));
    plan.extend(damage_plan_in((meta, size), size, 1, 700, 0x5EED_0012));
    for damage in &plan {
        let _ctx = Context("volume overwrite", *damage);
        let mut bytes = f.volume.clone();
        damage.apply(&mut bytes);
        if let Some(walked) = mount_and_walk(&bytes) {
            mounted += 1;
            walked_entries += walked.entries.len();
            with_errors += usize::from(walked.errors > 0);
        }
    }
    report(format_args!(
        "volume overwrite sweep: {} cases, {mounted} mounted, {with_errors} walks met errors, {walked_entries} entries walked, {:?}",
        plan.len(),
        started.elapsed()
    ));
    // Many overwrites land in file content or free space and change nothing
    // that matters; the sweep would prove little if none mounted.
    assert!(mounted > plan.len() / 4);
}

#[test]
fn truncated_volumes_never_panic() {
    let started = Instant::now();
    let f = fixture();
    let size = f.volume.len();
    let mut rng = Rng(0x5EED_0002);
    let mut lengths: Vec<usize> = (0..size).step_by(512).collect();
    lengths.extend([
        1,
        511,
        1023,
        1024,
        1025,
        1535,
        size - 513,
        size - 512,
        size - 1,
    ]);
    for _ in 0..200 {
        lengths.push(rng.below(size));
    }
    for len in &lengths {
        let _ctx = Context(
            "volume truncation",
            Damage {
                at: *len,
                len: 0,
                mode: 0,
                seed: 0,
            },
        );
        let _ = mount_and_walk(&f.volume[..*len]);
    }
    report(format_args!(
        "volume truncation sweep: {} cases, {:?}",
        lengths.len(),
        started.elapsed()
    ));
}

/// Reads the whole of `source` (bounded) in steps that straddle chunk and
/// band boundaries, and the last byte.
fn read_disk(source: &dyn BlockSource) {
    let _ = read_up_to(source, 4 << 20, false);
    if !source.is_empty() {
        let mut one = [0u8; 1];
        let _ = source.read_at(source.len() - 1, &mut one);
        let _ = source.read_bulk(0, &mut one);
    }
}

#[test]
fn damaged_udif_never_panics() {
    let started = Instant::now();
    let f = fixture();
    let disk = apm_disk(&f.volume);
    let bytes = udif_bytes(&disk);
    let (mut opened, mut full) = (0usize, 0usize);
    let plan = damage_plan(bytes.len(), 600, 0x5EED_0003);
    for damage in &plan {
        let _ctx = Context("UDIF overwrite", *damage);
        let mut bad = bytes.clone();
        damage.apply(&mut bad);
        let file: Arc<dyn BlockSource> = Arc::new(MemSource::new(bad));
        if let Ok(source) = UdifSource::new(file) {
            opened += 1;
            read_disk(&source);
            if source.len() == disk.len() as u64 {
                full += 1;
            }
        }
    }
    // The trailer and the property list are the metadata: damage there is
    // where the interesting failures are, and the sweep above hits them too.
    // Truncations cut the trailer off.
    let mut rng = Rng(0x5EED_0004);
    let mut lengths: Vec<usize> = (0..bytes.len()).step_by(4096).collect();
    lengths.extend([0, 1, 511, 512, bytes.len() - 512, bytes.len() - 1]);
    for _ in 0..100 {
        lengths.push(rng.below(bytes.len()));
    }
    for len in &lengths {
        let _ctx = Context(
            "UDIF truncation",
            Damage {
                at: *len,
                len: 0,
                mode: 0,
                seed: 0,
            },
        );
        let file: Arc<dyn BlockSource> = Arc::new(MemSource::new(&bytes[..*len]));
        if let Ok(source) = UdifSource::new(file) {
            read_disk(&source);
        }
    }
    report(format_args!(
        "UDIF sweep: {} overwrites ({opened} opened, {full} full size) and {} truncations, {:?}",
        plan.len(),
        lengths.len(),
        started.elapsed()
    ));
}

#[test]
fn damaged_sparse_image_never_panics() {
    let started = Instant::now();
    let f = fixture();
    let disk = gpt_disk(&f.volume);
    let bytes = sparse_image_bytes(&disk, 16);
    let mut opened = 0usize;
    // The header and table are the metadata: every sector of them, and spots
    // in the bands.
    let mut plan = damage_plan(SPARSE_HEADER * 2, 0, 0x5EED_0005);
    plan.extend(damage_plan(bytes.len(), 600, 0x5EED_0006));
    for damage in &plan {
        let _ctx = Context("sparse image overwrite", *damage);
        let mut bad = bytes.clone();
        damage.apply(&mut bad);
        let file: Arc<dyn BlockSource> = Arc::new(MemSource::new(bad));
        if let Ok(source) = SparseImageSource::new(file) {
            opened += 1;
            read_disk(&source);
        }
    }
    let mut lengths: Vec<usize> = (0..bytes.len()).step_by(8192).collect();
    lengths.extend([0, 1, 63, 64, 4095, 4096, 4097, bytes.len() - 1]);
    for len in &lengths {
        let _ctx = Context(
            "sparse image truncation",
            Damage {
                at: *len,
                len: 0,
                mode: 0,
                seed: 0,
            },
        );
        let file: Arc<dyn BlockSource> = Arc::new(MemSource::new(&bytes[..*len]));
        if let Ok(source) = SparseImageSource::new(file) {
            read_disk(&source);
        }
    }
    report(format_args!(
        "sparse image sweep: {} overwrites ({opened} opened) and {} truncations, {:?}",
        plan.len(),
        lengths.len(),
        started.elapsed()
    ));
}

#[test]
fn damaged_partition_maps_never_panic() {
    let f = fixture();
    for disk in [apm_disk(&f.volume), gpt_disk(&f.volume)] {
        // Only the start and end of the disk hold the map.
        let head = 40 * 512;
        let mut plan = damage_plan(head, 300, 0x5EED_0007);
        let tail_start = disk.len() - head;
        plan.extend(
            damage_plan(head, 300, 0x5EED_0008)
                .into_iter()
                .map(|d| Damage {
                    at: d.at + tail_start,
                    ..d
                }),
        );
        for damage in &plan {
            let _ctx = Context("partition map overwrite", *damage);
            let mut bad = disk.clone();
            damage.apply(&mut bad);
            let source: Arc<dyn BlockSource> = Arc::new(MemSource::new(bad));
            if let Ok(Some(map)) = partition::read(source.as_ref()) {
                for p in &map.partitions {
                    if let Ok(slice) = p.slice(Arc::clone(&source)) {
                        let slice: Arc<dyn BlockSource> = Arc::new(slice);
                        if let Ok(content) = probe::probe(slice.as_ref())
                            && let Ok(Some(probe::Mount::Volume(fs))) = probe::mount(slice, content)
                        {
                            let _ = fs.read_dir(fs.root());
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn the_walk_stops_at_its_bounds() {
    let f = fixture();
    let source: Arc<dyn BlockSource> = Arc::new(MemSource::new(f.volume.clone()));
    let fs = hfsplus::mount(source).unwrap();
    let few = walk(
        &fs,
        Limits {
            entries: 3,
            ..Limits::clean()
        },
    );
    assert!(few.bound_hit);
    assert_eq!(few.entries.len(), 3);
    let shallow = walk(
        &fs,
        Limits {
            depth: 2,
            ..Limits::clean()
        },
    );
    assert!(shallow.bound_hit);
    assert!(!shallow.entries.contains_key("Docs/Sub/big.bin"));
    assert!(shallow.entries.contains_key("Docs/notes.txt"));
    assert_eq!(fs.root(), NodeId(2));
}

// ---- classic HFS ------------------------------------------------------------

/// Opens `path`, reads the partition map (or takes the whole disk), probes
/// each partition, mounts the classic HFS ones and returns them with their
/// labels. Checks the layers on the way, as [`mount_image`] does.
fn mount_classic(path: &Path, format: ImageFormat) -> Vec<(String, Arc<dyn FileSystem>)> {
    let opened = image::open(path).unwrap();
    assert_eq!(opened.format, format);
    let source = opened.source;
    let mut mounted = Vec::new();
    let mut mount_one = |label: String, volume: Arc<dyn BlockSource>| {
        let content = probe::probe(volume.as_ref()).unwrap();
        if content != Content::Hfs {
            assert!(
                probe::mount(volume, content).unwrap().is_none(),
                "{label}: {content:?}"
            );
            return;
        }
        // The registry path and the direct one agree.
        let via_probe = probe::mount(Arc::clone(&volume), content)
            .unwrap()
            .expect("classic HFS has a reader")
            .into_volume()
            .expect("classic HFS mounts as one volume");
        let direct = hfs::mount(volume).unwrap();
        assert_eq!(via_probe.info().name, direct.info().name);
        mounted.push((label, via_probe));
    };
    match partition::read(source.as_ref()).unwrap() {
        Some(map) => {
            assert!(map.damage.is_empty(), "{:?}", map.damage);
            for p in map
                .partitions
                .iter()
                .filter(|p| p.hint != TypeHint::NotData)
            {
                let slice = p.slice(Arc::clone(&source)).unwrap();
                mount_one(format!("partition {}", p.number), Arc::new(slice));
            }
        }
        None => mount_one("whole disk".to_string(), source),
    }
    mounted
}

/// Checks that `walked` is exactly the manifest's tree, with both forks of
/// every file as the builder wrote them.
fn assert_classic_tree(walked: &Walked, manifest: &Manifest, what: &str) {
    assert_eq!(walked.errors, 0, "{what}: the walk met errors");
    assert!(!walked.bound_hit, "{what}: a bound ended the walk");
    let found: Vec<&str> = walked.entries.keys().map(String::as_str).collect();
    let mut wanted: Vec<&str> = manifest
        .entries
        .iter()
        .map(|e| e.path.trim_start_matches('/'))
        .collect();
    wanted.sort_unstable();
    assert_eq!(found, wanted, "{what}: the tree differs");
    for entry in &manifest.entries {
        let path = entry.path.trim_start_matches('/');
        let got = &walked.entries[path];
        if entry.is_dir {
            assert_eq!(got.kind, Some(NodeKind::Directory), "{what}: {path}");
            continue;
        }
        assert_eq!(got.kind, Some(NodeKind::File), "{what}: {path}");
        assert_eq!(got.size, Some(entry.data_len), "{what}: {path} size");
        assert!(
            got.data.as_deref() == Some(entry.data_bytes().as_slice()),
            "{what}: {path} data fork differs"
        );
        if entry.rsrc_len == 0 {
            assert!(got.rsrc.is_none(), "{what}: {path} has a resource fork");
        } else {
            assert!(
                got.rsrc.as_deref() == Some(entry.rsrc_bytes().as_slice()),
                "{what}: {path} resource fork differs"
            );
        }
    }
}

/// Writes `disk` in each of `containers`, mounts the classic HFS volume in
/// it and compares the walk, twice, with the manifest.
fn check_classic_stack(disk: &[u8], built: &Built, containers: &[Container], what: &str) {
    for container in containers {
        let temp = container.write(disk);
        let mounted = mount_classic(&temp.0, container.format());
        assert_eq!(mounted.len(), 1, "{what} in {container:?}");
        let (label, fs) = &mounted[0];
        let what = format!("{what} in {container:?}, {label}");
        assert_eq!(fs.info().name, built.manifest.volume_name, "{what}");
        let limits = Limits::clean().with_forks();
        assert_classic_tree(&walk(fs.as_ref(), limits), &built.manifest, &what);
        // The same walk twice: the caches change nothing.
        assert_classic_tree(&walk(fs.as_ref(), limits), &built.manifest, &what);
    }
}

#[test]
fn classic_hfs_bare_in_every_container() {
    let built = standard_volume();
    check_classic_stack(&built.image, &built, &Container::ALL, "bare volume");
}

#[test]
fn classic_hfs_in_an_apm_disk_in_every_container() {
    let built = standard_volume();
    let disk = apm_disk(&built.image);
    // The partition is found, typed and sized as built.
    let map = partition::read(&MemSource::new(disk.clone()))
        .unwrap()
        .unwrap();
    assert_eq!(map.scheme, partition::Scheme::Apm);
    let hfs_part = map
        .partitions
        .iter()
        .find(|p| p.hint == TypeHint::Hfs)
        .unwrap();
    assert_eq!(hfs_part.start, VOLUME_START as u64 * 512);
    assert_eq!(hfs_part.len, built.image.len() as u64);
    check_classic_stack(&disk, &built, &Container::ALL, "APM");
}

#[test]
fn classic_hfs_in_a_diskcopy_42_image() {
    let built = standard_volume();
    let temp = Container::DiskCopy42.write(&built.image);
    let opened = image::open(&temp.0).unwrap();
    assert_eq!(opened.format, ImageFormat::DiskCopy42);
    // The wrapper presents exactly the volume, with no partition map.
    assert_eq!(opened.source.len(), built.image.len() as u64);
    let mut got = vec![0u8; built.image.len()];
    opened.source.read_at(0, &mut got).unwrap();
    assert!(
        got == built.image,
        "the DiskCopy 4.2 image changed the volume"
    );
    assert!(partition::read(opened.source.as_ref()).unwrap().is_none());
    check_classic_stack(
        &built.image,
        &built,
        &[Container::DiskCopy42],
        "DiskCopy 4.2",
    );
}

/// Mounts `bytes` as a classic HFS volume and, if that works, exercises it
/// and walks it, both forks included.
fn mount_and_walk_classic(bytes: &[u8]) -> Option<Walked> {
    let source: Arc<dyn BlockSource> = Arc::new(MemSource::new(bytes));
    let fs = hfs::mount(source).ok()?;
    let _ = fs.info();
    let _ = fs.identity();
    let _ = fs.allocation_map(64);
    let mut seen = 0usize;
    let _ = fs.scan(&mut |_| {
        seen += 1;
        if seen >= 2000 {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    });
    Some(walk(&fs, Limits::damaged().with_forks()))
}

/// The byte range a fork-extent list covers in the volume image.
fn extent_span(manifest: &Manifest, extents: &[(u32, u32)]) -> (usize, usize) {
    let block = u64::from(manifest.block_size);
    let first = extents.iter().map(|e| e.0).min().unwrap();
    let end = extents.iter().map(|e| e.0 + e.1).max().unwrap();
    let at = |n: u32| (manifest.alloc_start + u64::from(n) * block) as usize;
    (at(first), at(end))
}

#[test]
fn damaged_classic_volumes_never_panic() {
    let started = Instant::now();
    let built = standard_volume();
    let m = &built.manifest;
    let size = built.image.len();
    let head = m.alloc_start as usize;
    let catalog = extent_span(m, &m.catalog_extents);
    let overflow = extent_span(m, &m.extents_extents);
    assert!(head > 1536 && catalog.1 <= size && overflow.1 <= size);
    // Every sector of the boot blocks, the master directory block and the
    // bitmap; every sector of the catalog and the overflow tree, with extra
    // spots in each; every eighth sector of the file data; and the
    // alternate master directory block at the end.
    let mut plan = damage_plan_in((0, head), size, 1, 100, 0x5EED_0101);
    plan.extend(damage_plan_in(catalog, size, 1, 600, 0x5EED_0102));
    plan.extend(damage_plan_in(overflow, size, 1, 50, 0x5EED_0103));
    plan.extend(damage_plan_in((head, size), size, 8, 0, 0x5EED_0104));
    plan.extend(damage_plan_in(
        (size - 1024, size),
        size,
        1,
        50,
        0x5EED_0105,
    ));
    let (mut mounted, mut walked_entries, mut with_errors) = (0usize, 0usize, 0usize);
    for damage in &plan {
        let _ctx = Context("classic HFS overwrite", *damage);
        let mut bytes = built.image.clone();
        damage.apply(&mut bytes);
        if let Some(walked) = mount_and_walk_classic(&bytes) {
            mounted += 1;
            walked_entries += walked.entries.len();
            with_errors += usize::from(walked.errors > 0);
        }
    }
    report(format_args!(
        "classic HFS overwrite sweep: {} cases, {mounted} mounted, {with_errors} walks met errors, {walked_entries} entries walked, {:?}",
        plan.len(),
        started.elapsed()
    ));
    // Many overwrites land in file content or free space and change nothing
    // that matters; the sweep would prove little if none mounted.
    assert!(mounted > plan.len() / 4);
}

#[test]
fn truncated_classic_volumes_never_panic() {
    let started = Instant::now();
    let built = standard_volume();
    let size = built.image.len();
    let mut rng = Rng(0x5EED_0106);
    let mut lengths: Vec<usize> = (0..size).step_by(512).collect();
    lengths.extend([
        1,
        511,
        1023,
        1024,
        1025,
        1535,
        size - 1025,
        size - 513,
        size - 512,
        size - 1,
    ]);
    for _ in 0..200 {
        lengths.push(rng.below(size));
    }
    for len in &lengths {
        let _ctx = Context(
            "classic HFS truncation",
            Damage {
                at: *len,
                len: 0,
                mode: 0,
                seed: 0,
            },
        );
        let _ = mount_and_walk_classic(&built.image[..*len]);
    }
    report(format_args!(
        "classic HFS truncation sweep: {} cases, {:?}",
        lengths.len(),
        started.elapsed()
    ));
}

/// Opens `bytes` as a DiskCopy 4.2 file and, if that works, reads the disk
/// and mounts what it presents. Returns whether the wrapper opened.
fn open_diskcopy_and_mount(bytes: &[u8]) -> bool {
    let file: Arc<dyn BlockSource> = Arc::new(MemSource::new(bytes));
    let Ok((_header, disk)) = dc42::open_source(file) else {
        return false;
    };
    let _ = dc42::checksum(&disk, 0, disk.len());
    read_disk(&disk);
    let disk: Arc<dyn BlockSource> = Arc::new(disk);
    if let Ok(content) = probe::probe(disk.as_ref())
        && let Ok(Some(probe::Mount::Volume(fs))) = probe::mount(disk, content)
    {
        let _ = fs.info();
        let _ = fs.read_dir(fs.root());
    }
    true
}

#[test]
fn damaged_diskcopy_42_never_panics() {
    let started = Instant::now();
    let built = standard_volume();
    let bytes = dc42::fixtures::build(b"Atlas HD", &built.image, &[]);
    assert!(open_diskcopy_and_mount(&bytes));
    let mut opened = 0usize;
    // The 84-byte header is the metadata: a lot of spots in the first
    // sector, then one in every sector of the file and some more.
    let mut plan = damage_plan_in((0, 512), bytes.len(), 1, 300, 0x5EED_0107);
    plan.extend(damage_plan(bytes.len(), 200, 0x5EED_0108));
    for damage in &plan {
        let _ctx = Context("DiskCopy 4.2 overwrite", *damage);
        let mut bad = bytes.clone();
        damage.apply(&mut bad);
        opened += usize::from(open_diskcopy_and_mount(&bad));
    }
    let mut rng = Rng(0x5EED_0109);
    let mut lengths: Vec<usize> = (0..bytes.len()).step_by(4096).collect();
    lengths.extend([
        0,
        1,
        83,
        84,
        85,
        511,
        512,
        bytes.len() - 512,
        bytes.len() - 1,
    ]);
    for _ in 0..100 {
        lengths.push(rng.below(bytes.len()));
    }
    for len in &lengths {
        let _ctx = Context(
            "DiskCopy 4.2 truncation",
            Damage {
                at: *len,
                len: 0,
                mode: 0,
                seed: 0,
            },
        );
        open_diskcopy_and_mount(&bytes[..*len]);
    }
    report(format_args!(
        "DiskCopy 4.2 sweep: {} overwrites ({opened} opened) and {} truncations, {:?}",
        plan.len(),
        lengths.len(),
        started.elapsed()
    ));
    // Most overwrites miss the few header bytes the reader checks.
    assert!(opened > plan.len() / 2);
}

/// Writes the test disks in every container into the folder named by
/// `DISK_ATLAS_E2E_OUT`, for trying `atlas_check` by hand. Does nothing when
/// the variable is unset. Run it with
/// `cargo test -p disk-atlas-lib --lib write_sample_images -- --ignored`.
#[test]
#[ignore = "writes files; run by hand"]
fn write_sample_images() {
    let Some(out) = std::env::var_os("DISK_ATLAS_E2E_OUT") else {
        return;
    };
    let out = Path::new(&out);
    fs::create_dir_all(out).unwrap();
    let f = fixture();
    for (name, disk) in [("apm", apm_disk(&f.volume)), ("gpt", gpt_disk(&f.volume))] {
        fs::write(out.join(format!("{name}.img")), &disk).unwrap();
        fs::write(out.join(format!("{name}.dmg")), udif_bytes(&disk)).unwrap();
        fs::write(
            out.join(format!("{name}.sparseimage")),
            sparse_image_bytes(&disk, 16),
        )
        .unwrap();
        write_sparse_bundle(&out.join(format!("{name}.sparsebundle")), &disk, 8192);
    }
}
