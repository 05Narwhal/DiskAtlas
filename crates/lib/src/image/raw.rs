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

//! Raw images: the file is the disk.

use std::path::Path;
use std::sync::Arc;

use crate::error::Result;
use crate::io::block::BlockSource;
use crate::io::file::FileSource;

/// Opens `path` read-only; the file's bytes are the disk's bytes.
pub fn open(path: &Path) -> Result<Arc<dyn BlockSource>> {
    Ok(Arc::new(FileSource::open(path)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_an_error() {
        let path =
            std::env::temp_dir().join(format!("disk-atlas-raw-missing-{}.img", std::process::id()));
        assert!(open(&path).is_err());
    }
}
