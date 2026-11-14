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

//! Fuzzes HFS+ journal replay: the input is the device. The volume header
//! is found and read as `mount` does, the journal is opened, and the whole
//! device is read through the result, so every patch the replay made is
//! read. Errors are fine; a panic is the bug.

#![no_main]

use std::sync::Arc;

use disk_atlas_lib::fs::hfsplus::journal;
use disk_atlas_lib::fs::hfsplus::volume_header::{locate, read_header};
use disk_atlas_lib::io::block::{BlockSource, MemSource, Slice};
use libfuzzer_sys::fuzz_target;

/// Bytes per read of the replayed device.
const STEP: usize = 4096;

fuzz_target!(|data: &[u8]| {
    let device: Arc<dyn BlockSource> = Arc::new(MemSource::new(data));
    let Ok(located) = locate(&*device) else {
        return;
    };
    let Ok(volume) = Slice::new(Arc::clone(&device), located.offset, located.len) else {
        return;
    };
    let Ok(header) = read_header(&volume) else {
        return;
    };
    let Ok(opened) = journal::open(device, located.offset, &header) else {
        return;
    };
    let _ = &opened.state;
    let replayed = opened.device;
    let len = replayed.len();
    let mut buf = vec![0u8; STEP];
    let mut pos = 0u64;
    while pos < len {
        let n = usize::try_from(len - pos).map_or(STEP, |left| left.min(STEP));
        let _ = replayed.read_at(pos, &mut buf[..n]);
        let _ = replayed.read_bulk(pos, &mut buf[..n]);
        pos += n as u64;
    }
});
