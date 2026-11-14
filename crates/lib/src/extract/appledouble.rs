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

//! AppleDouble sidecar files (`._name`) for resource forks and Finder information. Built in M5 task 3b.
//!
//! The layout is AppleDouble version 2 (RFC 1740, Apple's "AppleSingle/
//! AppleDouble Formats" note), all big-endian:
//!
//! ```text
//! 0   4  magic 0x00051607            4   4  version 0x00020000
//! 8   16 filler, zeros               24  2  number of entries (1 or 2)
//! 26  12 entry: id 9 (Finder info), offset, length 32
//! 38  12 entry: id 2 (resource fork), offset, length   (when there is a fork)
//! then the 32 Finder info bytes, then the fork bytes
//! ```
//!
//! [`header`] builds everything before the fork; the caller streams the
//! fork after it.

/// AppleDouble magic number.
pub const MAGIC: u32 = 0x0005_1607;
/// AppleDouble version 2.
pub const VERSION: u32 = 0x0002_0000;
/// Entry id of the resource fork.
pub const ENTRY_RESOURCE_FORK: u32 = 2;
/// Entry id of the Finder information.
pub const ENTRY_FINDER_INFO: u32 = 9;
/// Length of the Finder information entry.
pub const FINDER_INFO_LEN: usize = 32;

/// Bytes before the first entry descriptor: magic, version, filler, count.
const FIXED: usize = 26;
/// Bytes of one entry descriptor: id, offset, length.
const DESCRIPTOR: usize = 12;
/// Where the fork starts when there is one: two descriptors, then the
/// Finder information.
pub const FORK_OFFSET: usize = FIXED + 2 * DESCRIPTOR + FINDER_INFO_LEN;
/// Longest resource fork a sidecar can describe: its end must fit the u32
/// offset and length fields.
pub const MAX_FORK_LEN: u64 = u32::MAX as u64 - FORK_OFFSET as u64;

/// The sidecar bytes before the resource fork: the header, the entry
/// descriptors and the Finder information (zeros when `finder_info` is
/// `None`). `fork_len` 0 means no fork entry. `None` when `fork_len` is past
/// [`MAX_FORK_LEN`].
pub fn header(finder_info: Option<&[u8; FINDER_INFO_LEN]>, fork_len: u64) -> Option<Vec<u8>> {
    if fork_len > MAX_FORK_LEN {
        return None;
    }
    let fork_len = u32::try_from(fork_len).ok()?;
    let entries: u16 = if fork_len > 0 { 2 } else { 1 };
    let descriptors = DESCRIPTOR.checked_mul(usize::from(entries))?;
    let finder_at = FIXED.checked_add(descriptors)?;
    let total = finder_at.checked_add(FINDER_INFO_LEN)?;

    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&MAGIC.to_be_bytes());
    out.extend_from_slice(&VERSION.to_be_bytes());
    out.extend_from_slice(&[0u8; 16]);
    out.extend_from_slice(&entries.to_be_bytes());
    push_descriptor(
        &mut out,
        ENTRY_FINDER_INFO,
        u32::try_from(finder_at).ok()?,
        u32::try_from(FINDER_INFO_LEN).ok()?,
    );
    if fork_len > 0 {
        push_descriptor(
            &mut out,
            ENTRY_RESOURCE_FORK,
            u32::try_from(total).ok()?,
            fork_len,
        );
    }
    out.extend_from_slice(finder_info.unwrap_or(&[0u8; FINDER_INFO_LEN]));
    Some(out)
}

fn push_descriptor(out: &mut Vec<u8>, id: u32, offset: u32, len: u32) {
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&offset.to_be_bytes());
    out.extend_from_slice(&len.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finder() -> [u8; 32] {
        let mut info = [0u8; 32];
        info[..8].copy_from_slice(b"TEXTttxt");
        info[8] = 0x40;
        info[31] = 0xEE;
        info
    }

    fn fixed(entries: u8) -> Vec<u8> {
        let mut want = vec![0x00, 0x05, 0x16, 0x07, 0x00, 0x02, 0x00, 0x00];
        want.extend_from_slice(&[0u8; 16]);
        want.extend_from_slice(&[0x00, entries]);
        want
    }

    #[test]
    fn finder_info_only() {
        let info = finder();
        let got = header(Some(&info), 0).unwrap();
        let mut want = fixed(1);
        want.extend_from_slice(&[0, 0, 0, 9, 0, 0, 0, 38, 0, 0, 0, 32]);
        want.extend_from_slice(&info);
        assert_eq!(got, want);
        assert_eq!(got.len(), 70);
    }

    #[test]
    fn fork_and_finder_info() {
        let info = finder();
        let got = header(Some(&info), 0x0102_0304).unwrap();
        let mut want = fixed(2);
        want.extend_from_slice(&[0, 0, 0, 9, 0, 0, 0, 50, 0, 0, 0, 32]);
        want.extend_from_slice(&[0, 0, 0, 2, 0, 0, 0, 82, 0x01, 0x02, 0x03, 0x04]);
        want.extend_from_slice(&info);
        assert_eq!(got, want);
        assert_eq!(got.len(), FORK_OFFSET);
        assert_eq!(FORK_OFFSET, 82);
    }

    #[test]
    fn fork_without_finder_info() {
        let got = header(None, 3).unwrap();
        let mut want = fixed(2);
        want.extend_from_slice(&[0, 0, 0, 9, 0, 0, 0, 50, 0, 0, 0, 32]);
        want.extend_from_slice(&[0, 0, 0, 2, 0, 0, 0, 82, 0, 0, 0, 3]);
        want.extend_from_slice(&[0u8; 32]);
        assert_eq!(got, want);
    }

    #[test]
    fn largest_fork() {
        let got = header(None, MAX_FORK_LEN).unwrap();
        assert_eq!(&got[46..50], &(u32::MAX - 82).to_be_bytes());
        assert!(header(None, MAX_FORK_LEN + 1).is_none());
        assert!(header(None, u64::MAX).is_none());
    }
}
