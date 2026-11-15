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

//! Classic HFS (big-endian), the Mac OS volume format before HFS+.
//!
//! The catalog and extents overflow files are B-trees read through the
//! shared [`crate::fs::hfsplus::btree`] reader; forks map through
//! [`crate::fs::hfsplus::extents::ForkMap`]. Built in M6.

pub mod catalog;
pub mod encoding;
pub mod extents;
#[cfg(test)]
mod fuzz_seeds;
pub mod mdb;
#[cfg(test)]
pub(crate) mod test_volume;
