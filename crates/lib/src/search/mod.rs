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

//! In-memory name index and query engine. Built in M4.
//!
//! [`build`] scans a volume once into an [`Index`]: every name in one
//! buffer, the facts of each entry in parallel arrays, entries in
//! depth-first pre-order so a folder's subtree is a contiguous range. The
//! query engine scans the case-folded copy of the names on every keystroke.

pub use build::{BuildProgress, build};
pub use index::{Index, IndexFlags};
pub use order::{HitOrder, order_hits};
pub use query::{HEAD, Hits, Query, match_ranges, terms};

pub mod build;
pub mod fold;
pub mod index;
pub mod kind;
pub mod order;
pub mod persist;
pub mod query;

#[cfg(test)]
pub(crate) mod bench;
