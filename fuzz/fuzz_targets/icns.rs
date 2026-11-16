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


//! Fuzzes the icon file reader: parse, then ask for a picture at several sizes.
//!
//! Input: the `.icns` file's bytes.

#![no_main]

use disk_atlas_lib::mac::icns::{IconImage, Icns};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(icns) = Icns::parse(data) else {
        return;
    };
    for wanted in [16, 48, 128, 1024] {
        if let Some(IconImage::Rgba(image)) = icns.best(wanted) {
            assert_eq!(
                image.pixels.len() as u64,
                u64::from(image.width) * u64::from(image.height) * 4
            );
        }
    }
    let _ = icns.sizes();
});
