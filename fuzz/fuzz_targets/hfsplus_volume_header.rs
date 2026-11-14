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

//! Fuzzes `VolumeHeader::parse` and the wrapper/header locator.

#![no_main]

use disk_atlas_lib::fs::hfsplus::volume_header::{VolumeHeader, locate, read_header};
use disk_atlas_lib::io::block::MemSource;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(header) = VolumeHeader::parse(data) {
        let _ = header.is_hfsx();
        let _ = header.is_journaled();
        let _ = header.unmounted_cleanly();
        let _ = header.volume_bytes();
    }
    // The same bytes as a whole device: wrapper detection and the header at
    // offset 1024.
    let source = MemSource::new(data);
    let _ = locate(&source);
    let _ = read_header(&source);
});
