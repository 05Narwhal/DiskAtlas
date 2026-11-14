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

//! The selection set, the cursor and the anchor of the file list. No GPUI types.
//!
//! Everything is keyed by arrival index (see `loader`), so re-ordering the rows changes nothing
//! here. Operations that think in display positions take the current order.

const WORD: usize = u64::BITS as usize;

/// Which rows are selected, where the cursor is, and where a range selection starts.
#[derive(Debug, Clone, Default)]
pub struct Selection {
    /// One bit per arrival index; grows on demand.
    bits: Vec<u64>,
    count: usize,
    cursor: Option<u32>,
    anchor: Option<u32>,
}

impl Selection {
    /// Nothing selected, no cursor.
    pub fn new() -> Self {
        Self::default()
    }

    /// The row the cursor is on.
    pub fn cursor(&self) -> Option<u32> {
        self.cursor
    }

    /// Number of selected rows.
    pub fn count(&self) -> usize {
        self.count
    }

    /// True when the row is selected.
    pub fn is_selected(&self, arrival: u32) -> bool {
        let (word, bit) = Self::locate(arrival);
        self.bits.get(word).is_some_and(|w| w & bit != 0)
    }

    /// The selected row when exactly one is selected.
    pub fn single(&self) -> Option<u32> {
        if self.count != 1 {
            return None;
        }
        if let Some(cursor) = self.cursor.filter(|&c| self.is_selected(c)) {
            return Some(cursor);
        }
        let (index, word) = self.bits.iter().enumerate().find(|(_, w)| **w != 0)?;
        let arrival = index * WORD + word.trailing_zeros() as usize;
        u32::try_from(arrival).ok()
    }

    /// Selects only `row`; the cursor and the anchor go there.
    pub fn replace(&mut self, row: u32) {
        self.clear_bits();
        self.set(row);
        self.cursor = Some(row);
        self.anchor = Some(row);
    }

    /// Adds or removes `row`; the cursor and the anchor go there.
    pub fn toggle(&mut self, row: u32) {
        let (word, bit) = Self::locate(row);
        self.grow(word);
        if let Some(w) = self.bits.get_mut(word) {
            *w ^= bit;
            if *w & bit != 0 {
                self.count += 1;
            } else {
                self.count -= 1;
            }
        }
        self.cursor = Some(row);
        self.anchor = Some(row);
    }

    /// Selects the display range from the anchor (or the cursor, or the first row when there is
    /// neither) to `position`, replacing what was selected. The cursor goes to `position`; the
    /// anchor stays, or is set to the range's fixed end if it was unset. A `position` past the
    /// end means the last row. Does nothing when `order` is empty.
    pub fn extend_to(&mut self, position: usize, order: &[u32]) {
        let Some(last) = order.len().checked_sub(1) else {
            return;
        };
        let position = position.min(last);
        let from = self
            .anchor
            .or(self.cursor)
            .and_then(|row| order.iter().position(|&a| a == row))
            .unwrap_or(0);
        let (low, high) = if from <= position {
            (from, position)
        } else {
            (position, from)
        };
        self.clear_bits();
        for &row in order.get(low..=high).unwrap_or_default() {
            self.set(row);
        }
        self.cursor = order.get(position).copied();
        if self.anchor.is_none() {
            self.anchor = order.get(from).copied();
        }
    }

    /// Selects every row in `order`. The cursor and the anchor stay.
    pub fn select_all(&mut self, order: &[u32]) {
        self.clear_bits();
        for &row in order {
            self.set(row);
        }
    }

    /// Selects nothing. The cursor stays, and becomes the anchor.
    pub fn clear(&mut self) {
        self.clear_bits();
        self.anchor = self.cursor;
    }

    /// Forgets everything, for a new folder.
    pub fn reset(&mut self) {
        self.bits.clear();
        self.count = 0;
        self.cursor = None;
        self.anchor = None;
    }

    fn locate(arrival: u32) -> (usize, u64) {
        let arrival = arrival as usize;
        (arrival / WORD, 1 << (arrival % WORD))
    }

    fn grow(&mut self, word: usize) {
        if self.bits.len() <= word {
            self.bits.resize(word + 1, 0);
        }
    }

    /// Selects `row`, counting it if it was not selected.
    fn set(&mut self, row: u32) {
        let (word, bit) = Self::locate(row);
        self.grow(word);
        if let Some(w) = self.bits.get_mut(word)
            && *w & bit == 0
        {
            *w |= bit;
            self.count += 1;
        }
    }

    fn clear_bits(&mut self) {
        if self.count > 0 {
            self.bits.fill(0);
            self.count = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Display order: arrival indices 4, 2, 0, 3, 1 at positions 0 to 4.
    const ORDER: [u32; 5] = [4, 2, 0, 3, 1];

    fn selected(selection: &Selection) -> Vec<u32> {
        (0..200).filter(|&a| selection.is_selected(a)).collect()
    }

    #[test]
    fn replace_selects_one() {
        let mut s = Selection::new();
        assert_eq!(s.count(), 0);
        assert_eq!(s.single(), None);
        s.replace(3);
        s.replace(130);
        assert_eq!(selected(&s), [130]);
        assert_eq!(s.count(), 1);
        assert_eq!(s.single(), Some(130));
        assert_eq!((s.cursor(), s.anchor), (Some(130), Some(130)));
    }

    #[test]
    fn toggle_adds_and_removes() {
        let mut s = Selection::new();
        s.replace(1);
        s.toggle(70);
        assert_eq!(selected(&s), [1, 70]);
        assert_eq!(s.count(), 2);
        assert_eq!(s.single(), None);
        assert_eq!((s.cursor(), s.anchor), (Some(70), Some(70)));
        s.toggle(1);
        assert_eq!(selected(&s), [70]);
        assert_eq!(s.single(), Some(70));
        s.toggle(70);
        assert_eq!(s.count(), 0);
        assert_eq!(s.cursor(), Some(70));
        // The single row is found even when the cursor is not on it.
        s.toggle(5);
        s.toggle(9);
        s.toggle(9);
        assert_eq!(s.cursor(), Some(9));
        assert_eq!(s.single(), Some(5));
    }

    #[test]
    fn extend_forwards_and_backwards_from_the_anchor() {
        let mut s = Selection::new();
        s.replace(2); // position 1
        s.extend_to(3, &ORDER); // positions 1..=3
        assert_eq!(selected(&s), [0, 2, 3]);
        assert_eq!((s.cursor(), s.anchor), (Some(3), Some(2)));
        // Extending again from the same anchor replaces the range, here backwards.
        s.extend_to(0, &ORDER); // positions 0..=1
        assert_eq!(selected(&s), [2, 4]);
        assert_eq!(s.count(), 2);
        assert_eq!((s.cursor(), s.anchor), (Some(4), Some(2)));
        s.extend_to(1, &ORDER);
        assert_eq!(selected(&s), [2]);
        assert_eq!(s.single(), Some(2));
    }

    #[test]
    fn extend_without_anchor_uses_the_cursor_or_the_top() {
        let mut s = Selection::new();
        s.extend_to(2, &ORDER);
        assert_eq!(selected(&s), [0, 2, 4]);
        assert_eq!((s.cursor(), s.anchor), (Some(0), Some(4)));

        let mut s = Selection::new();
        s.replace(3); // position 3
        s.clear(); // keeps the cursor, which becomes the anchor
        assert_eq!(s.count(), 0);
        assert_eq!((s.cursor(), s.anchor), (Some(3), Some(3)));
        s.extend_to(4, &ORDER);
        assert_eq!(selected(&s), [1, 3]);
    }

    #[test]
    fn extend_past_the_end_and_on_empty_order() {
        let mut s = Selection::new();
        s.replace(0); // position 2
        s.extend_to(99, &ORDER);
        assert_eq!(selected(&s), [0, 1, 3]);
        assert_eq!(s.cursor(), Some(1));
        s.extend_to(0, &[]);
        assert_eq!(selected(&s), [0, 1, 3]);
        // An anchor that is not in the order counts as the top.
        let mut s = Selection::new();
        s.replace(77);
        s.extend_to(1, &ORDER);
        assert_eq!(selected(&s), [2, 4]);
        assert_eq!(s.anchor, Some(77));
    }

    #[test]
    fn select_all_clear_reset() {
        let mut s = Selection::new();
        s.replace(2);
        s.select_all(&ORDER);
        assert_eq!(selected(&s), [0, 1, 2, 3, 4]);
        assert_eq!(s.count(), 5);
        assert_eq!(s.cursor(), Some(2));
        s.clear();
        assert_eq!(s.count(), 0);
        assert!(selected(&s).is_empty());
        assert_eq!(s.cursor(), Some(2));
        s.select_all(&ORDER);
        s.reset();
        assert_eq!(s.count(), 0);
        assert!(selected(&s).is_empty());
        assert_eq!((s.cursor(), s.anchor), (None, None));
    }

    #[test]
    fn indices_past_the_end_are_not_selected() {
        let s = Selection::new();
        assert!(!s.is_selected(0));
        assert!(!s.is_selected(u32::MAX));
        let mut s = Selection::new();
        s.replace(63);
        s.toggle(64);
        assert!(s.is_selected(63) && s.is_selected(64));
        assert!(!s.is_selected(65) && !s.is_selected(1_000_000));
        assert_eq!(s.count(), 2);
    }
}
