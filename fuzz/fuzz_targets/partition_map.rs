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

//! Fuzzes the partition map readers (GPT, APM, MBR) over a whole-disk image.

#![no_main]

use std::sync::Arc;

use disk_atlas_lib::io::block::{BlockSource, MemSource};
use disk_atlas_lib::partition;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let source: Arc<dyn BlockSource> = Arc::new(MemSource::new(data));
    let Ok(Some(map)) = partition::read(&*source) else {
        return;
    };
    for part in map.partitions.iter().take(32) {
        // Slicing validates the range; a slice that opens must probe cleanly.
        if let Ok(slice) = part.slice(Arc::clone(&source)) {
            let _ = disk_atlas_lib::fs::probe::probe(&slice);
        }
    }
});
