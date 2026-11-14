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

//! Folder sizes.
//!
//! [`IndexSizes`] gives every folder of a volume Disk Atlas reads its total
//! size, and the volume its bytes per kind, from the volume's search
//! [`Index`](crate::search::index::Index). The Size column, the storage bar,
//! Properties and the space map read it.
//!
//! [`HostSizes`] measures the folders of one of this computer's own drives
//! on worker threads it owns, filling a tree of folders lazily and
//! correcting it in place when folders change.

pub mod host;
pub mod index;
pub mod watch;

pub use host::{
    HostDir, HostSizes, MAX_HOST_FOLDERS, NOTIFY_INTERVAL, Priority, SizeState, is_network_drive,
    size_workers,
};
pub use index::{ClassBytes, DENSE_NODE_LIMIT, IndexSizes, MAX_LINK_NESTING, Totals};
