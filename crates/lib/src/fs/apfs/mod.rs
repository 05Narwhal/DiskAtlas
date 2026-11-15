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

// Allowed until `mount` uses these (task 7).
#![allow(dead_code)]

pub(crate) mod btree;
pub(crate) mod bytes;
pub(crate) mod container;
#[cfg(test)]
mod fuzz_seeds;
pub(crate) mod object;
pub(crate) mod omap;
#[cfg(test)]
pub(crate) mod test_container;

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
