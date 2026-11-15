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

//! Fuzzes the binary property list reader and the attribute readers on it.
//!
//! Input: the property list's bytes (also read as quarantine text).

#![no_main]

use disk_atlas_lib::mac::{bplist, xattr};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = bplist::parse(data);
    let _ = bplist::parse_any(data);
    let _ = xattr::where_froms(data);
    let _ = xattr::user_tags(data);
    let _ = xattr::finder_comment(data);
    let _ = xattr::parse_quarantine(&String::from_utf8_lossy(data));
});
