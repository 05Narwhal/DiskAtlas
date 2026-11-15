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

//! Fuzzes mounting an APFS container and walking every volume in it.
//!
//! The input is the container. Opening covers block 0, checkpoint
//! selection, the checkpoint maps and the object maps; each volume that
//! mounts is walked, described, read and scanned within bounds. A damaged
//! tree can loop, so the walk is bounded in depth and in total entries.
//! Errors are fine; a panic is the bug.

#![no_main]

use std::ops::ControlFlow;
use std::sync::Arc;

use disk_atlas_lib::fs::apfs;
use disk_atlas_lib::fs::probe::VolumeState;
use disk_atlas_lib::fs::{
    DirBatch, DirProgress, FileSystem, MAX_ALLOCATION_BUCKETS, NodeId, Stream,
};
use disk_atlas_lib::io::block::{BlockSource, MemSource};
use libfuzzer_sys::fuzz_target;

/// Entries per `next_batch` call.
const BATCH: usize = 64;
/// Deepest folder the walk descends into.
const MAX_DEPTH: usize = 8;
/// Most entries the walk visits in total, per volume.
const MAX_ENTRIES: usize = 2000;
/// Most content read per stream.
const READ_LIMIT: u64 = 256 * 1024;
/// Bytes per content read; odd, so reads straddle block boundaries.
const STEP: usize = 64 * 1024 + 1;
/// Largest attribute value read.
const XATTR_LIMIT: usize = 64 * 1024;
/// Snapshots opened per volume.
const MAX_SNAPSHOTS: usize = 4;

fuzz_target!(|data: &[u8]| {
    let source: Arc<dyn BlockSource> = Arc::new(MemSource::new(data));
    let Ok(volumes) = apfs::mount(source) else {
        return;
    };
    for volume in volumes {
        if let VolumeState::Ready(fs) = volume.state {
            exercise(&*fs);
            if let Ok(snapshots) = fs.snapshots() {
                for snap in snapshots.iter().take(MAX_SNAPSHOTS) {
                    if let Ok(view) = fs.open_snapshot(snap.id) {
                        exercise(&*view);
                    }
                }
            }
        }
    }
});

/// Everything the target does to a mounted volume. Add new calls here.
fn exercise(volume: &dyn FileSystem) {
    let _ = volume.info();
    let root = volume.root();
    let _ = volume.metadata(root);
    let _ = volume.allocation_map(MAX_ALLOCATION_BUCKETS);
    let mut budget = MAX_ENTRIES;
    walk(volume, root, 0, &mut budget);
    let _ = volume.identity();
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
fn walk(volume: &dyn FileSystem, dir: NodeId, depth: usize, budget: &mut usize) {
    let Ok(mut stream) = volume.read_dir(dir) else {
        return;
    };
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

/// Maps and reads both streams of `node` and its attributes, within limits.
fn visit_entry(volume: &dyn FileSystem, node: NodeId) {
    if let Ok(xattrs) = volume.xattrs(node) {
        for x in xattrs.iter().take(16) {
            let _ = volume.read_xattr(node, &x.name, XATTR_LIMIT);
        }
    }
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
