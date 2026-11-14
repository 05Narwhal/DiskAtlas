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

//! Sort keys and comparators for the file list. No GPUI types.
//!
//! Names compare case-insensitively with runs of digits compared as numbers, so "file2" comes
//! before "file10". Every comparator here is a strict total order: two different names are never
//! `Equal`, which `sort_unstable_by` relies on.

use std::cmp::Ordering;
use std::sync::Arc;

use disk_atlas_lib::fs::DirEntry;

#[cfg(test)]
use crate::format;

/// The column the rows are ordered by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
    /// The entry's name.
    Name,
    /// When the content was last modified.
    Modified,
    /// Size of the data stream.
    Size,
    /// Size of the resource fork (HFS family volumes only).
    ResourceFork,
    /// The kind in plain words ("PNG image").
    Kind,
    /// Search results only: the order the search ranked them in. The default in results.
    Relevance,
    /// Search results only: the folder holding the entry, in path order.
    Folder,
}

/// A column and a direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortSpec {
    /// The column compared.
    pub column: SortColumn,
    /// True to reverse the column comparison. Folders stay first either way.
    pub descending: bool,
}

impl Default for SortSpec {
    /// By name, ascending.
    fn default() -> Self {
        Self {
            column: SortColumn::Name,
            descending: false,
        }
    }
}

/// The case-folded form of a character used for comparing: Unicode simple lowercase.
///
/// `char::to_lowercase` yields more than one character only for U+0130 (capital I with dot),
/// whose simple lowercase is its first character, "i".
#[inline]
fn fold(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

/// The character starting at byte `at` of `s`. `at` is always a character boundary here.
#[inline]
fn char_at(s: &str, at: usize) -> Option<char> {
    s.get(at..).and_then(|rest| rest.chars().next())
}

/// End of the run of ASCII digits starting at `start`.
#[inline]
fn digit_run_end(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while bytes.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    end
}

/// The digits of `bytes[start..end]` without leading zeros.
#[inline]
fn significant(bytes: &[u8], start: usize, end: usize) -> &[u8] {
    let digits = bytes.get(start..end).unwrap_or_default();
    let zeros = digits.iter().take_while(|&&d| d == b'0').count();
    digits.get(zeros..).unwrap_or_default()
}

/// Natural, case-insensitive comparison without the final tie-break: `Equal` for names that
/// differ only in case or in leading zeros.
///
/// Each name is read as a sequence of tokens: a maximal run of ASCII digits, or one character.
/// Two digit runs compare by numeric value; anything else compares by folded character, a digit
/// run taking the place of its first digit. The token sequences compare lexicographically, a
/// shorter sequence first. Because each token maps to a fixed key, this is a total preorder.
pub fn compare_tokens(a: &str, b: &str) -> Ordering {
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    loop {
        let (x, y) = match (ab.get(i), bb.get(j)) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(&x), Some(&y)) => (x, y),
        };
        if x.is_ascii_digit() && y.is_ascii_digit() {
            let (end_a, end_b) = (digit_run_end(ab, i), digit_run_end(bb, j));
            let (num_a, num_b) = (significant(ab, i, end_a), significant(bb, j, end_b));
            let by_value = num_a.len().cmp(&num_b.len()).then_with(|| num_a.cmp(num_b));
            if by_value != Ordering::Equal {
                return by_value;
            }
            i = end_a;
            j = end_b;
        } else if x.is_ascii() && y.is_ascii() {
            let by_char = x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase());
            if by_char != Ordering::Equal {
                return by_char;
            }
            i += 1;
            j += 1;
        } else {
            // At least one side is the first byte of a multi-byte character.
            let (Some(ca), Some(cb)) = (char_at(a, i), char_at(b, j)) else {
                // Not reached: both indices are character boundaries inside their strings.
                return a.cmp(b);
            };
            let by_char = fold(ca).cmp(&fold(cb));
            if by_char != Ordering::Equal {
                return by_char;
            }
            i += ca.len_utf8();
            j += cb.len_utf8();
        }
    }
}

/// Case-insensitive, digit runs compared as numbers ("file2" < "file10"), a TOTAL order.
///
/// Names that are equal apart from case or leading zeros are ordered by their bytes, so two
/// different strings are never `Equal`. No allocation.
pub fn compare_names(a: &str, b: &str) -> Ordering {
    compare_tokens(a, b).then_with(|| a.cmp(b))
}

/// True when `name` starts with `prefix`, comparing folded characters the way
/// [`compare_tokens`] does. No allocation.
pub fn starts_with_folded(name: &str, prefix: &str) -> bool {
    let mut name = name.chars();
    prefix
        .chars()
        .all(|p| name.next().is_some_and(|n| fold(n) == fold(p)))
}

/// The row order for `spec`.
///
/// Browsable entries (folders and links to folders) come first in both directions. Then the
/// column: names naturally, dates and sizes (resource fork included) with "unknown" lowest, kinds
/// by their text.
/// `descending` reverses only the column. Ties fall back to the name, ascending.
#[cfg(test)]
pub fn compare_entries(spec: SortSpec, a: &DirEntry<'_>, b: &DirEntry<'_>) -> Ordering {
    let (kind_a, kind_b) = if spec.column == SortColumn::Kind {
        (format::kind(a.name, a.kind), format::kind(b.name, b.kind))
    } else {
        ("", "")
    };
    compare_entries_with_kinds(spec, a, kind_a, b, kind_b)
}

/// [`compare_entries`] with each entry's `format::kind` text computed beforehand, which the
/// loader caches because the kind lookup is a table scan. The kinds are read only when sorting by
/// [`SortColumn::Kind`].
#[cfg(test)]
pub fn compare_entries_with_kinds(
    spec: SortSpec,
    a: &DirEntry<'_>,
    kind_a: &str,
    b: &DirEntry<'_>,
    kind_b: &str,
) -> Ordering {
    compare_entries_sized(spec, (a, kind_a, a.size), (b, kind_b, b.size))
}

/// The comparison of two entries, with each entry's size for [`SortColumn::Size`] given: a
/// folder's total from the volume's size table, a file's own size. `None` is lowest. Each side is
/// the entry, its kind text and its size.
pub fn compare_entries_sized(
    spec: SortSpec,
    (a, kind_a, size_a): (&DirEntry<'_>, &str, Option<u64>),
    (b, kind_b, size_b): (&DirEntry<'_>, &str, Option<u64>),
) -> Ordering {
    let folders_first = b.is_browsable().cmp(&a.is_browsable());
    if folders_first != Ordering::Equal {
        return folders_first;
    }
    let column = match spec.column {
        // `Relevance` and `Folder` order search results, which the engine sorts; a folder
        // listing never uses them (`Listing::set_sort` ignores them there). Ordering by name keeps
        // this comparator total for every column all the same.
        SortColumn::Name | SortColumn::Relevance | SortColumn::Folder => {
            compare_names(a.name, b.name)
        }
        // `Option`'s order puts `None` first.
        SortColumn::Modified => a.modified.cmp(&b.modified),
        SortColumn::Size => size_a.cmp(&size_b),
        SortColumn::ResourceFork => a.resource_size.cmp(&b.resource_size),
        SortColumn::Kind => kind_a.cmp(kind_b),
    };
    let column = if spec.descending {
        column.reverse()
    } else {
        column
    };
    column.then_with(|| compare_names(a.name, b.name))
}

/// The inverse of a display order: where each row id sits in it. Built on the background
/// executor together with the order, so the UI thread finds a row's position in O(1) instead of
/// scanning the order (the cursor after a re-sort, type-to-select).
///
/// It holds the order it was built from and answers only for that one ([`position_in`] checks
/// by pointer), so it can never be out of step with the order shown.
#[derive(Debug, Clone)]
pub struct Positions {
    /// The order this is the inverse of.
    order: Arc<[u32]>,
    /// `slots[id]` is the position of row `id` plus one; 0 for an id not in the order. Sized by
    /// the largest id in the order, so a few hits of a large index stay small (and the untouched
    /// zero pages of the allocation are never written).
    slots: Box<[u32]>,
}

impl Positions {
    /// The inverse of `order`. O(order) with one write per row; run it off the UI thread.
    /// `None` when a position would not fit in a `u32` (more than `u32::MAX - 1` rows, which ids
    /// of `u32` cannot reach without repeats).
    pub fn new(order: Arc<[u32]>) -> Option<Self> {
        if u32::try_from(order.len()).ok()? == u32::MAX {
            return None;
        }
        let len = order
            .iter()
            .max()
            .map_or(0, |&max| (max as usize).saturating_add(1));
        let mut slots = vec![0u32; len].into_boxed_slice();
        for (position, &id) in order.iter().enumerate() {
            // `position < order.len() < u32::MAX`, so `position + 1` fits.
            if let (Some(slot), Ok(stored)) = (
                slots.get_mut(id as usize),
                u32::try_from(position.saturating_add(1)),
            ) {
                *slot = stored;
            }
        }
        Some(Self { order, slots })
    }

    /// The order this is the inverse of.
    pub fn order(&self) -> &Arc<[u32]> {
        &self.order
    }

    /// The position of row `id` in [`Self::order`]; `None` when it is not there. O(1).
    pub fn get(&self, id: u32) -> Option<usize> {
        let stored = *self.slots.get(id as usize)?;
        let position = (stored as usize).checked_sub(1)?;
        // A repeated id keeps its last position; either way the answer holds `id`.
        (self.order.get(position) == Some(&id)).then_some(position)
    }
}

/// The position of row `id` in `order`: O(1) through `positions` when they were built from this
/// very order, a scan of the `u32` order otherwise (a folder still loading).
pub fn position_in(order: &Arc<[u32]>, positions: Option<&Positions>, id: u32) -> Option<usize> {
    match positions {
        Some(positions) if Arc::ptr_eq(positions.order(), order) => positions.get(id),
        _ => order.iter().position(|&row| row == id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_atlas_lib::fs::{EntryFlags, NodeId, NodeKind, Timestamp};

    fn less(a: &str, b: &str) {
        assert_eq!(compare_names(a, b), Ordering::Less, "{a:?} < {b:?}");
        assert_eq!(compare_names(b, a), Ordering::Greater, "{b:?} > {a:?}");
    }

    #[test]
    fn natural_order_examples() {
        less("a2", "a10");
        less("a10", "a10b");
        less("a2", "a10b");
        less("file2", "file10");
        less("file9.txt", "file10.txt");
        less("x", "x1");
        less("1", "a");
        less("", "a");
        less("", "0");
        assert_eq!(compare_names("", ""), Ordering::Equal);
        assert_eq!(compare_names("same", "same"), Ordering::Equal);
    }

    #[test]
    fn case_and_leading_zeros_are_deterministic_but_never_equal() {
        // Case-insensitive first: "apple" sorts before "Banana" regardless of case.
        less("apple", "Banana");
        less("Apple", "banana");
        // Equal apart from case or zeros: decided by bytes, consistently.
        assert_eq!(compare_tokens("A", "a"), Ordering::Equal);
        assert_eq!(compare_names("A", "a"), "A".cmp("a"));
        assert_ne!(compare_names("A", "a"), Ordering::Equal);
        assert_eq!(compare_tokens("01", "1"), Ordering::Equal);
        assert_ne!(compare_names("01", "1"), Ordering::Equal);
        assert_eq!(compare_names("01", "1"), compare_names("1", "01").reverse());
        // Leading zeros do not make a number larger.
        less("009", "10");
        less("a0001b", "a2b");
    }

    #[test]
    fn non_ascii() {
        assert_eq!(compare_tokens("\u{c4}", "\u{e4}"), Ordering::Equal); // Ä, ä
        assert_ne!(compare_names("\u{c4}", "\u{e4}"), Ordering::Equal);
        // Code point order after folding: ä (U+00E4) is after z.
        less("z", "\u{c4}");
        less("z", "\u{e4}");
        less("\u{e4}a", "\u{e4}b");
        less("\u{c4}a", "\u{e4}B");
        // Mixed ASCII and multi-byte at the same position.
        less("a", "\u{e9}");
        less("caf\u{e9}2", "caf\u{e9}10");
        // Capital I with dot folds to "i".
        assert_eq!(compare_tokens("\u{130}", "i"), Ordering::Equal);
        // Kelvin sign folds to "k".
        assert_eq!(compare_tokens("\u{212a}", "K"), Ordering::Equal);
    }

    #[test]
    fn long_digit_runs_beyond_u64() {
        let big = "99999999999999999999999999999999";
        let bigger = "100000000000000000000000000000000";
        less(big, bigger);
        less(&format!("v{big}"), &format!("v{bigger}"));
        less(
            "123456789012345678901234567890",
            "123456789012345678901234567891",
        );
        less(&format!("0000{big}"), bigger);
        assert_eq!(compare_tokens(&format!("000{big}"), big), Ordering::Equal);
    }

    /// Deterministic xorshift generator for the property checks.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    fn generated_names(count: usize, seed: u64) -> Vec<String> {
        const PIECES: &[&str] = &[
            "a",
            "A",
            "b",
            "B",
            "z",
            "0",
            "00",
            "1",
            "01",
            "2",
            "9",
            "10",
            "99",
            " ",
            ".",
            "_",
            "\u{c4}",
            "\u{e4}",
            "\u{df}",
            "\u{130}",
            "i",
            "\u{212a}",
            "k",
            "\u{65e5}",
            "\u{1f600}",
        ];
        let mut rng = Rng(seed);
        (0..count)
            .map(|_| {
                let len = rng.below(6);
                (0..len).map(|_| PIECES[rng.below(PIECES.len())]).collect()
            })
            .collect()
    }

    #[test]
    fn comparator_is_a_strict_total_order() {
        let names = generated_names(400, 0x9e37_79b9_7f4a_7c15);
        // Antisymmetric, and Equal only for identical strings.
        for a in &names {
            for b in &names {
                let ab = compare_names(a, b);
                assert_eq!(ab, compare_names(b, a).reverse(), "{a:?} vs {b:?}");
                assert_eq!(ab == Ordering::Equal, a == b, "{a:?} vs {b:?}");
            }
        }
        // Transitive on sampled triples.
        let mut rng = Rng(42);
        for _ in 0..200_000 {
            let a = &names[rng.below(names.len())];
            let b = &names[rng.below(names.len())];
            let c = &names[rng.below(names.len())];
            if compare_names(a, b) != Ordering::Greater && compare_names(b, c) != Ordering::Greater
            {
                assert_ne!(
                    compare_names(a, c),
                    Ordering::Greater,
                    "{a:?} <= {b:?} <= {c:?}"
                );
            }
        }
        // Sorting does not panic, and the result is consistent for every pair.
        let mut sorted = names.clone();
        sorted.sort_unstable_by(|a, b| compare_names(a, b));
        for (i, a) in sorted.iter().enumerate() {
            for b in &sorted[i + 1..] {
                assert_ne!(compare_names(a, b), Ordering::Greater, "{a:?} before {b:?}");
            }
        }
        // The same order comes out of a stable sort of a shuffled copy.
        let mut other = names;
        other.reverse();
        other.sort_by(|a, b| compare_names(a, b));
        assert_eq!(sorted, other);
    }

    #[test]
    fn starts_with_folded_matches_case_insensitively() {
        assert!(starts_with_folded("Readme.md", "read"));
        assert!(starts_with_folded("readme", "README"));
        assert!(starts_with_folded("\u{c4}pfel", "\u{e4}p"));
        assert!(starts_with_folded("anything", ""));
        assert!(!starts_with_folded("re", "read"));
        assert!(!starts_with_folded("bread", "read"));
    }

    fn entry(name: &str, kind: NodeKind, size: Option<u64>, modified: Option<i64>) -> DirEntry<'_> {
        DirEntry {
            node: NodeId(1),
            name,
            kind,
            size,
            resource_size: None,
            modified: modified.map(Timestamp::from_unix_nanos),
            flags: EntryFlags::EMPTY,
        }
    }

    #[test]
    fn folders_first_in_both_directions() {
        let folder = entry("zzz", NodeKind::Directory, None, None);
        let link = DirEntry {
            flags: EntryFlags::LINKS_TO_DIRECTORY,
            ..entry("yyy", NodeKind::Symlink, None, None)
        };
        let plain_link = entry("aaa-link", NodeKind::Symlink, None, None);
        let file = entry("aaa", NodeKind::File, Some(5), Some(1));
        for column in [
            SortColumn::Name,
            SortColumn::Modified,
            SortColumn::Size,
            SortColumn::ResourceFork,
            SortColumn::Kind,
        ] {
            for descending in [false, true] {
                let spec = SortSpec { column, descending };
                assert_eq!(compare_entries(spec, &folder, &file), Ordering::Less);
                assert_eq!(compare_entries(spec, &file, &folder), Ordering::Greater);
                assert_eq!(compare_entries(spec, &link, &file), Ordering::Less);
                assert_eq!(compare_entries(spec, &link, &plain_link), Ordering::Less);
            }
        }
    }

    #[test]
    fn none_is_lowest_and_descending_reverses_only_the_column() {
        let asc = |column| SortSpec {
            column,
            descending: false,
        };
        let desc = |column| SortSpec {
            column,
            descending: true,
        };
        let unknown = entry("b", NodeKind::File, None, None);
        let small = entry("c", NodeKind::File, Some(0), Some(-5));
        let large = entry("a", NodeKind::File, Some(10), Some(7));
        for column in [SortColumn::Size, SortColumn::Modified] {
            assert_eq!(
                compare_entries(asc(column), &unknown, &small),
                Ordering::Less
            );
            assert_eq!(compare_entries(asc(column), &small, &large), Ordering::Less);
            assert_eq!(
                compare_entries(desc(column), &small, &unknown),
                Ordering::Less
            );
            assert_eq!(
                compare_entries(desc(column), &large, &small),
                Ordering::Less
            );
        }
        // Ties fall back to the name ascending, in both directions.
        let a = entry("a", NodeKind::File, Some(3), None);
        let b = entry("b", NodeKind::File, Some(3), None);
        assert_eq!(
            compare_entries(asc(SortColumn::Size), &a, &b),
            Ordering::Less
        );
        assert_eq!(
            compare_entries(desc(SortColumn::Size), &a, &b),
            Ordering::Less
        );
        // Name descending.
        assert_eq!(
            compare_entries(desc(SortColumn::Name), &b, &a),
            Ordering::Less
        );
    }

    #[test]
    fn resource_fork_column_orders_like_size() {
        let asc = SortSpec {
            column: SortColumn::ResourceFork,
            descending: false,
        };
        let desc = SortSpec {
            descending: true,
            ..asc
        };
        let with = |name, resource_size| DirEntry {
            resource_size,
            // The data size points the other way, so only the fork can decide.
            ..entry(
                name,
                NodeKind::File,
                Some(1000 - resource_size.unwrap_or(0)),
                None,
            )
        };
        let unknown = with("b", None);
        let empty = with("c", Some(0));
        let large = with("a", Some(10));
        assert_eq!(compare_entries(asc, &unknown, &empty), Ordering::Less);
        assert_eq!(compare_entries(asc, &empty, &large), Ordering::Less);
        assert_eq!(compare_entries(desc, &empty, &unknown), Ordering::Less);
        assert_eq!(compare_entries(desc, &large, &empty), Ordering::Less);
        // Ties fall back to the name ascending, in both directions.
        let (x, y) = (with("x", Some(4)), with("y", Some(4)));
        assert_eq!(compare_entries(asc, &x, &y), Ordering::Less);
        assert_eq!(compare_entries(desc, &x, &y), Ordering::Less);
    }

    #[test]
    fn results_only_columns_order_folders_like_name() {
        let entries = [
            entry("b", NodeKind::File, Some(1), None),
            entry("A", NodeKind::File, Some(9), None),
            entry("z", NodeKind::Directory, None, None),
            entry("a10", NodeKind::File, None, Some(3)),
            entry("a2", NodeKind::File, None, Some(4)),
        ];
        for column in [SortColumn::Relevance, SortColumn::Folder] {
            for descending in [false, true] {
                let spec = SortSpec { column, descending };
                let by_name = SortSpec {
                    column: SortColumn::Name,
                    descending,
                };
                for a in &entries {
                    for b in &entries {
                        assert_eq!(
                            compare_entries(spec, a, b),
                            compare_entries(by_name, a, b),
                            "{spec:?} {} {}",
                            a.name,
                            b.name
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn kind_column_uses_the_kind_text() {
        let spec = SortSpec {
            column: SortColumn::Kind,
            descending: false,
        };
        let png = entry("z.png", NodeKind::File, None, None); // "PNG image"
        let doc = entry("a.unknown", NodeKind::File, None, None); // "Document"
        assert_eq!(compare_entries(spec, &doc, &png), Ordering::Less);
        // The precomputed form agrees.
        assert_eq!(
            compare_entries_with_kinds(spec, &doc, "Document", &png, "PNG image"),
            Ordering::Less
        );
    }

    #[test]
    fn positions_invert_the_order() {
        let order: Arc<[u32]> = Arc::from(vec![7u32, 0, 3, 12, 5]);
        let positions = Positions::new(Arc::clone(&order)).unwrap();
        for (position, &id) in order.iter().enumerate() {
            assert_eq!(positions.get(id), Some(position));
            assert_eq!(position_in(&order, Some(&positions), id), Some(position));
        }
        // Ids not in the order, inside and past the table.
        for id in [1, 2, 4, 6, 8, 11, 13, u32::MAX] {
            assert_eq!(positions.get(id), None, "{id}");
            assert_eq!(position_in(&order, Some(&positions), id), None, "{id}");
        }
        // Sized by the largest id, not by the order's length times anything.
        assert_eq!(positions.slots.len(), 13);
    }

    #[test]
    fn positions_of_empty_and_repeated_orders() {
        let empty: Arc<[u32]> = Arc::from(Vec::<u32>::new());
        let positions = Positions::new(Arc::clone(&empty)).unwrap();
        assert_eq!(positions.get(0), None);
        assert_eq!(position_in(&empty, Some(&positions), 0), None);
        // A repeated id (no order holds one, but nothing breaks): an answer that holds the id.
        let repeated: Arc<[u32]> = Arc::from(vec![4u32, 4, 2]);
        let positions = Positions::new(Arc::clone(&repeated)).unwrap();
        let at = positions.get(4).unwrap();
        assert_eq!(repeated[at], 4);
        assert_eq!(positions.get(2), Some(2));
        // The largest id there is.
        let big: Arc<[u32]> = Arc::from(vec![u32::MAX - 1, 0]);
        let positions = Positions::new(Arc::clone(&big)).unwrap();
        assert_eq!(positions.get(u32::MAX - 1), Some(0));
    }

    #[test]
    fn positions_of_another_order_are_not_used() {
        let old: Arc<[u32]> = Arc::from(vec![1u32, 2, 3]);
        let positions = Positions::new(Arc::clone(&old)).unwrap();
        // The same ids in another order (a new allocation): the scan answers, not the stale table.
        let new: Arc<[u32]> = Arc::from(vec![3u32, 1, 2]);
        assert_eq!(position_in(&new, Some(&positions), 3), Some(0));
        assert_eq!(position_in(&new, Some(&positions), 2), Some(2));
        assert_eq!(position_in(&new, None, 1), Some(1));
        assert_eq!(position_in(&new, None, 9), None);
    }

    #[test]
    fn positions_match_a_scan_on_random_orders() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for round in 0..50 {
            let len = (next() % 2_000) as usize;
            let spread = 1 + next() % 5_000;
            let mut ids: Vec<u32> = (0..len)
                .map(|i| (i as u64 * spread % 1_000_003) as u32)
                .collect();
            ids.sort_unstable();
            ids.dedup();
            for i in (1..ids.len()).rev() {
                let j = (next() % (i as u64 + 1)) as usize;
                ids.swap(i, j);
            }
            let order: Arc<[u32]> = Arc::from(ids);
            let positions = Positions::new(Arc::clone(&order)).unwrap();
            for probe in 0..300 {
                let id = (next() % 1_000_010) as u32;
                let id = if probe % 2 == 0 && !order.is_empty() {
                    order[id as usize % order.len()]
                } else {
                    id
                };
                assert_eq!(
                    position_in(&order, Some(&positions), id),
                    order.iter().position(|&x| x == id),
                    "round {round} id {id}"
                );
            }
        }
    }
}
