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


//! Fuzzes the `.DS_Store` reader and every accessor on what it reads.
//!
//! Input: the file's bytes.

#![no_main]

use disk_atlas_lib::mac::ds_store::{DsStore, MAX_DS_RECORDS};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(store) = DsStore::parse(data) else {
        return;
    };
    assert!(store.records().count() <= MAX_DS_RECORDS);
    let _ = (
        store.view_style(),
        store.icon_view(),
        store.window(),
        store.list_sort(),
    );
    for record in store.records().take(64) {
        let _ = (
            store.comment(&record.name),
            store.put_back(&record.name),
            store.icon_place(&record.name),
        );
    }
});
