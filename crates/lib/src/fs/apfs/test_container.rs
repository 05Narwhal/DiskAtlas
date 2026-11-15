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

//! Builds APFS containers in memory, with correct checksums, for tests and
//! fuzz seeds (design §12). Test code: it panics on misuse.

use std::sync::Arc;

use super::btree::{INFO_LEN, Layout};
use super::container::Blocks;
use super::object::{
    OBJ_EPHEMERAL, OBJ_PHYSICAL, TYPE_BTREE, TYPE_BTREE_NODE, TYPE_CHECKPOINT_MAP, TYPE_FS,
    TYPE_FSTREE, TYPE_NX_SUPERBLOCK, TYPE_OMAP, TYPE_OMAP_TREE, TYPE_SPACEMAN, seal,
};
use crate::io::block::MemSource;

/// Block size of every built container.
pub(crate) const BS: usize = 4096;
/// The container superblock's oid.
const NX_OID: u64 = 1;
/// Ephemeral oid of the space manager.
pub(crate) const SPACEMAN_OID: u64 = 0x400;
/// First virtual oid handed out.
const FIRST_VIRTUAL_OID: u64 = 1026;
/// Transaction every virtual mapping is made at.
pub(crate) const MAP_XID: u64 = 1;
/// The container UUID.
pub(crate) const NX_UUID: [u8; 16] = *b"disk-atlas-apfs!";

/// Blocks of an image under construction.
#[derive(Default)]
pub(crate) struct Image {
    blocks: Vec<Vec<u8>>,
}

impl Image {
    /// The next free block, zeroed.
    pub fn alloc(&mut self) -> u64 {
        self.blocks.push(vec![0u8; BS]);
        (self.blocks.len() - 1) as u64
    }

    /// Replaces block `paddr`, which must be allocated.
    pub fn put(&mut self, paddr: u64, block: Vec<u8>) {
        assert_eq!(block.len(), BS);
        self.blocks[paddr as usize] = block;
    }

    /// The image as bytes.
    pub fn bytes(&self) -> Vec<u8> {
        self.blocks.concat()
    }
}

fn put_u16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}
fn put_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_u64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// Writes an object header (checksum left for [`seal`]).
pub(crate) fn header(b: &mut [u8], oid: u64, xid: u64, type_and_flags: u32, subtype: u16) {
    put_u64(b, 8, oid);
    put_u64(b, 16, xid);
    put_u32(b, 24, type_and_flags);
    put_u32(b, 28, u32::from(subtype));
}

/// Key and value pairs.
pub(crate) type Records = Vec<(Vec<u8>, Vec<u8>)>;

/// How [`write_tree`] lays out a tree.
#[derive(Clone, Copy)]
pub(crate) struct NodeOpts {
    pub layout: Layout,
    /// Most records per node, so small inputs still give deep trees.
    pub per_node: usize,
    pub subtype: u16,
    /// Children are block addresses (else virtual oids).
    pub physical: bool,
    /// Nodes carry object headers.
    pub headers: bool,
    pub xid: u64,
}

impl NodeOpts {
    /// An object map tree.
    pub fn omap(per_node: usize) -> Self {
        Self {
            layout: Layout::Fixed { key: 16, value: 16 },
            per_node,
            subtype: TYPE_OMAP_TREE,
            physical: true,
            headers: true,
            xid: MAP_XID,
        }
    }

    /// A file-system tree (virtual, variable).
    pub fn fs_tree(per_node: usize) -> Self {
        Self {
            layout: Layout::Variable,
            per_node,
            subtype: TYPE_FSTREE,
            physical: false,
            headers: true,
            xid: MAP_XID,
        }
    }
}

/// A written tree: its root and, for a virtual tree, the oid of every node
/// with where it was put.
pub(crate) struct WrittenTree {
    pub root_paddr: u64,
    pub root_oid: u64,
    pub mappings: Vec<(u64, u64)>,
}

/// Writes a tree of `records` (sorted by the caller) into `img`. Virtual
/// node oids come from `next_oid`.
pub(crate) fn write_tree(
    img: &mut Image,
    records: &[(Vec<u8>, Vec<u8>)],
    opts: NodeOpts,
    next_oid: &mut u64,
) -> WrittenTree {
    assert!(opts.per_node >= 2);
    let mut mappings = Vec::new();
    let node_count = {
        // Count nodes first, for the tree info.
        let mut n = records.len().div_ceil(opts.per_node).max(1);
        let mut total = n;
        while n > 1 {
            n = n.div_ceil(opts.per_node);
            total += n;
        }
        total as u64
    };
    let longest_key = records.iter().map(|r| r.0.len()).max().unwrap_or(0) as u32;
    let longest_value = records.iter().map(|r| r.1.len()).max().unwrap_or(0) as u32;

    // Level 0: (first key, records) per leaf.
    let mut level: Vec<(Vec<u8>, Records)> = if records.is_empty() {
        vec![(Vec::new(), Vec::new())]
    } else {
        records
            .chunks(opts.per_node)
            .map(|c| (c[0].0.clone(), c.to_vec()))
            .collect()
    };
    let mut depth: u16 = 0;
    loop {
        let is_root = level.len() == 1;
        let mut ids = Vec::new();
        for (first, entries) in &level {
            let paddr = img.alloc();
            let oid = if opts.physical {
                paddr
            } else {
                let oid = *next_oid;
                *next_oid += 1;
                mappings.push((oid, paddr));
                oid
            };
            let info = is_root.then_some(RootInfo {
                physical: opts.physical,
                key_count: records.len() as u64,
                node_count,
                longest_key,
                longest_value,
            });
            let block = node_block(oid, depth, entries, opts, info);
            img.put(paddr, block);
            ids.push((first.clone(), oid, paddr));
        }
        if is_root {
            let (_, oid, paddr) = ids.pop().unwrap();
            return WrittenTree {
                root_paddr: paddr,
                root_oid: oid,
                mappings,
            };
        }
        level = ids
            .chunks(opts.per_node)
            .map(|c| {
                let entries = c
                    .iter()
                    .map(|(k, oid, _)| (k.clone(), oid.to_le_bytes().to_vec()))
                    .collect();
                (c[0].0.clone(), entries)
            })
            .collect();
        depth += 1;
    }
}

struct RootInfo {
    physical: bool,
    key_count: u64,
    node_count: u64,
    longest_key: u32,
    longest_value: u32,
}

fn node_block(
    oid: u64,
    level: u16,
    entries: &[(Vec<u8>, Vec<u8>)],
    opts: NodeOpts,
    info: Option<RootInfo>,
) -> Vec<u8> {
    let mut b = vec![0u8; BS];
    let root = info.is_some();
    let fixed = matches!(opts.layout, Layout::Fixed { .. });
    let entry_size = if fixed { 4 } else { 8 };
    let mut flags = 0u16;
    if root {
        flags |= 1;
    }
    if level == 0 {
        flags |= 2;
    }
    if fixed {
        flags |= 4;
    }
    put_u16(&mut b, 32, flags);
    put_u16(&mut b, 34, level);
    put_u32(&mut b, 36, entries.len() as u32);
    let toc_len = entries.len() * entry_size;
    put_u16(&mut b, 40, 0);
    put_u16(&mut b, 42, toc_len as u16);
    let key_start = 56 + toc_len;
    let val_end = if root { BS - INFO_LEN } else { BS };
    let mut key_off = 0usize;
    let mut val_off = 0usize;
    for (i, (k, v)) in entries.iter().enumerate() {
        let at = 56 + i * entry_size;
        let ks = key_start + key_off;
        b[ks..ks + k.len()].copy_from_slice(k);
        val_off += v.len();
        let vs = val_end - val_off;
        assert!(ks + k.len() <= vs, "node overflows: lower per_node");
        b[vs..vs + v.len()].copy_from_slice(v);
        if fixed {
            put_u16(&mut b, at, key_off as u16);
            put_u16(&mut b, at + 2, val_off as u16);
        } else {
            put_u16(&mut b, at, key_off as u16);
            put_u16(&mut b, at + 2, k.len() as u16);
            put_u16(&mut b, at + 4, val_off as u16);
            put_u16(&mut b, at + 6, v.len() as u16);
        }
        key_off += k.len();
    }
    if let Some(info) = info {
        let at = BS - INFO_LEN;
        let (ks, vs) = match opts.layout {
            Layout::Fixed { key, value } => (u32::from(key), u32::from(value)),
            Layout::Variable => (0, 0),
        };
        let mut tree_flags = 0u32;
        if info.physical {
            tree_flags |= 0x10;
        }
        if !opts.headers {
            tree_flags |= 0x100;
        }
        put_u32(&mut b, at, tree_flags);
        put_u32(&mut b, at + 4, BS as u32);
        put_u32(&mut b, at + 8, ks);
        put_u32(&mut b, at + 12, vs);
        put_u32(&mut b, at + 16, info.longest_key);
        put_u32(&mut b, at + 20, info.longest_value);
        put_u64(&mut b, at + 24, info.key_count);
        put_u64(&mut b, at + 32, info.node_count);
    }
    if opts.headers {
        let kind = if root { TYPE_BTREE } else { TYPE_BTREE_NODE };
        let storage = if opts.physical { OBJ_PHYSICAL } else { 0 };
        header(
            &mut b,
            oid,
            opts.xid,
            u32::from(kind) | storage,
            opts.subtype,
        );
        seal(&mut b);
    } else {
        put_u16(&mut b, 32, flags | 0x10);
    }
    b
}

/// An object map key.
pub(crate) fn omap_key(oid: u64, xid: u64) -> Vec<u8> {
    let mut k = oid.to_le_bytes().to_vec();
    k.extend_from_slice(&xid.to_le_bytes());
    k
}

/// An object map value.
pub(crate) fn omap_value(flags: u32, paddr: u64) -> Vec<u8> {
    let mut v = flags.to_le_bytes().to_vec();
    v.extend_from_slice(&(BS as u32).to_le_bytes());
    v.extend_from_slice(&paddr.to_le_bytes());
    v
}

/// Writes an object map holding `entries` (oid, xid, flags, paddr), in any
/// order; returns the header's address.
pub(crate) fn write_omap(
    img: &mut Image,
    entries: &[(u64, u64, u32, u64)],
    per_node: usize,
) -> u64 {
    let mut sorted = entries.to_vec();
    sorted.sort_by_key(|e| (e.0, e.1));
    let records: Vec<_> = sorted
        .iter()
        .map(|&(oid, xid, flags, paddr)| (omap_key(oid, xid), omap_value(flags, paddr)))
        .collect();
    let paddr = img.alloc();
    let tree = write_tree(img, &records, NodeOpts::omap(per_node), &mut 0);
    let mut b = vec![0u8; BS];
    header(
        &mut b,
        paddr,
        MAP_XID,
        OBJ_PHYSICAL | u32::from(TYPE_OMAP),
        0,
    );
    put_u32(&mut b, 40, OBJ_PHYSICAL | u32::from(TYPE_BTREE));
    put_u32(&mut b, 44, OBJ_PHYSICAL | u32::from(TYPE_BTREE));
    put_u64(&mut b, 48, tree.root_paddr);
    seal(&mut b);
    img.put(paddr, b);
    paddr
}

/// Flips one byte in the middle of block `paddr`.
pub(crate) fn corrupt_block(image: &mut [u8], paddr: u64, block_size: usize) {
    let at = paddr as usize * block_size + block_size / 2;
    image[at] ^= 0x5A;
}

/// A built tree or object map, for the B-tree and object map tests.
pub(crate) struct Built {
    pub image: Vec<u8>,
    pub blocks: Arc<Blocks>,
    pub root: u64,
}

impl Built {
    /// Block access over `image`.
    pub fn blocks_over(image: Vec<u8>) -> Arc<Blocks> {
        let count = (image.len() / BS) as u64;
        Arc::new(Blocks::new(
            Arc::new(MemSource::new(image)),
            BS as u32,
            count,
        ))
    }

    /// Every byte `blocks` reads.
    pub fn image_of(blocks: &Blocks) -> Vec<u8> {
        let mut out = vec![0u8; blocks.source().len() as usize];
        blocks.source().read_at(0, &mut out).unwrap();
        out
    }
}

/// A physical tree of `records` alone in an image (block 0 left empty).
pub(crate) fn build_tree_image(records: &[(Vec<u8>, Vec<u8>)], opts: NodeOpts) -> Built {
    let mut img = Image::default();
    img.alloc();
    let tree = write_tree(&mut img, records, opts, &mut 0);
    let image = img.bytes();
    Built {
        blocks: Built::blocks_over(image.clone()),
        image,
        root: tree.root_paddr,
    }
}

/// An object map alone in an image: its blocks and header address.
pub(crate) fn omap_image(entries: &[(u64, u64, u32, u64)], per_node: usize) -> (Arc<Blocks>, u64) {
    let mut img = Image::default();
    img.alloc();
    let paddr = write_omap(&mut img, entries, per_node.max(2));
    (Built::blocks_over(img.bytes()), paddr)
}

/// One volume of a built container.
#[derive(Clone)]
pub(crate) struct VolumeSpec {
    pub name: String,
    /// `apfs_role`.
    pub role: u16,
    /// `apfs_incompatible_features`.
    pub incompatible: u64,
    /// `apfs_fs_flags` (0x1 unencrypted).
    pub fs_flags: u64,
    /// File-system tree records, sorted.
    pub records: Vec<(Vec<u8>, Vec<u8>)>,
    /// Records per file-system tree node.
    pub per_node: usize,
    pub uuid: [u8; 16],
}

impl VolumeSpec {
    /// An unencrypted, case-sensitive volume with no records.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            role: 0,
            incompatible: 0,
            fs_flags: 1,
            records: Vec::new(),
            per_node: 16,
            uuid: [0x11; 16],
        }
    }
}

/// A built container and where its parts are.
pub(crate) struct BuiltContainer {
    pub image: Vec<u8>,
    /// The newest checkpoint's transaction.
    pub xid: u64,
    /// Where each volume superblock is.
    pub volume_paddrs: Vec<u64>,
    pub spaceman_oid: u64,
    /// The ring superblock and map of each requested checkpoint, in order.
    pub checkpoint_superblocks: Vec<u64>,
    pub checkpoint_maps: Vec<u64>,
}

/// Builds a container.
pub(crate) struct ContainerBuilder {
    checkpoints: Vec<u64>,
    block_zero_xid: Option<u64>,
    volumes: Vec<VolumeSpec>,
}

impl ContainerBuilder {
    /// One checkpoint at xid 10 and one empty volume.
    pub fn new() -> Self {
        Self {
            checkpoints: vec![10],
            block_zero_xid: None,
            volumes: vec![VolumeSpec::new("Atlas")],
        }
    }

    /// Checkpoints at these transactions, in ring order.
    pub fn checkpoints(mut self, xids: &[u64]) -> Self {
        self.checkpoints = xids.to_vec();
        self
    }

    /// Block 0 as a copy of a checkpoint at this transaction. When no ring
    /// checkpoint has it, its map is in the ring and its superblock is not
    /// (the ring wrapped over it).
    pub fn block_zero_xid(mut self, xid: u64) -> Self {
        self.block_zero_xid = Some(xid);
        self
    }

    /// Replaces the volumes.
    pub fn volumes(mut self, volumes: Vec<VolumeSpec>) -> Self {
        self.volumes = volumes;
        self
    }

    pub fn build(self) -> BuiltContainer {
        let mut img = Image::default();
        img.alloc(); // block 0
        let zero_xid = self
            .block_zero_xid
            .unwrap_or(*self.checkpoints.last().expect("a checkpoint"));
        let stale = !self.checkpoints.contains(&zero_xid);
        let slots = self.checkpoints.len() + usize::from(stale);
        let desc_blocks = (2 * slots).max(8) as u32;
        let desc_base = img.alloc();
        for _ in 1..desc_blocks {
            img.alloc();
        }
        let data_blocks = 4u32;
        let data_base = img.alloc();
        for _ in 1..data_blocks {
            img.alloc();
        }
        let spaceman_paddr = data_base;
        let mut sm = vec![0u8; BS];
        header(
            &mut sm,
            SPACEMAN_OID,
            MAP_XID,
            OBJ_EPHEMERAL | u32::from(TYPE_SPACEMAN),
            0,
        );
        put_u32(&mut sm, 32, BS as u32);
        seal(&mut sm);
        img.put(spaceman_paddr, sm);

        // Volumes, then the container object map that finds them.
        let mut next_oid = FIRST_VIRTUAL_OID;
        let mut fs_oids = Vec::new();
        let mut volume_paddrs = Vec::new();
        let mut nx_map = Vec::new();
        for (index, spec) in self.volumes.iter().enumerate() {
            let oid = next_oid;
            next_oid += 1;
            let paddr = write_volume(&mut img, spec, index as u32, oid, &mut next_oid);
            fs_oids.push(oid);
            volume_paddrs.push(paddr);
            nx_map.push((oid, MAP_XID, 0, paddr));
        }
        let omap = write_omap(&mut img, &nx_map, 16);

        // The ring: checkpoint k at slots 2k (map) and 2k+1 (superblock);
        // a stale block 0's map last, with no superblock.
        let block_count = img.blocks.len() as u64;
        let nx = |xid: u64, index: u32| {
            let mut b = vec![0u8; BS];
            header(
                &mut b,
                NX_OID,
                xid,
                OBJ_EPHEMERAL | u32::from(TYPE_NX_SUPERBLOCK),
                0,
            );
            b[32..36].copy_from_slice(b"NXSB");
            put_u32(&mut b, 36, BS as u32);
            put_u64(&mut b, 40, block_count);
            put_u64(&mut b, 64, 2);
            b[72..88].copy_from_slice(&NX_UUID);
            put_u64(&mut b, 88, next_oid);
            put_u64(&mut b, 96, xid + 1);
            put_u32(&mut b, 104, desc_blocks);
            put_u32(&mut b, 108, data_blocks);
            put_u64(&mut b, 112, desc_base);
            put_u64(&mut b, 120, data_base);
            put_u32(&mut b, 136, index);
            put_u32(&mut b, 140, 2);
            put_u32(&mut b, 148, 1);
            put_u64(&mut b, 152, SPACEMAN_OID);
            put_u64(&mut b, 160, omap);
            put_u32(&mut b, 180, 100);
            for (i, oid) in fs_oids.iter().enumerate() {
                put_u64(&mut b, 184 + 8 * i, *oid);
            }
            seal(&mut b);
            b
        };
        let map = |xid: u64, paddr: u64| {
            let mut b = vec![0u8; BS];
            header(
                &mut b,
                paddr,
                xid,
                OBJ_PHYSICAL | u32::from(TYPE_CHECKPOINT_MAP),
                0,
            );
            put_u32(&mut b, 32, 1); // last
            put_u32(&mut b, 36, 1);
            put_u32(&mut b, 40, OBJ_EPHEMERAL | u32::from(TYPE_SPACEMAN));
            put_u32(&mut b, 48, BS as u32);
            put_u64(&mut b, 64, SPACEMAN_OID);
            put_u64(&mut b, 72, spaceman_paddr);
            seal(&mut b);
            b
        };
        let mut checkpoint_superblocks = Vec::new();
        let mut checkpoint_maps = Vec::new();
        let mut zero_index = 0;
        for (k, &xid) in self.checkpoints.iter().enumerate() {
            let index = 2 * k as u32;
            let map_paddr = desc_base + u64::from(index);
            let sb_paddr = map_paddr + 1;
            img.put(map_paddr, map(xid, map_paddr));
            img.put(sb_paddr, nx(xid, index));
            checkpoint_maps.push(map_paddr);
            checkpoint_superblocks.push(sb_paddr);
            if xid == zero_xid {
                zero_index = index;
            }
        }
        if stale {
            let index = 2 * self.checkpoints.len() as u32;
            let map_paddr = desc_base + u64::from(index);
            img.put(map_paddr, map(zero_xid, map_paddr));
            zero_index = index;
        }
        img.put(0, nx(zero_xid, zero_index));

        BuiltContainer {
            image: img.bytes(),
            xid: self.checkpoints.iter().copied().max().unwrap_or(zero_xid),
            volume_paddrs,
            spaceman_oid: SPACEMAN_OID,
            checkpoint_superblocks,
            checkpoint_maps,
        }
    }
}

/// Writes one volume: its file-system tree, its object map and its
/// superblock. Returns the superblock's address.
fn write_volume(
    img: &mut Image,
    spec: &VolumeSpec,
    index: u32,
    oid: u64,
    next_oid: &mut u64,
) -> u64 {
    let tree = write_tree(
        img,
        &spec.records,
        NodeOpts::fs_tree(spec.per_node),
        next_oid,
    );
    let entries: Vec<_> = tree
        .mappings
        .iter()
        .map(|&(oid, paddr)| (oid, MAP_XID, 0u32, paddr))
        .collect();
    let omap = write_omap(img, &entries, 16);
    let paddr = img.alloc();
    let mut b = vec![0u8; BS];
    header(&mut b, oid, MAP_XID, u32::from(TYPE_FS), 0);
    b[32..36].copy_from_slice(b"APSB");
    put_u32(&mut b, 36, index);
    put_u64(&mut b, 56, spec.incompatible);
    put_u32(&mut b, 116, u32::from(TYPE_BTREE));
    put_u32(&mut b, 120, OBJ_PHYSICAL | u32::from(TYPE_BTREE));
    put_u32(&mut b, 124, OBJ_PHYSICAL | u32::from(TYPE_BTREE));
    put_u64(&mut b, 128, omap);
    put_u64(&mut b, 136, tree.root_oid);
    b[240..256].copy_from_slice(&spec.uuid);
    put_u64(&mut b, 256, 1_700_000_000_000_000_000);
    put_u64(&mut b, 264, spec.fs_flags);
    let name = spec.name.as_bytes();
    b[704..704 + name.len()].copy_from_slice(name);
    put_u16(&mut b, 964, spec.role);
    seal(&mut b);
    img.put(paddr, b);
    paddr
}
