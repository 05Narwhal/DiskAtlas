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

//! Fuzzes loading a saved search index, then uses what loaded.
//!
//! The expected identity is taken from the input's own header (bytes 24..40
//! and 40..56, zero-filled when the input is shorter) so the fuzzer gets
//! past the identity check. An index that loads is searched, ordered and
//! walked, which touches every array. Errors are fine; a panic is the bug.

#![no_main]

use std::sync::atomic::AtomicBool;

use disk_atlas_lib::fs::VolumeIdentity;
use disk_atlas_lib::search::{HitOrder, Query, order_hits, persist};
use libfuzzer_sys::fuzz_target;

/// Entries probed one by one; the searches cover the rest.
const PROBES: u32 = 64;

/// 16 bytes of `data` from `at`, zero-filled past the end.
fn field(data: &[u8], at: usize) -> [u8; 16] {
    let mut out = [0u8; 16];
    for (dst, src) in out.iter_mut().zip(data.iter().skip(at)) {
        *dst = *src;
    }
    out
}

fuzz_target!(|data: &[u8]| {
    let identity = VolumeIdentity {
        id: field(data, 24),
        change: field(data, 40),
    };
    let Ok(Some(index)) = persist::load(&mut &data[..], data.len() as u64, identity) else {
        return;
    };
    let never = AtomicBool::new(false);
    let by_text = Query {
        text: "a".to_owned(),
        ..Query::default()
    };
    let by_size = Query {
        size: Some(0..=1 << 20),
        ..Query::default()
    };
    for query in [&by_text, &by_size] {
        if let Some(hits) = index.search(query, None, &never) {
            let _ = order_hits(&index, &hits, HitOrder::Name, false, &never);
        }
    }
    let mut path = String::new();
    for i in 0..index.len().min(PROBES) {
        let _ = index.entry(i);
        let _ = index.crumbs(i);
        path.clear();
        index.folder_path(i, "/", &mut path);
        let _ = index.subtree(i);
    }
});
