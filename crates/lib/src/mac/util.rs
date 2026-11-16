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

//! Small helpers the Mac convention readers share: big-endian reads that
//! return `None` instead of indexing, and the corruption error builders.

use crate::error::{CorruptKind, Corruption, Error, Location};

/// A corruption error at `offset` of the structure `what`.
pub(crate) fn corrupt(what: &'static str, offset: u64, kind: CorruptKind) -> Error {
    Error::Corrupt(Corruption::new(what, Location::Byte(offset), kind))
}

/// A corruption error with no useful location.
pub(crate) fn corrupt_text(what: &'static str, text: &'static str) -> Error {
    Error::Corrupt(Corruption::new(
        what,
        Location::Unspecified,
        CorruptKind::Other(text),
    ))
}

/// A truncation error: `needed` bytes at `offset`, `available` in the input.
pub(crate) fn truncated(what: &'static str, offset: u64, needed: u64, available: u64) -> Error {
    corrupt(what, offset, CorruptKind::Truncated { needed, available })
}

/// The `N` bytes at `at`, or `None` when the slice is shorter.
pub(crate) fn array<const N: usize>(b: &[u8], at: usize) -> Option<[u8; N]> {
    b.get(at..)?.first_chunk::<N>().copied()
}

/// Big-endian u16 at `at`.
pub(crate) fn be16(b: &[u8], at: usize) -> Option<u16> {
    array::<2>(b, at).map(u16::from_be_bytes)
}

/// Big-endian u32 at `at`.
pub(crate) fn be32(b: &[u8], at: usize) -> Option<u32> {
    array::<4>(b, at).map(u32::from_be_bytes)
}

/// Big-endian u64 at `at`.
pub(crate) fn be64(b: &[u8], at: usize) -> Option<u64> {
    array::<8>(b, at).map(u64::from_be_bytes)
}

/// `usize` from a `u64`, saturating (a value that large fails every later
/// bounds check).
pub(crate) fn to_usize(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// The length of `bytes` as a `u64`.
pub(crate) fn len64(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).unwrap_or(u64::MAX)
}
