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

//! Fuzzes the AppleDouble / AppleSingle header reader.
//!
//! Input: the file's bytes. The header is parsed against the real length and
//! against a lying one.

#![no_main]

use disk_atlas_lib::mac::appledouble::{MAX_AD_HEADER, parse};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let head = &data[..data.len().min(MAX_AD_HEADER)];
    let len = data.len() as u64;
    if let Ok(sidecar) = parse(head, len) {
        if let Some((offset, length)) = sidecar.resource {
            assert!(offset + length <= len);
        }
    }
    // A file length that disagrees with the bytes must still not panic.
    let _ = parse(head, u64::MAX);
    let _ = parse(head, 0);
});
