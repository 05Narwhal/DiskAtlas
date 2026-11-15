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

//! Classic HFS extents: the 8-byte overflow key, three-extent records and
//! fork mapping through the extents overflow B-tree.

use crate::fs::hfsplus::bytes::be_u16;

/// Bytes in one extent descriptor (`HFSExtentDescriptor`).
pub const EXTENT_LEN: usize = 4;

/// Extent descriptors in one extent record (`HFSExtentRecord`).
pub const EXTENTS_PER_RECORD: usize = 3;

/// Bytes in one extent record.
pub const EXTENT_RECORD_LEN: usize = EXTENT_LEN * EXTENTS_PER_RECORD;

/// One run of allocation blocks (`HFSExtentDescriptor`): big-endian
/// `startBlock` u16 then `blockCount` u16. A count of 0 is an unused slot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct HfsExtent {
    /// First allocation block of the run.
    pub start_block: u16,
    /// Allocation blocks in the run.
    pub block_count: u16,
}

/// Three extent descriptors (`HFSExtentRecord`).
pub type HfsExtentRecord = [HfsExtent; EXTENTS_PER_RECORD];

/// Parses an extent record from the first [`EXTENT_RECORD_LEN`] bytes of
/// `b`. `None` when `b` is shorter. Values are not validated here.
pub fn parse_extent_record(b: &[u8]) -> Option<HfsExtentRecord> {
    let mut record = [HfsExtent::default(); EXTENTS_PER_RECORD];
    for (i, slot) in record.iter_mut().enumerate() {
        let at = i.checked_mul(EXTENT_LEN)?;
        *slot = HfsExtent {
            start_block: be_u16(b, at)?,
            block_count: be_u16(b, at.checked_add(2)?)?,
        };
    }
    Some(record)
}

/// Writes `record` into the first [`EXTENT_RECORD_LEN`] bytes of `out`, the
/// inverse of [`parse_extent_record`]. `None` (and `out` unchanged) when
/// `out` is shorter.
pub fn write_extent_record(record: &HfsExtentRecord, out: &mut [u8]) -> Option<()> {
    let out = out.get_mut(..EXTENT_RECORD_LEN)?;
    for (extent, slot) in record.iter().zip(out.chunks_exact_mut(EXTENT_LEN)) {
        slot[..2].copy_from_slice(&extent.start_block.to_be_bytes());
        slot[2..].copy_from_slice(&extent.block_count.to_be_bytes());
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extent_record_round_trip_and_short_input() {
        let bytes: Vec<u8> = (1..=12).collect();
        let record = parse_extent_record(&bytes).unwrap();
        assert_eq!(
            record[0],
            HfsExtent {
                start_block: 0x0102,
                block_count: 0x0304
            }
        );
        assert_eq!(record[2].block_count, 0x0B0C);
        let mut out = [0u8; EXTENT_RECORD_LEN];
        write_extent_record(&record, &mut out).unwrap();
        assert_eq!(&out[..], &bytes[..]);
        assert_eq!(parse_extent_record(&bytes[..11]), None);
        assert_eq!(write_extent_record(&record, &mut [0u8; 11]), None);
    }
}
