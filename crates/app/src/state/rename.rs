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

//! The text of an in-place rename (`r1-redesign.md` 4): a name with a caret and a selection.
//! Pure, so the editing rules are tested without a window. Offsets are byte offsets on character
//! boundaries.

use std::ops::Range;

use crate::ops::stem_len;

/// A name being edited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameEdit {
    text: String,
    /// Where the selection started.
    anchor: usize,
    /// Where the caret is.
    cursor: usize,
}

impl RenameEdit {
    /// Starts on `name` with its stem selected, not the extension.
    pub fn new(name: &str) -> Self {
        Self {
            text: name.to_owned(),
            anchor: 0,
            cursor: stem_len(name),
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// The selected bytes (empty when there is only a caret).
    pub fn selection(&self) -> Range<usize> {
        self.anchor.min(self.cursor)..self.anchor.max(self.cursor)
    }

    #[cfg(test)]
    pub fn caret(&self) -> usize {
        self.cursor
    }

    /// Types `typed`, replacing the selection. Control characters are dropped.
    pub fn insert(&mut self, typed: &str) {
        let clean: String = typed.chars().filter(|c| !c.is_control()).collect();
        if clean.is_empty() {
            return;
        }
        let range = self.selection();
        self.text.replace_range(range.clone(), &clean);
        self.cursor = range.start + clean.len();
        self.anchor = self.cursor;
    }

    /// Backspace: the selection, or the character before the caret.
    pub fn backspace(&mut self) {
        if self.selection().is_empty() {
            let before = self.prev(self.cursor);
            self.anchor = before;
        }
        self.remove_selection();
    }

    /// Delete: the selection, or the character after the caret.
    pub fn delete(&mut self) {
        if self.selection().is_empty() {
            self.cursor = self.next(self.cursor);
        }
        self.remove_selection();
    }

    fn remove_selection(&mut self) {
        let range = self.selection();
        self.text.replace_range(range.clone(), "");
        self.cursor = range.start;
        self.anchor = range.start;
    }

    pub fn left(&mut self, extend: bool) {
        let to = if !extend && !self.selection().is_empty() {
            self.selection().start
        } else {
            self.prev(self.cursor)
        };
        self.place(to, extend);
    }

    pub fn right(&mut self, extend: bool) {
        let to = if !extend && !self.selection().is_empty() {
            self.selection().end
        } else {
            self.next(self.cursor)
        };
        self.place(to, extend);
    }

    pub fn home(&mut self, extend: bool) {
        self.place(0, extend);
    }

    pub fn end(&mut self, extend: bool) {
        self.place(self.text.len(), extend);
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.cursor = self.text.len();
    }

    fn place(&mut self, to: usize, extend: bool) {
        self.cursor = to;
        if !extend {
            self.anchor = to;
        }
    }

    fn prev(&self, from: usize) -> usize {
        self.text[..from]
            .char_indices()
            .next_back()
            .map_or(0, |(at, _)| at)
    }

    fn next(&self, from: usize) -> usize {
        self.text[from..]
            .chars()
            .next()
            .map_or(from, |c| from + c.len_utf8())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_with_the_stem_selected() {
        let edit = RenameEdit::new("photo.jpg");
        assert_eq!(edit.selection(), 0..5);
        assert_eq!(RenameEdit::new(".hidden").selection(), 0..7);
    }

    #[test]
    fn typing_replaces_the_selection_and_drops_control_characters() {
        let mut edit = RenameEdit::new("photo.jpg");
        edit.insert("new\n");
        assert_eq!(edit.text(), "new.jpg");
        assert_eq!(edit.caret(), 3);
        assert!(edit.selection().is_empty());
    }

    #[test]
    fn editing_respects_character_boundaries() {
        let mut edit = RenameEdit::new("é\u{1F600}x");
        edit.end(false);
        edit.left(false);
        edit.left(false);
        assert_eq!(edit.caret(), "é".len());
        edit.backspace();
        assert_eq!(edit.text(), "\u{1F600}x");
        edit.delete();
        assert_eq!(edit.text(), "x");
        // Both ends are quiet.
        edit.backspace();
        edit.left(false);
        edit.delete();
        edit.delete();
        assert_eq!(edit.text(), "");
    }

    #[test]
    fn shift_extends_and_a_plain_arrow_collapses_to_the_edge() {
        let mut edit = RenameEdit::new("abcd");
        edit.home(false);
        edit.right(true);
        edit.right(true);
        assert_eq!(edit.selection(), 0..2);
        edit.right(false);
        assert_eq!((edit.caret(), edit.selection()), (2, 2..2));
    }
}
