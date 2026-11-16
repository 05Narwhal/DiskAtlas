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


//! Fuzzes the Recycle Bin `$I` record and the Linux `.trashinfo` readers.
//!
//! Input: the file's bytes, read as a `$I` record and as `.trashinfo` text.

#![no_main]

use disk_atlas_lib::mac::trash;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let head = &data[..data.len().min(trash::MAX_TRASH_INFO)];
    if let Ok(record) = trash::parse_recycle_record(head) {
        assert!(!record.original_path.is_empty());
    }
    let _ = trash::parse_trashinfo(&String::from_utf8_lossy(head));
    let _ = trash::recycle_data_name(&String::from_utf8_lossy(head));
});
