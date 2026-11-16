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

//! Builds small UDF volumes for tests and fuzz seeds.
//!
//! These are written from the specification, not from a real disc, so they
//! prove the reader agrees with the builder. Real images from other writers
//! are still wanted (`m8-formats-plugins.md` §5).

/// A piece of a file's content.
#[derive(Clone)]
pub enum Piece {
    Data(Vec<u8>),
    /// Allocated but never written: reads as zeros.
    Hole(u64),
}

/// An entry of the tree to build.
#[derive(Clone)]
pub enum Entry {
    File { name: String, pieces: Vec<Piece> },
    Dir { name: String, kids: Vec<Entry> },
    Link { name: String, target: String },
}

impl Entry {
    pub fn file(name: &str, data: &[u8]) -> Self {
        Self::File {
            name: name.to_owned(),
            pieces: vec![Piece::Data(data.to_vec())],
        }
    }

    pub fn pieces(name: &str, pieces: Vec<Piece>) -> Self {
        Self::File {
            name: name.to_owned(),
            pieces,
        }
    }

    pub fn dir(name: &str, kids: Vec<Entry>) -> Self {
        Self::Dir {
            name: name.to_owned(),
            kids,
        }
    }

    pub fn link(name: &str, target: &str) -> Self {
        Self::Link {
            name: name.to_owned(),
            target: target.to_owned(),
        }
    }
}

/// What to build.
#[derive(Clone)]
pub struct Opts {
    /// Sector size: 512, 2048 or 4096. The block size is the same.
    pub sector: usize,
    /// Put file entries and folders in a metadata partition (UDF 2.50).
    pub metadata: bool,
    /// Extended file entries instead of file entries.
    pub extended: bool,
    /// Long allocation descriptors instead of short ones.
    pub long_ads: bool,
    /// Embed content of up to this many bytes in the file entry.
    pub embed: usize,
    /// Blocks reserved for the metadata file.
    pub meta_blocks: u32,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            sector: 2048,
            metadata: false,
            extended: false,
            long_ads: false,
            embed: 0,
            meta_blocks: 64,
        }
    }
}

/// 2024-03-05 10:20:30 at +01:00, as Unix seconds in UTC.
pub const MTIME_UNIX: i64 = 1_709_630_430;
const NO_BLOCK: u32 = u32::MAX;

fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn put64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &byte in data {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Fills the tag of `body` (its first 16 bytes) for a descriptor at
/// `location`.
pub fn seal(body: &mut [u8], id: u16, location: u32) {
    let crc_len = body.len() - 16;
    put16(body, 0, id);
    put16(body, 2, 2);
    put16(body, 6, 1);
    put16(body, 8, crc16(&body[16..]));
    put16(body, 10, u16::try_from(crc_len).unwrap());
    put32(body, 12, location);
    let sum = body[..16]
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 4)
        .fold(0u8, |a, (_, b)| a.wrapping_add(*b));
    body[4] = sum;
}

fn dstring(text: &str, field: usize) -> Vec<u8> {
    let mut out = vec![0u8; field];
    out[0] = 8;
    for (i, c) in text.chars().enumerate() {
        out[1 + i] = u8::try_from(u32::from(c)).unwrap();
    }
    out[field - 1] = u8::try_from(1 + text.chars().count()).unwrap();
    out
}

/// OSTA compressed name: 8-bit when it fits, else 16-bit.
fn osta(name: &str) -> Vec<u8> {
    if name.chars().all(|c| u32::from(c) < 256) {
        let mut out = vec![8u8];
        out.extend(name.chars().map(|c| u8::try_from(u32::from(c)).unwrap()));
        out
    } else {
        let mut out = vec![16u8];
        for unit in name.encode_utf16() {
            out.extend_from_slice(&unit.to_be_bytes());
        }
        out
    }
}

fn timestamp() -> [u8; 12] {
    let mut t = [0u8; 12];
    t[..2].copy_from_slice(&(0x1000u16 | 60).to_le_bytes());
    t[2..4].copy_from_slice(&2024i16.to_le_bytes());
    t[4] = 3;
    t[5] = 5;
    t[6] = 10;
    t[7] = 20;
    t[8] = 30;
    t
}

struct Builder {
    img: Vec<u8>,
    bs: usize,
    opts: Opts,
    part_start: usize,
    meta_next: u32,
    data_next: u32,
}

#[derive(Clone, Copy)]
struct At {
    part: u16,
    block: u32,
}

impl Builder {
    fn meta_ref(&self) -> u16 {
        u16::from(self.opts.metadata)
    }

    fn put(&mut self, at: usize, bytes: &[u8]) {
        if self.img.len() < at + bytes.len() {
            self.img.resize(at + bytes.len(), 0);
        }
        self.img[at..at + bytes.len()].copy_from_slice(bytes);
    }

    /// Reserves `n` blocks of the space that holds metadata.
    fn alloc_meta(&mut self, n: u32) -> u32 {
        if self.opts.metadata {
            let at = self.meta_next;
            self.meta_next += n;
            assert!(
                self.meta_next <= self.opts.meta_blocks,
                "metadata area full"
            );
            at
        } else {
            self.alloc_data(n)
        }
    }

    fn alloc_data(&mut self, n: u32) -> u32 {
        let at = self.data_next;
        self.data_next += n;
        at
    }

    fn physical(&self, meta_block: u32) -> usize {
        let block = if self.opts.metadata {
            1 + meta_block
        } else {
            meta_block
        };
        self.part_start + block as usize * self.bs
    }

    fn write_meta(&mut self, block: u32, body: &mut [u8], id: u16) {
        seal(body, id, block);
        let at = self.physical(block);
        let bytes = body.to_vec();
        self.put(at, &bytes);
    }

    fn ad(&self, kind: u32, len: u64, at: At) -> Vec<u8> {
        let raw = (kind << 30) | u32::try_from(len).unwrap();
        let mut out = Vec::new();
        out.extend_from_slice(&raw.to_le_bytes());
        out.extend_from_slice(&at.block.to_le_bytes());
        if self.opts.long_ads {
            out.extend_from_slice(&at.part.to_le_bytes());
            out.extend_from_slice(&[0u8; 6]);
        } else {
            out.truncate(8);
        }
        out
    }

    /// Writes a file entry at `block` and returns nothing; `area` is the
    /// allocation descriptors or embedded data.
    #[allow(clippy::too_many_arguments)]
    fn file_entry(&mut self, block: u32, ftype: u8, size: u64, form: u16, area: &[u8], links: u16) {
        let header = if self.opts.extended { 216 } else { 176 };
        let mut body = vec![0u8; header + area.len()];
        body[27] = ftype;
        put16(&mut body, 34, form);
        put32(&mut body, 36, 1000);
        put32(&mut body, 40, 100);
        // Owner, group and other all read, write and execute where it makes
        // sense: rwxr-xr-- as UDF bits.
        let perm = (0b111u32 << 10) | (0b101 << 5) | 0b100;
        put32(&mut body, 44, perm);
        put16(&mut body, 48, links);
        put64(&mut body, 56, size);
        let t = timestamp();
        let (ea, times): (usize, &[usize]) = if self.opts.extended {
            (208, &[80, 92, 104, 116])
        } else {
            (168, &[72, 84, 96])
        };
        for &at in times {
            body[at..at + 12].copy_from_slice(&t);
        }
        put32(&mut body, ea + 4, u32::try_from(area.len()).unwrap());
        body[header..].copy_from_slice(area);
        let id = if self.opts.extended { 266 } else { 261 };
        self.write_meta(block, &mut body, id);
    }

    /// Stores `data` as a folder or link body: embedded, or in blocks of the
    /// metadata space. Returns (form flag, area).
    fn body_area(&mut self, data: &[u8]) -> (u16, Vec<u8>) {
        if data.len() <= self.opts.embed {
            return (3, data.to_vec());
        }
        if data.is_empty() {
            return (u16::from(self.opts.long_ads), Vec::new());
        }
        let blocks = u32::try_from(data.len().div_ceil(self.bs)).unwrap();
        let first = self.alloc_meta(blocks);
        for i in 0..blocks as usize {
            let from = i * self.bs;
            let to = (from + self.bs).min(data.len());
            let at = self.physical(first) + i * self.bs;
            self.put(at, &data[from..to]);
        }
        let at = At {
            part: self.meta_ref(),
            block: first,
        };
        (
            u16::from(self.opts.long_ads),
            self.ad(0, data.len() as u64, at),
        )
    }

    fn fid(&self, name: &str, dir: bool, parent: bool, at: At) -> Vec<u8> {
        let ident = if parent { Vec::new() } else { osta(name) };
        let total = (38 + ident.len() + 3) & !3;
        let mut out = vec![0u8; total];
        out[18] = if parent {
            0x0A
        } else if dir {
            0x02
        } else {
            0
        };
        out[19] = u8::try_from(ident.len()).unwrap();
        put32(&mut out, 20, u32::try_from(self.bs).unwrap());
        put32(&mut out, 24, at.block);
        put16(&mut out, 28, at.part);
        out[38..38 + ident.len()].copy_from_slice(&ident);
        seal(&mut out, 257, 0);
        out
    }

    /// Builds `entry`; returns where its file entry went.
    fn emit(&mut self, entry: &Entry, parent: At) -> At {
        let me = At {
            part: self.meta_ref(),
            block: self.alloc_meta(1),
        };
        match entry {
            Entry::Dir { kids, .. } => {
                let mut fids = self.fid("", true, true, parent);
                let mut subdirs = 0u16;
                for kid in kids {
                    let at = self.emit(kid, me);
                    let (name, dir) = match kid {
                        Entry::Dir { name, .. } => (name, true),
                        Entry::File { name, .. } | Entry::Link { name, .. } => (name, false),
                    };
                    subdirs += u16::from(dir);
                    let fid = self.fid(name, dir, false, at);
                    fids.extend_from_slice(&fid);
                }
                let (form, area) = self.body_area(&fids);
                self.file_entry(me.block, 4, fids.len() as u64, form, &area, 1 + subdirs);
            }
            Entry::Link { target, .. } => {
                let mut data = Vec::new();
                if let Some(rest) = target.strip_prefix('/') {
                    data.extend_from_slice(&[2, 0, 0, 0]);
                    self.components(rest, &mut data);
                } else {
                    self.components(target, &mut data);
                }
                let (form, area) = self.body_area(&data);
                self.file_entry(me.block, 12, data.len() as u64, form, &area, 1);
            }
            Entry::File { pieces, .. } => {
                let size: u64 = pieces
                    .iter()
                    .map(|p| match p {
                        Piece::Data(d) => d.len() as u64,
                        Piece::Hole(n) => *n,
                    })
                    .sum();
                let single = match pieces.as_slice() {
                    [Piece::Data(d)] if d.len() <= self.opts.embed => Some(d.clone()),
                    _ => None,
                };
                if let Some(data) = single {
                    self.file_entry(me.block, 5, size, 3, &data, 1);
                } else {
                    let mut area = Vec::new();
                    for piece in pieces {
                        match piece {
                            Piece::Data(d) if !d.is_empty() => {
                                let blocks = u32::try_from(d.len().div_ceil(self.bs)).unwrap();
                                let first = self.alloc_data(blocks);
                                let at = self.part_start + first as usize * self.bs;
                                self.put(at, d);
                                let ad = self.ad(
                                    0,
                                    d.len() as u64,
                                    At {
                                        part: 0,
                                        block: first,
                                    },
                                );
                                area.extend_from_slice(&ad);
                            }
                            Piece::Data(_) => {}
                            Piece::Hole(n) => {
                                let ad = self.ad(1, *n, At { part: 0, block: 0 });
                                area.extend_from_slice(&ad);
                            }
                        }
                    }
                    self.file_entry(me.block, 5, size, u16::from(self.opts.long_ads), &area, 1);
                }
            }
        }
        me
    }

    fn components(&self, path: &str, out: &mut Vec<u8>) {
        for part in path.split('/').filter(|p| !p.is_empty()) {
            match part {
                ".." => out.extend_from_slice(&[3, 0, 0, 0]),
                "." => out.extend_from_slice(&[4, 0, 0, 0]),
                name => {
                    let id = osta(name);
                    out.extend_from_slice(&[5, u8::try_from(id.len()).unwrap(), 0, 0]);
                    out.extend_from_slice(&id);
                }
            }
        }
    }
}

/// Builds a volume holding `tree` under the root.
pub fn build(tree: &[Entry], opts: &Opts) -> Vec<u8> {
    let bs = opts.sector;
    let anchor = 256usize;
    let part_sector = anchor + 1;
    let mut b = Builder {
        img: Vec::new(),
        bs,
        opts: opts.clone(),
        part_start: part_sector * bs,
        meta_next: 0,
        data_next: if opts.metadata {
            1 + opts.meta_blocks
        } else {
            0
        },
    };
    // The volume recognition sequence, always in 2048-byte steps.
    for (i, id) in [b"BEA01", b"NSR02", b"TEA01"].iter().enumerate() {
        let mut d = vec![0u8; 2048];
        d[1..6].copy_from_slice(*id);
        d[6] = 1;
        b.put(32768 + i * 2048, &d);
    }
    // The file set descriptor is the first thing in the metadata space.
    let fsd_block = b.alloc_meta(1);
    // The root's own file entry is allocated by `emit`; its parent is itself,
    // which is the next block after the file set descriptor.
    let root_at = At {
        part: b.meta_ref(),
        block: fsd_block + 1,
    };
    let root = Entry::dir("", tree.to_vec());
    let got = b.emit(&root, root_at);
    assert_eq!(got.block, root_at.block);
    // The file set descriptor.
    let mut fsd = vec![0u8; 512];
    put32(&mut fsd, 400, u32::try_from(bs).unwrap());
    put32(&mut fsd, 404, root_at.block);
    put16(&mut fsd, 408, root_at.part);
    b.write_meta(fsd_block, &mut fsd, 256);
    if opts.metadata {
        // The metadata file's entry sits at block 0 of the physical partition.
        let mut ad = Vec::new();
        ad.extend_from_slice(&(opts.meta_blocks * u32::try_from(bs).unwrap()).to_le_bytes());
        ad.extend_from_slice(&1u32.to_le_bytes());
        let header = if opts.extended { 216 } else { 176 };
        let mut body = vec![0u8; header + ad.len()];
        body[27] = 250;
        put64(&mut body, 56, u64::from(opts.meta_blocks) * bs as u64);
        let ea = if opts.extended { 208 } else { 168 };
        put32(&mut body, ea + 4, 8);
        body[header..].copy_from_slice(&ad);
        seal(&mut body, if opts.extended { 266 } else { 261 }, 0);
        let at = b.part_start;
        b.put(at, &body);
    }
    let blocks = b.data_next.max(1);
    // Descriptors.
    let main_sector = 65536 / bs;
    let reserve_sector = 98304 / bs;
    for (base, seqs) in [(main_sector, 2u32), (reserve_sector, 1)] {
        let mut pvd = vec![0u8; 512];
        put32(&mut pvd, 16, seqs);
        pvd[24..56].copy_from_slice(&dstring("TESTPVD", 32));
        seal(&mut pvd, 1, u32::try_from(base).unwrap());
        let mut pd = vec![0u8; 512];
        put32(&mut pd, 16, seqs);
        put16(&mut pd, 22, 0);
        put32(&mut pd, 188, u32::try_from(part_sector).unwrap());
        put32(&mut pd, 192, blocks);
        seal(&mut pd, 5, u32::try_from(base + 1).unwrap());
        let mut lvd = vec![0u8; 440 + 6 + if opts.metadata { 64 } else { 0 }];
        put32(&mut lvd, 16, seqs);
        lvd[84..212].copy_from_slice(&dstring("Test Volume", 128));
        put32(&mut lvd, 212, u32::try_from(bs).unwrap());
        put32(&mut lvd, 248, u32::try_from(bs).unwrap());
        put32(&mut lvd, 252, fsd_block);
        put16(&mut lvd, 256, b.meta_ref());
        let table = u32::try_from(lvd.len() - 440).unwrap();
        put32(&mut lvd, 264, table);
        put32(&mut lvd, 268, if opts.metadata { 2 } else { 1 });
        lvd[440] = 1;
        lvd[441] = 6;
        if opts.metadata {
            let m = 446;
            lvd[m] = 2;
            lvd[m + 1] = 64;
            let id = b"*UDF Metadata Partition";
            lvd[m + 5..m + 5 + id.len()].copy_from_slice(id);
            put32(&mut lvd, m + 40, 0);
            put32(&mut lvd, m + 44, NO_BLOCK);
        }
        seal(&mut lvd, 6, u32::try_from(base + 2).unwrap());
        let mut td = vec![0u8; 512];
        seal(&mut td, 8, u32::try_from(base + 3).unwrap());
        for (i, d) in [pvd, pd, lvd, td].iter().enumerate() {
            b.put((base + i) * bs, d);
        }
    }
    let mut avdp = vec![0u8; 512];
    put32(&mut avdp, 16, u32::try_from(4 * bs).unwrap());
    put32(&mut avdp, 20, u32::try_from(main_sector).unwrap());
    put32(&mut avdp, 24, u32::try_from(4 * bs).unwrap());
    put32(&mut avdp, 28, u32::try_from(reserve_sector).unwrap());
    seal(&mut avdp, 2, u32::try_from(anchor).unwrap());
    b.put(anchor * bs, &avdp);
    let end = b.part_start + blocks as usize * bs;
    if b.img.len() < end {
        b.img.resize(end, 0);
    }
    b.img
}
