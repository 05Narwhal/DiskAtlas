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

//! Fuzzes decmpfs: the header, every compression kind, the resource fork
//! chunk tables and the decoders behind them.
//!
//! Input: the attribute's length (u16 LE), the `com.apple.decmpfs`
//! attribute, then the resource fork.

#![no_main]

use std::sync::Arc;

use disk_atlas_lib::fs::hfsplus::compression::{self, DecmpfsHeader};
use disk_atlas_lib::io::block::{BlockSource, MemSource};
use libfuzzer_sys::fuzz_target;

/// Most decompressed bytes read per input, so a large claimed size costs
/// little time.
const READ_LIMIT: u64 = 1 << 20;
/// Bytes per read: one decmpfs chunk, plus one byte so reads straddle
/// chunk boundaries.
const STEP: usize = 64 * 1024 + 1;

fuzz_target!(|data: &[u8]| {
    let Some((len, rest)) = data.split_first_chunk::<2>() else {
        return;
    };
    let len = usize::from(u16::from_le_bytes(*len)).min(rest.len());
    let (xattr, fork) = rest.split_at(len);
    let _ = DecmpfsHeader::parse(xattr, 1);
    let fork: Arc<dyn BlockSource> = Arc::new(MemSource::new(fork));
    let Ok(content) = compression::open(xattr, 1, &mut || Ok(Arc::clone(&fork))) else {
        return;
    };
    let end = content.len().min(READ_LIMIT);
    let mut buf = vec![0u8; STEP];
    let mut pos = 0;
    while pos < end {
        let n = usize::try_from(end - pos).map_or(STEP, |left| left.min(STEP));
        if content.read_at(pos, &mut buf[..n]).is_err() {
            break;
        }
        pos += n as u64;
    }
    // Read the tail again, after the cache moved on.
    if end > 0 {
        let _ = content.read_at(0, &mut buf[..1]);
    }
});
