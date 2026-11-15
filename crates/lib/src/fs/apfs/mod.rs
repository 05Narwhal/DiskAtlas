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

//! APFS, read-only (little-endian). Design: `.claude/docs/design/m7-apfs.md`.
//!
//! A [`container::Container`] is one per partition: the superblock of the
//! newest valid checkpoint, the container object map and a shared node
//! cache. Each volume holds the container behind an `Arc`.

use std::sync::Arc;

use crate::error::{CorruptKind, Corruption, Error, Location, Result};
use crate::fs::probe::{ContainerVolume, VolumeRole, VolumeState};
use crate::io::block::BlockSource;

pub(crate) mod btree;
pub(crate) mod bytes;
pub(crate) mod container;
#[cfg(test)]
mod fuzz_seeds;
pub(crate) mod object;
pub(crate) mod omap;
pub(crate) mod records;
pub(crate) mod spaceman;
#[cfg(test)]
pub(crate) mod test_container;
pub mod volume;

pub use volume::Volume;

use container::Container;
use object::TYPE_FS;
use volume::ApfsSuperblock;

/// Opens the APFS container in `source` and every volume in it, in the
/// container's slot order.
///
/// Fails only when the container itself cannot be opened; a volume that
/// cannot be read is returned as [`VolumeState::Failed`] beside the others.
pub fn mount(source: Arc<dyn BlockSource>) -> Result<Vec<ContainerVolume>> {
    let container = Container::open(source)?;
    Ok(container
        .superblock
        .fs_oids
        .iter()
        .map(|&oid| open_slot(&container, oid))
        .collect())
}

/// The volume superblock named by the virtual oid `oid`, as of the
/// container's checkpoint.
fn read_superblock(container: &Container, oid: u64) -> Result<ApfsSuperblock> {
    let mapping = container
        .omap
        .lookup(&container.blocks, oid, container.xid())?
        .ok_or_else(|| {
            Error::Corrupt(Corruption::new(
                "container object map",
                Location::Object(oid),
                CorruptKind::Other("volume is not in the container's object map"),
            ))
        })?;
    let block = container.blocks.block(mapping.paddr)?;
    let header = object::verify(
        &block,
        mapping.paddr,
        "volume superblock",
        &[TYPE_FS],
        Some(oid),
    )?;
    ApfsSuperblock::parse(&block, mapping.paddr, header.xid)
}

fn open_slot(container: &Arc<Container>, oid: u64) -> ContainerVolume {
    let sb = match read_superblock(container, oid) {
        Ok(sb) => sb,
        Err(e) => return ContainerVolume::new("", None, VolumeState::Failed(e)),
    };
    let name = sb.name.clone();
    let role = role_of(sb.role);
    let state = match Volume::open(Arc::clone(container), sb, container.xid()) {
        Ok(volume) => VolumeState::Ready(Arc::new(volume)),
        Err(e) => VolumeState::Failed(e),
    };
    ContainerVolume::new(name, role, state)
}

/// The sidebar role of `apfs_role` (§7): user and data are both Data; a
/// role without a variant is `None`.
fn role_of(role: u16) -> Option<VolumeRole> {
    match role {
        0x1 => Some(VolumeRole::System),
        0x2 | 0x40 => Some(VolumeRole::Data),
        0x4 => Some(VolumeRole::Recovery),
        0x8 => Some(VolumeRole::Vm),
        0x10 => Some(VolumeRole::Preboot),
        _ => None,
    }
}

/// Entry points for the fuzz targets in `fuzz/`. Not an API: they exist so
/// the targets can reach parsers that are private to the engine.
#[doc(hidden)]
pub mod fuzzing {
    use std::sync::Arc;

    use super::btree::{Layout, Node, NodeSpec};
    use super::container::Container;
    use crate::io::block::MemSource;

    /// Parses `data` as a B-tree node of each layout and reads every entry
    /// of the ones that parse. Errors are fine; a panic is the bug.
    pub fn btree_node(data: &[u8]) {
        for layout in [Layout::Variable, Layout::Fixed { key: 16, value: 16 }] {
            let spec = NodeSpec {
                oid: None,
                headers: true,
                layout,
                subtype: 0,
                structure: "fuzzed node",
            };
            if let Ok(node) = Node::parse(data.to_vec().into_boxed_slice(), 0, &spec) {
                for i in 0..node.len() {
                    let _ = (node.key(i).len(), node.value(i).map(<[u8]>::len));
                }
            }
        }
    }

    /// Opens the container in `data` and resolves every volume through its
    /// object map. Errors are fine; a panic is the bug.
    pub fn container(data: &[u8]) {
        let Ok(container) = Container::open(Arc::new(MemSource::new(data))) else {
            return;
        };
        let _ = container.ephemeral(container.superblock.spaceman_oid);
        for &oid in &container.superblock.fs_oids {
            let _ = container
                .omap
                .lookup(&container.blocks, oid, container.xid());
        }
    }
}
