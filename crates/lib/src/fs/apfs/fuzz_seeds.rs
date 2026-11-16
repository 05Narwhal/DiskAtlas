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

//! Writes seed inputs for the APFS fuzz targets in `fuzz/`.

use std::fs;
use std::path::Path;

use super::test_container::{
    BS, ContainerBuilder, VolumeSpec, sealed_with_snapshot, standard_volume,
};

/// Containers for the mount target.
fn containers() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        (
            "apfs-standard",
            ContainerBuilder::new()
                .volumes(vec![standard_volume("Atlas", 0)])
                .free_blocks(16)
                .build()
                .image,
        ),
        (
            "apfs-two-volumes-insensitive",
            ContainerBuilder::new()
                .volumes(vec![
                    standard_volume("Macintosh HD", 0x9),
                    VolumeSpec::new("Empty"),
                ])
                .build()
                .image,
        ),
        (
            "apfs-sealed-with-snapshot",
            ContainerBuilder::new()
                .volumes(vec![sealed_with_snapshot(), standard_volume("Data", 0x9)])
                .build()
                .image,
        ),
        (
            "apfs-stale-block-zero",
            ContainerBuilder::new()
                .checkpoints(&[9, 12])
                .block_zero_xid(5)
                .build()
                .image,
        ),
    ]
}

/// Writes the seeds when `DISK_ATLAS_FUZZ_SEEDS` names a folder. Run with
/// `cargo test -p disk-atlas-lib --lib write_fuzz_seeds -- --ignored`.
#[test]
#[ignore = "writes files; run by the fuzz CI job"]
fn write_fuzz_seeds() {
    let Some(root) = std::env::var_os("DISK_ATLAS_FUZZ_SEEDS") else {
        return;
    };
    let root = Path::new(&root);
    let mount_dir = root.join("apfs_mount");
    let node_dir = root.join("apfs_btree_node");
    for dir in [&mount_dir, &node_dir] {
        fs::create_dir_all(dir).expect("create seed directory");
    }
    for (name, image) in containers() {
        // Every whole block is also a node seed: the trees' nodes among them.
        for (i, block) in image.as_chunks::<BS>().0.iter().enumerate() {
            if block.iter().any(|&b| b != 0) {
                fs::write(node_dir.join(format!("{name}-block-{i}")), &block[..])
                    .expect("write node seed");
            }
        }
        fs::write(mount_dir.join(name), image).expect("write mount seed");
    }
}
