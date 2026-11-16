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

use super::test_image::{BLOCK, FIRST_TABLE, INODE_SIZE, Item, LABEL, MTIME, build};
use super::*;
use crate::fs::DirBatch;
use crate::fs::probe::{self, Content};

fn mounted(items: &[Item]) -> Ext {
    mount(Arc::new(MemSource::new(build(items)))).expect("mount")
}

fn list(fs: &Ext, dir: NodeId) -> Vec<(String, NodeId, NodeKind, Option<u64>)> {
    let mut stream = fs.read_dir(dir).unwrap();
    let mut batch = DirBatch::default();
    let mut out = Vec::new();
    loop {
        batch.clear();
        let progress = stream.next_batch(&mut batch, 2).unwrap();
        out.extend(
            batch
                .iter()
                .map(|e| (e.name.to_owned(), e.node, e.kind, e.size)),
        );
        if progress == DirProgress::Done {
            return out;
        }
    }
}

fn read_all(source: &dyn BlockSource) -> Vec<u8> {
    let mut buf = vec![0u8; usize::try_from(source.len()).unwrap()];
    source.read_at(0, &mut buf).unwrap();
    buf
}

fn bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn tree() -> Vec<Item> {
    vec![
        Item::File("readme.txt", b"hello ext".to_vec()),
        Item::File("big.bin", bytes(20_000, 3)),
        Item::Dir(
            "Docs",
            vec![
                Item::File("Report.txt", bytes(3000, 9)),
                Item::File("empty", Vec::new()),
            ],
        ),
        Item::Link("short", "Docs/Report.txt"),
        Item::Link("dirlink", "Docs"),
    ]
}

#[test]
fn probe_finds_it_and_mounts() {
    let image = build(&tree());
    let source: Arc<dyn BlockSource> = Arc::new(MemSource::new(image));
    let head_len = usize::try_from(source.len())
        .unwrap()
        .min(probe::PROBE_BYTES);
    let mut head = vec![0u8; head_len];
    source.read_at(0, &mut head).unwrap();
    assert_eq!(probe::probe_bytes(&head, source.len()), Content::Ext);
    assert!(matches!(
        probe::mount(source, Content::Ext).unwrap(),
        Some(probe::Mount::Volume(_))
    ));
}

#[test]
fn volume_info_and_identity() {
    let fs = mounted(&tree());
    let info = fs.info();
    assert_eq!(info.name.as_bytes(), LABEL);
    assert_eq!(info.format, Some("ext"));
    assert_eq!(fs.identity().unwrap().id, super::test_image::UUID);
}

#[test]
fn lists_root_and_subfolder_in_small_batches() {
    let fs = mounted(&tree());
    let mut root = list(&fs, fs.root());
    root.sort_by(|a, b| a.0.cmp(&b.0));
    let names: Vec<_> = root.iter().map(|r| r.0.as_str()).collect();
    assert_eq!(names, ["Docs", "big.bin", "dirlink", "readme.txt", "short"]);
    let find = |n: &str| root.iter().find(|r| r.0 == n).unwrap();
    assert_eq!(find("Docs").2, NodeKind::Directory);
    assert_eq!(find("big.bin").3, Some(20_000));
    assert_eq!(find("short").2, NodeKind::Symlink);
    let docs = list(&fs, find("Docs").1);
    assert_eq!(docs.len(), 2);
}

#[test]
fn reads_direct_and_indirect_files_exactly() {
    let fs = mounted(&tree());
    let big = fs.lookup(fs.root(), "big.bin").unwrap().unwrap();
    let source = fs.open(big, Stream::Data).unwrap();
    assert_eq!(read_all(&*source), bytes(20_000, 3));
    // A read that starts mid-block and crosses the direct/indirect boundary.
    let mut part = vec![0u8; 2500];
    source.read_at(11 * BLOCK as u64 + 100, &mut part).unwrap();
    assert_eq!(part, bytes(20_000, 3)[11 * BLOCK + 100..11 * BLOCK + 2600]);
    assert!(source.read_at(19_999, &mut [0u8; 2]).is_err());
    let docs = fs.lookup(fs.root(), "Docs").unwrap().unwrap();
    let empty = fs.lookup(docs, "empty").unwrap().unwrap();
    assert_eq!(fs.open(empty, Stream::Data).unwrap().len(), 0);
}

/// A sparse file: one block of data, then holes as far as the size says, across
/// the single and double indirect ranges. A zero pointer is a hole, not block 0
/// of the volume (found on a real mke2fs image; ext4-view 1.0.0 failed it).
#[test]
fn holes_under_indirect_blocks_read_as_zeros() {
    let mut image = build(&[Item::File("sparse", vec![7u8; BLOCK])]);
    // The first inode the builder hands out is 11. Raise its size to 300,000
    // bytes (293 blocks, past 12 direct + 256 indirect) with no blocks behind it.
    let size_at = FIRST_TABLE * BLOCK + 10 * INODE_SIZE + 4;
    image[size_at..size_at + 4].copy_from_slice(&300_000u32.to_le_bytes());
    let fs = mount(Arc::new(MemSource::new(image))).expect("mount");
    let node = fs.lookup(fs.root(), "sparse").unwrap().unwrap();
    let source = fs.open(node, Stream::Data).unwrap();
    assert_eq!(source.len(), 300_000);
    let data = read_all(&*source);
    assert!(data[..BLOCK].iter().all(|&b| b == 7));
    assert!(data[BLOCK..].iter().all(|&b| b == 0));
}

#[test]
fn metadata_symlinks_and_owner() {
    let fs = mounted(&tree());
    let link = fs.lookup(fs.root(), "short").unwrap().unwrap();
    let m = fs.metadata(link).unwrap();
    assert_eq!(m.kind, NodeKind::Symlink);
    assert_eq!(m.link_target.as_deref(), Some("Docs/Report.txt"));
    assert_eq!(
        read_all(&*fs.open(link, Stream::Data).unwrap()),
        b"Docs/Report.txt"
    );
    let dirlink = fs.lookup(fs.root(), "dirlink").unwrap().unwrap();
    assert!(
        fs.metadata(dirlink)
            .unwrap()
            .flags
            .contains(EntryFlags::LINKS_TO_DIRECTORY)
    );
    let file = fs.lookup(fs.root(), "readme.txt").unwrap().unwrap();
    let m = fs.metadata(file).unwrap();
    assert_eq!(m.size, Some(9));
    assert_eq!(m.modified, Some(Timestamp::from_unix(i64::from(MTIME), 0)));
    let owner = m.owner.unwrap();
    assert_eq!((owner.user, owner.group, owner.mode), (1000, 100, 0o100644));
}

#[test]
fn lookup_and_error_cases() {
    let fs = mounted(&tree());
    assert_eq!(fs.lookup(fs.root(), "nope").unwrap(), None);
    assert_eq!(fs.lookup(fs.root(), "a/b").unwrap(), None);
    let file = fs.lookup(fs.root(), "readme.txt").unwrap().unwrap();
    assert!(matches!(
        fs.lookup(file, "x"),
        Err(Error::NotADirectory { .. })
    ));
    assert!(matches!(
        fs.read_dir(file),
        Err(Error::NotADirectory { .. })
    ));
    assert!(matches!(
        fs.open(fs.root(), Stream::Data),
        Err(Error::IsADirectory { .. })
    ));
    assert!(matches!(
        fs.open(file, Stream::Resource),
        Err(Error::NoSuchStream { .. })
    ));
    for bogus in [999u64, u64::MAX] {
        assert!(matches!(
            fs.metadata(NodeId(bogus)),
            Err(Error::NotFound { .. })
        ));
    }
}

#[test]
fn scan_reaches_every_entry() {
    let fs = mounted(&tree());
    let mut count = 0;
    fs.scan(&mut |_| {
        count += 1;
        std::ops::ControlFlow::Continue(())
    })
    .unwrap();
    assert_eq!(count, 7);
}

#[test]
fn hostile_superblocks_fail_cleanly() {
    let good = build(&tree());
    // Not ext at all, bad block size, zero group counts, inode size past the
    // block, unknown incompatible features.
    let patches: [(usize, &[u8]); 6] = [
        (1024 + 0x38, &[0, 0]),
        (1024 + 0x18, &[0xFF, 0xFF, 0xFF, 0xFF]),
        (1024 + 0x20, &[0, 0, 0, 0]),
        (1024 + 0x28, &[0, 0, 0, 0]),
        (1024 + 0x58, &[0xFF, 0xFF]),
        (1024 + 0x60, &[0xFF, 0xFF, 0xFF, 0xFF]),
    ];
    for (at, patch) in patches {
        let mut image = good.clone();
        image[at..at + patch.len()].copy_from_slice(patch);
        assert!(
            mount(Arc::new(MemSource::new(image))).is_err(),
            "patch at {at:#x}"
        );
    }
    assert!(mount(Arc::new(MemSource::new(good[..1500].to_vec()))).is_err());
    assert!(mount(Arc::new(MemSource::new(bytes(70_000, 5)))).is_err());
}

#[test]
fn damaged_metadata_never_panics() {
    let good = build(&tree());
    // Flip one byte at a time across the metadata area, then walk what mounts.
    for at in (1024..14 * BLOCK).step_by(37) {
        let mut image = good.clone();
        image[at] ^= 0xFF;
        let Ok(fs) = mount(Arc::new(MemSource::new(image))) else {
            continue;
        };
        let mut queue = vec![fs.root()];
        let mut seen = 0;
        while let Some(dir) = queue.pop() {
            seen += 1;
            if seen > 50 {
                break;
            }
            let Ok(mut stream) = fs.read_dir(dir) else {
                continue;
            };
            let mut batch = DirBatch::default();
            for _ in 0..64 {
                batch.clear();
                let progress = stream.next_batch(&mut batch, 8);
                for e in batch.iter() {
                    let _ = fs.metadata(e.node);
                    if let Ok(src) = fs.open(e.node, Stream::Data) {
                        let mut buf = vec![0u8; 4096];
                        let n = usize::try_from(src.len()).unwrap_or(0).min(buf.len());
                        let _ = src.read_at(0, &mut buf[..n]);
                    }
                    if e.is_browsable() {
                        queue.push(e.node);
                    }
                }
                if !matches!(progress, Ok(DirProgress::More)) {
                    break;
                }
            }
        }
    }
}
