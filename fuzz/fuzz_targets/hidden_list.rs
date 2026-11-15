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

//! Fuzzes the parser of a volume root's `.hidden` list.
//!
//! Input: the file's bytes.

#![no_main]

use disk_atlas_lib::mac::conventions::{
    MAX_HIDDEN_NAME, MAX_HIDDEN_NAMES, parse_hidden_list,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let names = parse_hidden_list(data);
    assert!(names.len() <= MAX_HIDDEN_NAMES);
    for name in &names {
        assert!(!name.is_empty() && name.len() <= MAX_HIDDEN_NAME);
        assert!(!name.contains(['\0', '/', '\n', '\r']));
    }
});
