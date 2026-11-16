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


//! Fuzzes the resource map reader: read the map, then every resource of every type.
//!
//! Input: the resource fork's bytes.

#![no_main]

use disk_atlas_lib::io::block::{BlockSource, MemSource};
use disk_atlas_lib::mac::resource_map::ResourceMap;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let fork = MemSource::new(data.to_vec());
    let Ok(map) = ResourceMap::read(&fork) else {
        return;
    };
    let mut seen = 0usize;
    for (kind, count) in map.types() {
        assert_eq!(map.ids(kind).count(), count);
        for id in map.ids(kind) {
            seen += 1;
            let _ = map.name(kind, id);
            if let Ok(Some(bytes)) = map.load(&fork, kind, id, 1 << 16) {
                assert!(bytes.len() as u64 <= fork.len());
            }
        }
        // A few reads are enough: the map itself is what is under test.
        if seen > 256 {
            break;
        }
    }
});
