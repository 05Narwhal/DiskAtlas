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

//! The maths of the icon view (`r1-redesign.md` 1.7): how many tiles fit across, which grid row
//! holds a position, and where the keyboard goes. No GPUI, so it is tested alone.
//!
//! The grid is a `uniform_list` of grid rows, each holding `columns` tiles. Position `p` of the
//! listing sits in grid row `p / columns`, column `p % columns`.

use std::ops::Range;

/// Tile side at the regular density, in pixels.
pub const TILE_REGULAR: f32 = 96.0;
/// Tile side at the roomy density, in pixels.
pub const TILE_ROOMY: f32 = 128.0;
/// Space between tiles, across and down.
pub const GAP: f32 = 8.0;
/// Space between the grid and the edge of the view.
pub const INSET: f32 = 12.0;
/// Height of the name under the picture: two lines.
pub const LABEL_HEIGHT: f32 = 40.0;
/// Columns assumed before the view has been measured.
pub const UNMEASURED_COLUMNS: usize = 6;

/// Tiles that fit across `width` pixels of view, at least one.
pub fn columns_for(width: f32, tile: f32) -> usize {
    if !width.is_finite() || width <= 0.0 {
        return UNMEASURED_COLUMNS;
    }
    let usable = width - 2.0 * INSET + GAP;
    ((usable / (tile + GAP)).floor() as usize).max(1)
}

/// Height of one grid row: the tile, its label and the gap below.
pub fn row_height(tile: f32) -> f32 {
    tile + LABEL_HEIGHT + GAP
}

/// Grid rows needed for `len` tiles in `columns`.
pub fn row_count(len: usize, columns: usize) -> usize {
    len.div_ceil(columns.max(1))
}

/// The positions shown in grid row `row`.
pub fn positions(row: usize, columns: usize, len: usize) -> Range<usize> {
    let columns = columns.max(1);
    let start = row.saturating_mul(columns).min(len);
    start..start.saturating_add(columns).min(len)
}

/// The grid row that holds `position`.
pub fn row_of(position: usize, columns: usize) -> usize {
    position / columns.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_follow_the_width_and_never_reach_zero() {
        assert_eq!(columns_for(0.0, TILE_REGULAR), UNMEASURED_COLUMNS);
        assert_eq!(columns_for(f32::NAN, TILE_REGULAR), UNMEASURED_COLUMNS);
        assert_eq!(columns_for(50.0, TILE_REGULAR), 1);
        // 12 + 3 * 96 + 2 * 8 + 12 = 328 fits three tiles, 327 only two.
        assert_eq!(columns_for(328.0, TILE_REGULAR), 3);
        assert_eq!(columns_for(327.0, TILE_REGULAR), 2);
    }

    #[test]
    fn rows_cover_every_position_once() {
        assert_eq!(row_count(0, 4), 0);
        assert_eq!(row_count(9, 4), 3);
        assert_eq!(positions(2, 4, 9), 8..9);
        assert_eq!(positions(3, 4, 9), 9..9);
        assert_eq!(positions(usize::MAX, 4, 9), 9..9);
        assert_eq!(row_of(8, 4), 2);
    }
}
