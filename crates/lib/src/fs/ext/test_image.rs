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

//! Builds a small ext2 image in memory: 1 KiB blocks, one block group,
//! 128-byte inodes, block maps (direct and single indirect), fast and slow
//! symbolic links. Enough to exercise the reader without `mkfs`.

pub const BLOCK: usize = 1024;
/// Most blocks one file may use here: 12 direct and one indirect block.
pub const MAX_FILE_BLOCKS: usize = 12 + BLOCK / 4;
/// Modification time of everything built, in Unix seconds.
pub const MTIME: u32 = 1_700_000_000;
pub const LABEL: &[u8] = b"TESTEXT";
pub const UUID: [u8; 16] = *b"0123456789abcdef";

const INODE_SIZE: usize = 128;
const INODES: usize = 32;
const TABLE_BLOCKS: usize = INODES * INODE_SIZE / BLOCK;
const FIRST_TABLE: usize = 5;
const FIRST_DATA: usize = FIRST_TABLE + TABLE_BLOCKS;
const TOTAL_BLOCKS: usize = 160;

pub enum Item {
    File(&'static str, Vec<u8>),
    Dir(&'static str, Vec<Item>),
    Link(&'static str, &'static str),
}

struct Builder {
    image: Vec<u8>,
    next_block: usize,
    next_inode: u32,
}

fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

impl Builder {
    fn block(&mut self) -> usize {
        let b = self.next_block;
        self.next_block += 1;
        assert!(b < TOTAL_BLOCKS, "test image out of blocks");
        b
    }

    fn write_inode(&mut self, ino: u32, mode: u16, size: u32, blocks512: u32, block: &[u8; 60]) {
        let at = FIRST_TABLE * BLOCK + (ino as usize - 1) * INODE_SIZE;
        let i = &mut self.image[at..at + INODE_SIZE];
        put16(i, 0, mode);
        put16(i, 2, 1000);
        put32(i, 4, size);
        put32(i, 8, MTIME);
        put32(i, 12, MTIME);
        put32(i, 16, MTIME);
        put16(i, 24, 100);
        put16(i, 26, 1);
        put32(i, 28, blocks512);
        i[40..100].copy_from_slice(block);
    }

    /// Stores `data` in blocks; returns the inode's block array and the
    /// count of 512-byte units.
    fn store(&mut self, data: &[u8]) -> ([u8; 60], u32) {
        let mut map = [0u8; 60];
        let count = data.len().div_ceil(BLOCK);
        assert!(count <= MAX_FILE_BLOCKS, "test file too large");
        let mut numbers = Vec::new();
        for chunk in data.chunks(BLOCK) {
            let b = self.block();
            self.image[b * BLOCK..b * BLOCK + chunk.len()].copy_from_slice(chunk);
            numbers.push(b as u32);
        }
        let mut units = numbers.len();
        for (k, n) in numbers.iter().take(12).enumerate() {
            put32(&mut map, k * 4, *n);
        }
        if numbers.len() > 12 {
            let ind = self.block();
            units += 1;
            for (k, n) in numbers[12..].iter().enumerate() {
                put32(&mut self.image, ind * BLOCK + k * 4, *n);
            }
            put32(&mut map, 12 * 4, ind as u32);
        }
        (map, u32::try_from(units * (BLOCK / 512)).unwrap())
    }

    fn dir(&mut self, ino: u32, parent: u32, items: &[Item]) {
        let mut entries: Vec<(u32, u8, Vec<u8>)> =
            vec![(ino, 2, b".".to_vec()), (parent, 2, b"..".to_vec())];
        for item in items {
            let (name, kind, child) = match item {
                Item::File(name, data) => {
                    let n = self.alloc_inode();
                    let (map, units) = self.store(data);
                    self.write_inode(n, 0o100644, data.len() as u32, units, &map);
                    (*name, 1u8, n)
                }
                Item::Dir(name, sub) => {
                    let n = self.alloc_inode();
                    self.dir(n, ino, sub);
                    (*name, 2u8, n)
                }
                Item::Link(name, target) => {
                    let n = self.alloc_inode();
                    let bytes = target.as_bytes();
                    if bytes.len() < 60 {
                        let mut map = [0u8; 60];
                        map[..bytes.len()].copy_from_slice(bytes);
                        self.write_inode(n, 0o120777, bytes.len() as u32, 0, &map);
                    } else {
                        let (map, units) = self.store(bytes);
                        self.write_inode(n, 0o120777, bytes.len() as u32, units, &map);
                    }
                    (*name, 7u8, n)
                }
            };
            entries.push((child, kind, name.as_bytes().to_vec()));
        }
        // Lay the entries into 1 KiB blocks; the last in a block takes the rest.
        let mut blocks: Vec<Vec<u8>> = vec![Vec::new()];
        let mut starts: Vec<usize> = Vec::new();
        for (child, kind, name) in &entries {
            let len = (8 + name.len()).next_multiple_of(4);
            if blocks.last().unwrap().len() + len > BLOCK {
                blocks.push(Vec::new());
            }
            let block = blocks.last_mut().unwrap();
            starts.push(block.len());
            block.extend_from_slice(&child.to_le_bytes());
            block.extend_from_slice(&(len as u16).to_le_bytes());
            block.push(name.len() as u8);
            block.push(*kind);
            block.extend_from_slice(name);
            block.resize(block.len() + len - 8 - name.len(), 0);
        }
        let mut data = Vec::new();
        for block in &mut blocks {
            // Stretch the last record to the end of the block.
            let mut at = 0;
            let mut last = 0;
            while at < block.len() {
                last = at;
                at += usize::from(u16::from_le_bytes([block[at + 4], block[at + 5]]));
            }
            let stretch = BLOCK - last;
            block[last + 4..last + 6].copy_from_slice(&(stretch as u16).to_le_bytes());
            block.resize(BLOCK, 0);
            data.extend_from_slice(block);
        }
        let (map, units) = self.store(&data);
        self.write_inode(ino, 0o040755, data.len() as u32, units, &map);
    }

    fn alloc_inode(&mut self) -> u32 {
        let n = self.next_inode;
        self.next_inode += 1;
        assert!(n as usize <= INODES, "test image out of inodes");
        n
    }
}

/// An ext2 image holding `items` in its root folder.
pub fn build(items: &[Item]) -> Vec<u8> {
    let mut b = Builder {
        image: vec![0u8; TOTAL_BLOCKS * BLOCK],
        next_block: FIRST_DATA,
        next_inode: 11,
    };
    b.dir(2, 2, items);
    let used_blocks = b.next_block;
    let used_inodes = b.next_inode - 1;
    let sb = &mut b.image[1024..2048];
    put32(sb, 0x00, INODES as u32);
    put32(sb, 0x04, TOTAL_BLOCKS as u32);
    put32(sb, 0x0C, (TOTAL_BLOCKS - used_blocks) as u32);
    put32(sb, 0x10, INODES as u32 - used_inodes);
    put32(sb, 0x14, 1);
    put32(sb, 0x18, 0);
    put32(sb, 0x1C, 0);
    put32(sb, 0x20, 8192);
    put32(sb, 0x24, 8192);
    put32(sb, 0x28, INODES as u32);
    put16(sb, 0x38, 0xEF53);
    put16(sb, 0x3A, 1);
    put32(sb, 0x4C, 1);
    put32(sb, 0x54, 11);
    put16(sb, 0x58, INODE_SIZE as u16);
    put32(sb, 0x60, 0x2);
    sb[0x68..0x78].copy_from_slice(&UUID);
    sb[0x78..0x78 + LABEL.len()].copy_from_slice(LABEL);
    let gd = &mut b.image[2 * BLOCK..2 * BLOCK + 32];
    put32(gd, 0, 3);
    put32(gd, 4, 4);
    put32(gd, 8, FIRST_TABLE as u32);
    put16(gd, 12, (TOTAL_BLOCKS - used_blocks) as u16);
    put16(gd, 14, (INODES as u32 - used_inodes) as u16);
    put16(gd, 16, 1);
    b.image
}
