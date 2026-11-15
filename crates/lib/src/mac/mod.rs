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

//! Mac (and other operating systems') housekeeping conventions. Pure
//! functions, apart from [`read_hidden_list`], which reads one small file.

pub mod alias;
pub mod appledouble;
pub mod bplist;
pub mod bundle;
pub mod conventions;
pub mod icns;
pub mod ds_store;
pub mod original;
pub mod resource_map;
pub mod sidecar_fs;
pub mod trash;
pub mod xattr;
pub(crate) mod util;

pub use conventions::{SystemItem, parse_hidden_list, read_hidden_list, system_item};
