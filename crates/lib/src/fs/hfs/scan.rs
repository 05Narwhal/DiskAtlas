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

//! The volume scan for the search index: one front-to-back read of the
//! catalog's leaves ([`BTree::scan_leaves`](crate::fs::hfsplus::btree::BTree::scan_leaves)).
//!
//! Classic HFS has no links, so every folder and file record is reported as
//! it is met, with its parent taken from its key. Thread records carry no
//! entry of their own and are passed over.

use std::ops::ControlFlow;

use super::catalog::{Catalog, CatalogKey, CatalogRecord, ROOT_FOLDER_ID, ROOT_PARENT_ID};
use crate::error::Result;
use crate::fs::hfsplus::btree::Node;
use crate::fs::hfsplus::catalog::entry_flags;
use crate::fs::hfsplus::volume_header::hfs_time;
use crate::fs::{NodeId, NodeKind, ScanEntry, ScanSummary};

/// Record type bytes of the two thread records.
const FOLDER_THREAD: u8 = 3;
const FILE_THREAD: u8 = 4;

/// [`FileSystem::scan`](crate::fs::FileSystem::scan) for classic HFS.
///
/// A record whose key or data does not parse, and every node the tree reader
/// skips, counts as one skipped structure.
pub(super) fn scan(
    catalog: &Catalog,
    visit: &mut dyn FnMut(ScanEntry<'_>) -> ControlFlow<()>,
) -> Result<ScanSummary> {
    let encoding = catalog.encoding();
    let mut summary = ScanSummary::default();
    // The current record's display name, reused.
    let mut name = String::new();
    let leaves = catalog.tree().scan_leaves(&mut |node: &Node| {
        // Bounded by the node's record count.
        for index in 0..node.len() {
            let Ok(rec) = node.record(index) else {
                summary.skipped = summary.skipped.saturating_add(1);
                continue;
            };
            if matches!(rec.data.first(), Some(&(FOLDER_THREAD | FILE_THREAD))) {
                continue;
            }
            let (Some(key), Ok(record)) =
                (CatalogKey::parse(rec.key), CatalogRecord::parse(rec.data))
            else {
                summary.skipped = summary.skipped.saturating_add(1);
                continue;
            };
            // The root folder (keyed by its parent, 1) is never reported.
            if key.parent_id == ROOT_PARENT_ID {
                continue;
            }
            let (id, kind, size, resource_size, modified, finder_flags, finder_type) = match &record
            {
                CatalogRecord::Folder(f) => (
                    f.folder_id,
                    NodeKind::Directory,
                    None,
                    None,
                    f.modified,
                    f.finder_flags(),
                    None,
                ),
                CatalogRecord::File(f) => {
                    let ty = f.file_type();
                    (
                        f.file_id,
                        NodeKind::File,
                        Some(u64::from(f.data_logical)),
                        Some(u64::from(f.rsrc_logical)),
                        f.modified,
                        f.finder_flags(),
                        (ty != [0; 4]).then_some(ty),
                    )
                }
                CatalogRecord::FolderThread(_) | CatalogRecord::FileThread(_) => continue,
            };
            if id == ROOT_FOLDER_ID {
                continue;
            }
            name.clear();
            encoding.decode_into(key.name, &mut name);
            summary.entries = summary.entries.saturating_add(1);
            let flow = visit(ScanEntry {
                node: NodeId(u64::from(id)),
                parent: NodeId(u64::from(key.parent_id)),
                name: &name,
                kind,
                size,
                resource_size,
                modified: hfs_time(modified),
                flags: entry_flags(false, finder_flags),
                finder_type,
            });
            if flow.is_break() {
                return Ok(ControlFlow::Break(()));
            }
        }
        Ok(ControlFlow::Continue(()))
    })?;
    summary.skipped = summary.skipped.saturating_add(u64::from(leaves.skipped));
    Ok(summary)
}
