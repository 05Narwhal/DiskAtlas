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

//! Fuzzes mounting an HFS+ or HFSX volume and walking it.
//!
//! Directory hard links can make the catalog cyclic, so the walk is bounded
//! in depth and in total entries. Errors are fine; a panic is the bug.

#![no_main]

use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use disk_atlas_lib::extract::{self, CopyEvent, CopyOptions, HostRules, Item};
use disk_atlas_lib::fs::hfsplus::{self, HfsPlus};
use disk_atlas_lib::fs::{
    DirBatch, DirProgress, FileSystem, MAX_ALLOCATION_BUCKETS, NodeId, Stream,
};
use disk_atlas_lib::io::block::{BlockSource, MemSource};
use libfuzzer_sys::fuzz_target;

/// Entries per `next_batch` call.
const BATCH: usize = 64;
/// Deepest folder the walk descends into.
const MAX_DEPTH: usize = 8;
/// Most entries the walk visits in total.
const MAX_ENTRIES: usize = 2000;
/// Host bytes the copy may write before it is cancelled. The copy reports
/// progress once per MiB, so the real total can reach about twice this.
const COPY_BYTE_CAP: u64 = 1 << 20;
/// Copy events (files, folders, failures, skips) before the copy is cancelled.
const COPY_EVENT_CAP: usize = 4000;

fuzz_target!(|data: &[u8]| {
    let source: Arc<dyn BlockSource> = Arc::new(MemSource::new(data));
    let Ok(volume) = hfsplus::mount(source) else {
        return;
    };
    exercise(&volume);
    copy_everything(&volume);
});

/// Copies the whole volume into a scratch folder, so hostile names reach the
/// sanitizer and hostile forks reach the AppleDouble writer. Bounded in bytes
/// and events by cancelling the copy; the folder is removed afterwards.
fn copy_everything(volume: &HfsPlus) {
    let dest = std::env::temp_dir().join(format!("disk-atlas-fuzz-copy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    if std::fs::create_dir_all(&dest).is_err() {
        return;
    }
    copy_into(volume, &dest);
    let _ = std::fs::remove_dir_all(&dest);
}

fn copy_into(volume: &HfsPlus, dest: &Path) {
    let items = [Item {
        node: volume.root(),
        name: "volume".to_owned(),
    }];
    // Windows rules are the strictest, so every sanitizing branch is reachable
    // whatever host the fuzzer runs on.
    let options = CopyOptions {
        forks: true,
        hidden: true,
        // Everything on the volume is copied, as before the switch existed.
        copy_system_items: true,
        rules: HostRules::Windows,
    };
    let cancel = AtomicBool::new(false);
    let mut events = 0usize;
    let _ = extract::copy_out(volume, &items, dest, &options, &cancel, &mut |event| {
        events += 1;
        let over_bytes = matches!(
            event,
            CopyEvent::Bytes { total_bytes } if total_bytes >= COPY_BYTE_CAP
        );
        if over_bytes || events >= COPY_EVENT_CAP {
            cancel.store(true, Ordering::Relaxed);
        }
    });
}

/// Everything the target does to a mounted volume. Add new calls here.
fn exercise(volume: &HfsPlus) {
    let _ = volume.info();
    let _ = volume.journal();
    let root = volume.root();
    let _ = volume.metadata(root);
    let _ = volume.allocation_map(MAX_ALLOCATION_BUCKETS);
    let mut budget = MAX_ENTRIES;
    walk(volume, root, 0, &mut budget);
    let _ = volume.identity();
    // The whole-volume scan, under the same entry budget as the walk.
    let mut seen = 0usize;
    let _ = volume.scan(&mut |entry| {
        let _ = (entry.node, entry.parent, entry.name.len());
        seen += 1;
        if seen >= MAX_ENTRIES {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    });
}

/// Lists `dir`, then looks up and describes every entry, descending into
/// folders until the depth or entry budget runs out.
fn walk(volume: &HfsPlus, dir: NodeId, depth: usize, budget: &mut usize) {
    let Ok(mut stream) = volume.read_dir(dir) else {
        return;
    };
    // Folders found in this listing, visited after it is fully read.
    let mut subdirs: Vec<NodeId> = Vec::new();
    let mut batch = DirBatch::new();
    loop {
        batch.clear();
        let progress = stream.next_batch(&mut batch, BATCH);
        for entry in batch.iter() {
            if *budget == 0 {
                return;
            }
            *budget -= 1;
            let _ = volume.lookup(dir, entry.name);
            let _ = volume.metadata(entry.node);
            visit_entry(volume, entry.node);
            if entry.is_browsable() {
                subdirs.push(entry.node);
            }
        }
        match progress {
            Ok(DirProgress::More) => {}
            Ok(DirProgress::Done) | Err(_) => break,
        }
    }
    if depth >= MAX_DEPTH {
        return;
    }
    for sub in subdirs {
        if *budget == 0 {
            return;
        }
        walk(volume, sub, depth + 1, budget);
    }
}

/// Most file content read per entry.
const READ_LIMIT: u64 = 256 * 1024;
/// Bytes per content read; one more than a decmpfs chunk, so reads straddle
/// chunk boundaries.
const STEP: usize = 64 * 1024 + 1;

fn visit_entry(volume: &HfsPlus, node: NodeId) {
    if let Ok(list) = volume.xattrs(node) {
        for attr in list.iter().take(8) {
            let _ = volume.read_xattr(node, &attr.name, 64 * 1024);
        }
    }
    let _ = volume.open(node, Stream::Resource);
    let _ = volume.extents(node, Stream::Data);
    let _ = volume.extents(node, Stream::Resource);
    let Ok(content) = volume.open(node, Stream::Data) else {
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
}
