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

use super::test_image::{Entry, MTIME_UNIX, Opts, Piece, build, seal};
use super::*;
use crate::fs::DirBatch;
use crate::fs::fat::test_volume::pattern;
use crate::fs::probe::{self, Content};

fn mounted(image: Vec<u8>) -> Udf {
    mount(Arc::new(MemSource::new(image))).expect("mount")
}

fn list(fs: &Udf, dir: NodeId, batch_size: usize) -> Vec<(String, NodeId, NodeKind, Option<u64>)> {
    let mut stream = fs.read_dir(dir).unwrap();
    let mut batch = DirBatch::default();
    let mut out = Vec::new();
    loop {
        batch.clear();
        let progress = stream.next_batch(&mut batch, batch_size).unwrap();
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

fn sparse() -> Vec<u8> {
    let mut v = pattern(100, 1);
    v.extend(vec![0u8; 3000]);
    v.extend(pattern(50, 2));
    v
}

fn tree() -> Vec<Entry> {
    vec![
        Entry::file("readme.txt", b"hello disc"),
        Entry::file("Big File.bin", &pattern(5000, 3)),
        Entry::dir(
            "Docs",
            vec![
                Entry::file("Report.txt", &pattern(2048, 9)),
                Entry::file("empty.bin", b""),
                Entry::dir("deep", vec![Entry::file("leaf.dat", &pattern(3000, 1))]),
            ],
        ),
        Entry::pieces(
            "sparse.bin",
            vec![
                Piece::Data(pattern(100, 1)),
                Piece::Hole(3000),
                Piece::Data(pattern(50, 2)),
            ],
        ),
        Entry::pieces(
            "multi.bin",
            vec![Piece::Data(pattern(2048, 4)), Piece::Data(pattern(100, 5))],
        ),
        Entry::file("日本語.txt", b"unicode"),
        Entry::link("current", "../lib/x"),
    ]
}

/// What every layout of the volume must show.
fn check(fs: &Udf) {
    assert_eq!(fs.info().format, Some("UDF"));
    assert_eq!(fs.info().name, "Test Volume");
    assert!(!fs.info().local_times);
    let root = list(fs, fs.root(), 3);
    let names: Vec<_> = root.iter().map(|e| e.0.as_str()).collect();
    assert_eq!(
        names,
        [
            "readme.txt",
            "Big File.bin",
            "Docs",
            "sparse.bin",
            "multi.bin",
            "日本語.txt",
            "current"
        ]
    );
    let big = fs.lookup(fs.root(), "big file.BIN").unwrap().unwrap();
    assert_eq!(
        read_all(&*fs.open(big, Stream::Data).unwrap()),
        pattern(5000, 3)
    );
    let meta = fs.metadata(big).unwrap();
    assert_eq!(meta.size, Some(5000));
    assert_eq!(meta.kind, NodeKind::File);
    assert_eq!(meta.modified.unwrap().unix_seconds(), MTIME_UNIX);
    let owner = meta.owner.unwrap();
    assert_eq!(
        (owner.user, owner.group, owner.mode),
        (1000, 100, 0o100_754)
    );
    let docs = fs.lookup(fs.root(), "Docs").unwrap().unwrap();
    assert_eq!(fs.metadata(docs).unwrap().kind, NodeKind::Directory);
    assert_eq!(fs.metadata(docs).unwrap().size, None);
    let deep = fs.lookup(docs, "deep").unwrap().unwrap();
    let leaf = fs.lookup(deep, "leaf.dat").unwrap().unwrap();
    assert_eq!(
        read_all(&*fs.open(leaf, Stream::Data).unwrap()),
        pattern(3000, 1)
    );
    let empty = fs.lookup(docs, "empty.bin").unwrap().unwrap();
    assert_eq!(fs.open(empty, Stream::Data).unwrap().len(), 0);
    assert!(fs.extents(empty, Stream::Data).unwrap().is_empty());
    let hello = fs.lookup(fs.root(), "readme.txt").unwrap().unwrap();
    assert_eq!(
        read_all(&*fs.open(hello, Stream::Data).unwrap()),
        b"hello disc"
    );
    let sp = fs.lookup(fs.root(), "sparse.bin").unwrap().unwrap();
    assert_eq!(read_all(&*fs.open(sp, Stream::Data).unwrap()), sparse());
    assert_eq!(fs.metadata(sp).unwrap().size, Some(3150));
    let multi = fs.lookup(fs.root(), "multi.bin").unwrap().unwrap();
    let want: Vec<u8> = [pattern(2048, 4), pattern(100, 5)].concat();
    assert_eq!(read_all(&*fs.open(multi, Stream::Data).unwrap()), want);
    let jp = fs.lookup(fs.root(), "日本語.txt").unwrap().unwrap();
    assert_eq!(read_all(&*fs.open(jp, Stream::Data).unwrap()), b"unicode");
    let link = fs.lookup(fs.root(), "current").unwrap().unwrap();
    let meta = fs.metadata(link).unwrap();
    assert_eq!(meta.kind, NodeKind::Symlink);
    assert_eq!(meta.link_target.as_deref(), Some("../lib/x"));
    assert_eq!(
        read_all(&*fs.open(link, Stream::Data).unwrap()),
        b"../lib/x"
    );
    assert!(fs.lookup(docs, "missing").unwrap().is_none());
    assert!(matches!(
        fs.open(docs, Stream::Data),
        Err(Error::IsADirectory { .. })
    ));
    assert!(matches!(fs.read_dir(big), Err(Error::NotADirectory { .. })));
    assert!(matches!(
        fs.open(big, Stream::Resource),
        Err(Error::NoSuchStream { .. })
    ));
}

#[test]
fn plain_volume() {
    check(&mounted(build(&tree(), &Opts::default())));
}

#[test]
fn every_layout_reads_the_same() {
    for sector in [512, 2048, 4096] {
        for metadata in [false, true] {
            for extended in [false, true] {
                for long_ads in [false, true] {
                    for embed in [0, 64] {
                        let opts = Opts {
                            sector,
                            metadata,
                            extended,
                            long_ads,
                            embed,
                            meta_blocks: 256,
                        };
                        let image = build(&tree(), &opts);
                        let fs = mount(Arc::new(MemSource::new(image))).unwrap_or_else(|e| {
                            panic!(
                                "mount {sector} meta={metadata} ext={extended} long={long_ads} embed={embed}: {e}"
                            )
                        });
                        check(&fs);
                    }
                }
            }
        }
    }
}

#[test]
fn extents_skip_holes_and_merge() {
    let fs = mounted(build(&tree(), &Opts::default()));
    let sp = fs.lookup(fs.root(), "sparse.bin").unwrap().unwrap();
    let extents = fs.extents(sp, Stream::Data).unwrap();
    assert_eq!(extents.len(), 2);
    assert_eq!(extents[0].len, 100);
    assert_eq!(extents[1].len, 50);
    let multi = fs.lookup(fs.root(), "multi.bin").unwrap().unwrap();
    let extents = fs.extents(multi, Stream::Data).unwrap();
    assert_eq!(extents.iter().map(|e| e.len).sum::<u64>(), 2148);
}

#[test]
fn big_folder_spans_blocks() {
    let files: Vec<Entry> = (0..300)
        .map(|i| Entry::file(&format!("file number {i}.txt"), b"x"))
        .collect();
    for sector in [512, 2048] {
        let opts = Opts {
            sector,
            metadata: true,
            meta_blocks: 400,
            ..Opts::default()
        };
        let fs = mounted(build(&files, &opts));
        let names = list(&fs, fs.root(), 7);
        assert_eq!(names.len(), 300);
        assert_eq!(names[299].0, "file number 299.txt");
        assert!(
            fs.lookup(fs.root(), "FILE NUMBER 150.TXT")
                .unwrap()
                .is_some()
        );
    }
}

#[test]
fn probe_mounts_udf() {
    let image = build(&tree(), &Opts::default());
    let source: Arc<dyn BlockSource> = Arc::new(MemSource::new(image));
    let mount = probe::mount(source, Content::Udf).unwrap().unwrap();
    assert!(matches!(mount, probe::Mount::Volume(_)));
}

fn walk(fs: &Udf, dir: NodeId, depth: usize, budget: &mut usize) {
    if depth > 6 || *budget == 0 {
        return;
    }
    let Ok(mut stream) = fs.read_dir(dir) else {
        return;
    };
    let mut batch = DirBatch::default();
    let mut subdirs = Vec::new();
    loop {
        batch.clear();
        let progress = stream.next_batch(&mut batch, 64);
        for e in batch.iter() {
            *budget = budget.saturating_sub(1);
            let _ = fs.metadata(e.node);
            let _ = fs.lookup(dir, e.name);
            if e.kind == NodeKind::Directory {
                subdirs.push(e.node);
            } else if let Ok(src) = fs.open(e.node, Stream::Data) {
                let n = src.len().min(8192) as usize;
                let mut buf = vec![0u8; n];
                let _ = src.read_at(0, &mut buf);
                let _ = fs.extents(e.node, Stream::Data);
            }
        }
        match progress {
            Ok(DirProgress::More) if *budget > 0 => {}
            _ => break,
        }
    }
    for sub in subdirs {
        walk(fs, sub, depth + 1, budget);
    }
}

#[test]
fn damage_never_panics() {
    for opts in [
        Opts::default(),
        Opts {
            metadata: true,
            extended: true,
            long_ads: true,
            ..Opts::default()
        },
    ] {
        let image = build(&tree(), &opts);
        // Every 7th byte of every populated 2 KiB sector, set to each of
        // three values.
        let mut touched = 0usize;
        for sector in 0..image.len() / 2048 {
            let base = sector * 2048;
            if image[base..base + 2048].iter().all(|&b| b == 0) {
                continue;
            }
            for at in (base..base + 2048).step_by(7) {
                for value in [0x00u8, 0xFF, 0x80] {
                    if image[at] == value {
                        continue;
                    }
                    let mut dmg = image.clone();
                    dmg[at] = value;
                    touched += 1;
                    if let Ok(fs) = mount(Arc::new(MemSource::new(dmg))) {
                        let mut budget = 400;
                        walk(&fs, fs.root(), 0, &mut budget);
                    }
                }
            }
        }
        assert!(touched > 1000);
    }
}

#[test]
fn truncated_images_fail_cleanly() {
    let image = build(&tree(), &Opts::default());
    for len in [0, 100, 32768, 70000, 524288, 524288 + 2048, image.len() - 1] {
        let cut = image[..len.min(image.len())].to_vec();
        if let Ok(fs) = mount(Arc::new(MemSource::new(cut))) {
            let mut budget = 400;
            walk(&fs, fs.root(), 0, &mut budget);
        }
    }
}

#[test]
fn virtual_partition_is_named_unsupported() {
    let mut image = build(
        &tree(),
        &Opts {
            metadata: true,
            ..Opts::default()
        },
    );
    // The main logical volume descriptor is the third sector of the main
    // sequence at byte 65536.
    let at = 65536 + 2 * 2048;
    let id = b"*UDF Virtual Partition\0";
    image[at + 446 + 5..at + 446 + 5 + id.len()].copy_from_slice(id);
    let mut body = image[at..at + 510].to_vec();
    seal(&mut body, 6, u32::try_from(at / 2048).unwrap());
    image[at..at + 510].copy_from_slice(&body);
    let Err(err) = mount(Arc::new(MemSource::new(image))) else {
        panic!("a virtual partition mounted");
    };
    assert!(
        matches!(&err, Error::Unsupported(what) if what.contains("virtual")),
        "{err}"
    );
}

#[test]
fn falls_back_to_the_reserve_sequence() {
    let mut image = build(&tree(), &Opts::default());
    // Wreck the main sequence's logical volume descriptor.
    let at = 65536 + 2 * 2048;
    image[at + 20] ^= 0xFF;
    check(&mounted(image));
}

#[test]
fn bad_anchor_is_not_a_udf_volume() {
    let mut image = build(&tree(), &Opts::default());
    image[256 * 2048 + 4] ^= 1;
    assert!(mount(Arc::new(MemSource::new(image))).is_err());
}

#[test]
fn timestamps_use_their_zone() {
    let mut t = [0u8; 12];
    t[..2].copy_from_slice(&(0x1000u16 | 60).to_le_bytes());
    t[2..4].copy_from_slice(&2024i16.to_le_bytes());
    t[4] = 3;
    t[5] = 5;
    t[6] = 10;
    t[7] = 20;
    t[8] = 30;
    assert_eq!(timestamp(&t).unwrap().unix_seconds(), MTIME_UNIX);
    // A zone of -05:00 is 0x1000 - 300 in 12 bits.
    t[..2].copy_from_slice(&(0x1000u16 | (0x1000 - 300)).to_le_bytes());
    assert_eq!(timestamp(&t).unwrap().unix_seconds(), MTIME_UNIX + 6 * 3600);
    t[4] = 13;
    assert!(timestamp(&t).is_none());
}
