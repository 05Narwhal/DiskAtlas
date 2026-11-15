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

//! Fuzzes mounting a classic HFS volume and walking it.
//!
//! The input is the volume. It is mounted in Mac OS Roman and again with
//! the Cyrillic encoding, so the name decoding is covered; each mount is
//! walked, read and scanned within bounds, and the Roman mount is copied out
//! to a scratch folder. A damaged catalog can loop, so the walk is bounded
//! in depth and in total entries. Errors are fine; a panic is the bug.

#![no_main]

use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use disk_atlas_lib::extract::{self, CopyEvent, CopyOptions, HostRules, Item};
use disk_atlas_lib::fs::hfs::encoding::Encoding;
use disk_atlas_lib::fs::hfs::{self, Hfs};
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
/// Most content read per fork.
const READ_LIMIT: u64 = 256 * 1024;
/// Bytes per content read; odd, so reads straddle block boundaries.
const STEP: usize = 64 * 1024 + 1;

fuzz_target!(|data: &[u8]| {
    let source: Arc<dyn BlockSource> = Arc::new(MemSource::new(data));
    if let Ok(volume) = hfs::mount(Arc::clone(&source)) {
        exercise(&volume);
        copy_everything(&volume);
    }
    if let Ok(volume) = hfs::mount_with(source, Encoding::Cyrillic) {
        exercise(&volume);
    }
});

/// Copies the whole volume into a scratch folder, so hostile names reach the
/// sanitizer and hostile forks reach the AppleDouble writer. Bounded in bytes
/// and events by cancelling the copy; the folder is removed afterwards.
fn copy_everything(volume: &Hfs) {
    let dest =
        std::env::temp_dir().join(format!("disk-atlas-fuzz-hfs-copy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dest);
    if std::fs::create_dir_all(&dest).is_err() {
        return;
    }
    copy_into(volume, &dest);
    let _ = std::fs::remove_dir_all(&dest);
}

fn copy_into(volume: &Hfs, dest: &Path) {
    let items = [Item {
        node: volume.root(),
        name: "volume".to_owned(),
    }];
    // Windows rules are the strictest, so every sanitizing branch is reachable
    // whatever host the fuzzer runs on.
    let options = CopyOptions {
        forks: true,
        hidden: true,
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
fn exercise(volume: &Hfs) {
    let _ = volume.info();
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
fn walk(volume: &Hfs, dir: NodeId, depth: usize, budget: &mut usize) {
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

/// Maps and reads both forks of `node`, up to [`READ_LIMIT`] each.
fn visit_entry(volume: &Hfs, node: NodeId) {
    for stream in [Stream::Data, Stream::Resource] {
        let _ = volume.extents(node, stream);
        let Ok(content) = volume.open(node, stream) else {
            continue;
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
}
