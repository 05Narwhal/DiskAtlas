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

//! Fuzzes the sparse image reader: the header, the band table and reads
//! across band boundaries.

#![no_main]

use std::sync::Arc;

use disk_atlas_lib::image::sparseimage::SparseImageSource;
use disk_atlas_lib::io::block::{BlockSource, MemSource};
use libfuzzer_sys::fuzz_target;

/// Most disk bytes read per input, so a large claimed size costs little time.
const READ_LIMIT: u64 = 1 << 20;
/// Bytes per read: not a multiple of any band size, so reads straddle bands.
const STEP: usize = 64 * 1024 + 1;

fuzz_target!(|data: &[u8]| {
    let file: Arc<dyn BlockSource> = Arc::new(MemSource::new(data));
    let Ok(disk) = SparseImageSource::new(file) else {
        return;
    };
    let end = disk.len().min(READ_LIMIT);
    let mut buf = vec![0u8; STEP];
    let mut pos = 0;
    while pos < end {
        let n = usize::try_from(end - pos).map_or(STEP, |left| left.min(STEP));
        if disk.read_at(pos, &mut buf[..n]).is_err() {
            break;
        }
        pos += n as u64;
    }
    // Read the start again, after the walk.
    if end > 0 {
        let _ = disk.read_bulk(0, &mut buf[..1]);
    }
    // And the last byte of the disk.
    if !disk.is_empty() {
        let _ = disk.read_at(disk.len() - 1, &mut buf[..1]);
    }
});
