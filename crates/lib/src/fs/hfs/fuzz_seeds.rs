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

//! Writes seed inputs for the classic HFS fuzz targets in `fuzz/`.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use super::catalog::{
    CatalogKey, CatalogRecord, FileRecord, FolderRecord, ROOT_FOLDER_ID, ThreadKind, ThreadRecord,
    encode_key,
};
use super::extents::{
    EXTENT_RECORD_LEN, ExtentKey, HfsExtent, HfsExtentRecord, parse_extent_record,
    write_extent_record,
};
use super::mdb::{MDB_LEN, MDB_OFFSET, Mdb};
use super::test_volume::{self, standard, standard_volume};
use crate::fs::FileSystem;
use crate::image::dc42;
use crate::io::block::{BlockSource, MemSource};

/// The largest input the fuzz run accepts (`-max_len=1048576`).
const MAX_SEED: usize = 1 << 20;

/// Volumes for the mount target: the standard one, one with 1024-byte
/// allocation blocks, one without the root thread and one whose catalog is
/// in nine pieces (so six of its extents are overflow records).
fn volumes() -> Vec<(&'static str, Vec<u8>)> {
    let mut big_blocks = standard();
    big_blocks.block_size(1024);
    let mut no_thread = standard();
    no_thread.without_root_thread();
    let mut fragmented = standard();
    fragmented.catalog_fragments(9);
    vec![
        ("hfs-standard", standard_volume().image),
        ("hfs-1k-blocks", big_blocks.build()),
        ("hfs-no-root-thread", no_thread.build()),
        ("hfs-fragmented-catalog", fragmented.build()),
    ]
}

/// The raw-record seeds, named. The target feeds each input to every parser
/// from byte 0, so each seed is the bytes one parser reads.
fn records() -> Vec<(&'static str, Vec<u8>)> {
    let image = standard_volume().image;
    let at = usize::try_from(MDB_OFFSET).unwrap();
    let mdb = image[at..at + MDB_LEN].to_vec();

    let folder = FolderRecord {
        flags: 0,
        valence: 3,
        folder_id: 16,
        created: test_volume::CREATED,
        modified: test_volume::MODIFIED,
        backed_up: 0,
        finder_info: [0x11; 16],
        ext_finder_info: [0x22; 16],
        reserved: [0; 16],
    };
    let file = FileRecord {
        flags: 0,
        file_kind: 0,
        finder_info: *b"TEXTttxt\0\0\0\0\0\0\0\0",
        file_id: 22,
        data_start_block: 7,
        data_logical: 1200,
        data_physical: 1536,
        rsrc_start_block: 11,
        rsrc_logical: 300,
        rsrc_physical: 512,
        created: test_volume::CREATED,
        modified: test_volume::MODIFIED,
        backed_up: 0,
        ext_finder_info: [0; 16],
        clump_size: 0,
        data_extents: extent_record(),
        rsrc_extents: [HfsExtent::default(); 3],
        reserved: 0,
    };
    let thread = ThreadRecord {
        kind: ThreadKind::Folder,
        reserved: [0; 8],
        parent_id: ROOT_FOLDER_ID,
        name: b"Documents".to_vec(),
    };
    let mut ext_record = [0u8; EXTENT_RECORD_LEN];
    write_extent_record(&extent_record(), &mut ext_record).unwrap();
    let ext_key = ExtentKey {
        fork_type: 0,
        file_id: 25,
        start_block: 3,
    };
    vec![
        ("hfs-mdb", mdb),
        ("hfs-folder-record", CatalogRecord::Folder(folder).encode()),
        ("hfs-file-record", CatalogRecord::File(file).encode()),
        (
            "hfs-thread-record",
            CatalogRecord::FolderThread(thread).encode(),
        ),
        ("hfs-extent-key", ext_key.encode()[1..].to_vec()),
        ("hfs-extent-record", ext_record.to_vec()),
        (
            "hfs-catalog-key",
            encode_key(ROOT_FOLDER_ID, b"Documents")[1..].to_vec(),
        ),
    ]
}

fn extent_record() -> HfsExtentRecord {
    [
        HfsExtent {
            start_block: 7,
            block_count: 3,
        },
        HfsExtent {
            start_block: 11,
            block_count: 1,
        },
        HfsExtent::default(),
    ]
}

/// Writes seed files for the classic HFS targets in `fuzz/` into the
/// directory named by `DISK_ATLAS_FUZZ_SEEDS`, one subfolder per target.
/// Does nothing when the variable is unset. The `dc42` and `probe` seeds
/// carry an `hfs-` prefix so the other seeds in those folders stay. Run it
/// with `cargo test -p disk-atlas-lib --lib write_fuzz_seeds -- --ignored`.
#[test]
#[ignore = "writes files; run by the fuzz CI job"]
fn write_fuzz_seeds() {
    let Some(root) = std::env::var_os("DISK_ATLAS_FUZZ_SEEDS") else {
        return;
    };
    let root = Path::new(&root);
    let mount_dir = root.join("hfs_mount");
    let records_dir = root.join("hfs_records");
    let dc42_dir = root.join("dc42");
    let probe_dir = root.join("probe");
    for dir in [&mount_dir, &records_dir, &dc42_dir, &probe_dir] {
        fs::create_dir_all(dir).expect("create seed directory");
    }
    for (name, image) in volumes() {
        fs::write(mount_dir.join(name), &image).expect("write mount seed");
    }
    for (name, bytes) in records() {
        fs::write(records_dir.join(name), bytes).expect("write record seed");
    }
    let image = standard_volume().image;
    let wrapped = dc42::fixtures::build(b"Atlas HD", &image, &[]);
    fs::write(dc42_dir.join("hfs-standard.dc42"), wrapped).expect("write dc42 seed");
    // The probe reads only the first 64 KiB.
    let head = &image[..image.len().min(crate::fs::probe::PROBE_BYTES)];
    fs::write(probe_dir.join("hfs-standard"), head).expect("write probe seed");
}

/// Every mount seed fits the run's input limit, mounts and lists its root,
/// so the target starts on the valid path.
#[test]
fn mount_seeds_are_valid_and_small() {
    for (name, image) in volumes() {
        assert!(image.len() <= MAX_SEED, "{name}: {} bytes", image.len());
        let volume =
            super::mount(Arc::new(MemSource::new(image))).unwrap_or_else(|e| panic!("{name}: {e}"));
        // Without a root thread the root cannot be listed, only described.
        if name == "hfs-no-root-thread" {
            assert!(volume.metadata(volume.root()).is_ok(), "{name}");
        } else {
            assert!(volume.read_dir(volume.root()).is_ok(), "{name}");
        }
    }
}

/// Every record seed parses with the parser it is meant for and survives the
/// round trip the target asserts.
#[test]
fn record_seeds_parse() {
    for (name, bytes) in records() {
        match name {
            "hfs-mdb" => {
                let mdb = Mdb::parse(&bytes).expect(name);
                mdb.validate(1024, standard_volume().image.len() as u64)
                    .expect(name);
            }
            "hfs-extent-key" => assert!(ExtentKey::parse(&bytes).is_some(), "{name}"),
            "hfs-extent-record" => {
                assert_eq!(parse_extent_record(&bytes), Some(extent_record()), "{name}")
            }
            "hfs-catalog-key" => assert!(CatalogKey::parse(&bytes).is_some(), "{name}"),
            _ => {
                let record =
                    CatalogRecord::parse(&bytes).unwrap_or_else(|e| panic!("{name}: {e:?}"));
                assert_eq!(record.encode(), bytes, "{name}");
            }
        }
    }
}

/// The DiskCopy 4.2 and probe seeds are what the targets expect.
#[test]
fn wrapper_and_probe_seeds_are_recognised() {
    use crate::fs::probe::{Content, PROBE_BYTES, probe_bytes};

    let image = standard_volume().image;
    let wrapped = dc42::fixtures::build(b"Atlas HD", &image, &[]);
    let (header, disk) =
        dc42::open_source(Arc::new(MemSource::new(wrapped))).expect("dc42 seed opens");
    assert_eq!(disk.len(), image.len() as u64);
    assert_eq!(header.name, b"Atlas HD");
    let head = &image[..image.len().min(PROBE_BYTES)];
    assert_eq!(probe_bytes(head, image.len() as u64), Content::Hfs);
}
