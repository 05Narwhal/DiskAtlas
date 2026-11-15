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

//! Total and free bytes of the host volume that holds a path.

use std::path::Path;

/// The total and free bytes of the host volume that holds `path`, as
/// `(total, free)`, or `None` when the system cannot tell (the path is gone,
/// the drive is not ready, a network drive is offline).
///
/// Free is the volume's free space, not the share of it the current user's
/// quota allows, so the numbers match the system's own drive properties.
/// One system call, nothing cached: call it again for fresh numbers.
pub fn volume_space(path: &Path) -> Option<(u64, u64)> {
    let (total, free) = imp::volume_space(path)?;
    // A filesystem that reports more free than total is not trusted for either.
    (free <= total).then_some((total, free))
}

#[cfg(windows)]
mod imp {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    #[allow(unsafe_code)]
    pub(super) fn volume_space(path: &Path) -> Option<(u64, u64)> {
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.is_empty() || wide.contains(&0) {
            return None;
        }
        // A UNC path must end in a separator; a local folder accepts one too.
        if !matches!(wide.last(), Some(&c) if c == u16::from(b'\\') || c == u16::from(b'/')) {
            wide.push(u16::from(b'\\'));
        }
        wide.push(0);
        let mut total: u64 = 0;
        let mut free: u64 = 0;
        // SAFETY: `wide` is a live, NUL-terminated UTF-16 buffer with no
        // interior NUL, borrowed for the whole call and only read by it.
        // `total` and `free` are live, writable `u64`s exclusively borrowed
        // for the call; the caller-quota output is optional and passed as
        // null. The call is synchronous and keeps no pointer after it returns.
        let ok = unsafe {
            GetDiskFreeSpaceExW(wide.as_ptr(), std::ptr::null_mut(), &mut total, &mut free)
        };
        (ok != 0).then_some((total, free))
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::Path;

    // TODO(R7-unix): statvfs through `libc` (total = f_blocks * f_frsize,
    // free = f_bavail * f_frsize, checked). Not added yet: the Unix branch
    // could not be type-checked on the Windows build host.
    pub(super) fn volume_space(_path: &Path) -> Option<(u64, u64)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn temp_dir_has_space() {
        let (total, free) = volume_space(&std::env::temp_dir()).expect("temp dir volume");
        assert!(total > 0);
        assert!(free <= total);
    }

    #[test]
    fn missing_path_has_none() {
        let missing = std::env::temp_dir().join("disk-atlas-space-does-not-exist-7f3a");
        assert_eq!(volume_space(&missing), None);
        assert_eq!(volume_space(Path::new("")), None);
    }
}
