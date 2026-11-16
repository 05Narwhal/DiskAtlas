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

//! Writes seed inputs for the UDF fuzz target in `fuzz/`.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use super::test_image::{Entry, Opts, Piece, build};
use crate::fs::FileSystem;
use crate::io::block::MemSource;

/// The largest input the fuzz run accepts (`-max_len=1048576`).
const MAX_SEED: usize = 1 << 20;

fn tree() -> Vec<Entry> {
    vec![
        Entry::file("readme.txt", b"hello disc"),
        Entry::dir("Docs", vec![Entry::file("Report.txt", &[7u8; 3000])]),
        Entry::pieces(
            "sparse.bin",
            vec![
                Piece::Data(vec![1u8; 100]),
                Piece::Hole(3000),
                Piece::Data(vec![2u8; 50]),
            ],
        ),
        Entry::link("current", "../lib/x"),
    ]
}

fn volumes() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("udf-plain", build(&tree(), &Opts::default())),
        (
            "udf-metadata",
            build(
                &tree(),
                &Opts {
                    metadata: true,
                    extended: true,
                    long_ads: true,
                    ..Opts::default()
                },
            ),
        ),
        (
            "udf-embedded-512",
            build(
                &tree(),
                &Opts {
                    sector: 512,
                    embed: 64,
                    ..Opts::default()
                },
            ),
        ),
    ]
}

/// Writes the seeds into `$DISK_ATLAS_FUZZ_SEEDS/udf_mount` (and `probe`).
/// Run it with `cargo test -p disk-atlas-lib --lib write_fuzz_seeds -- --ignored`.
#[test]
#[ignore = "writes files; run by the fuzz CI job"]
fn write_fuzz_seeds() {
    let Some(root) = std::env::var_os("DISK_ATLAS_FUZZ_SEEDS") else {
        return;
    };
    let root = Path::new(&root);
    let mount_dir = root.join("udf_mount");
    let probe_dir = root.join("probe");
    for dir in [&mount_dir, &probe_dir] {
        fs::create_dir_all(dir).expect("create seed directory");
    }
    for (name, image) in volumes() {
        fs::write(mount_dir.join(name), &image).expect("write mount seed");
        let head = &image[..image.len().min(crate::fs::probe::PROBE_BYTES)];
        fs::write(probe_dir.join(name), head).expect("write probe seed");
    }
}

/// Every seed fits the run's input limit, mounts and lists its root.
#[test]
fn mount_seeds_are_valid_and_small() {
    for (name, image) in volumes() {
        assert!(image.len() <= MAX_SEED, "{name}: {} bytes", image.len());
        let volume =
            super::mount(Arc::new(MemSource::new(image))).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(volume.read_dir(volume.root()).is_ok(), "{name}");
    }
}
