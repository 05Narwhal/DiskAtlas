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

//! Ordering search hits by a column.
//!
//! The rules follow the app's folder listings (`state/order.rs`):
//! browsable entries (folders and links to folders) first in both
//! directions, then the column, with `descending` reversing only the column,
//! then the name ascending. Names compare case-insensitively with digit runs
//! as numbers, here over the index's folded bytes. A final tie-break on the
//! entry number makes the order total, so the parallel unstable sort is
//! deterministic.

use std::cmp::Ordering;
use std::sync::Arc;
use std::sync::atomic::{self, AtomicBool};

use rayon::prelude::*;

use crate::search::index::{Index, IndexFlags, NO_RSIZE, NO_SIZE};
use crate::search::kind::KindClass;
use crate::search::query::Hits;

/// The column search results are ordered by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HitOrder {
    /// Natural, case-insensitive name order.
    Name,
    /// Modification date, unknown lowest.
    Modified,
    /// Data size, unknown (and folders) lowest.
    Size,
    /// Resource fork size, none lowest.
    ResourceSize,
    /// [`KindClass::label`], then the extension, then the name.
    Kind,
    /// The folder holding the entry, in path order.
    Folder,
}

/// The entries of `hits` ordered by `by`. `None` when `cancel` is set
/// before or after the sort (a sort cannot be interrupted; at 5 million
/// hits it takes under a second on 8 cores).
pub fn order_hits(
    index: &Index,
    hits: &Hits,
    by: HitOrder,
    descending: bool,
    cancel: &AtomicBool,
) -> Option<Arc<[u32]>> {
    if cancel.load(atomic::Ordering::Relaxed) {
        return None;
    }
    let columns = Columns::new(index);
    let mut entries = hits.by_entry().to_vec();
    entries.par_sort_unstable_by(|&a, &b| columns.compare(by, descending, a, b));
    if cancel.load(atomic::Ordering::Relaxed) {
        return None;
    }
    Some(entries.into())
}

/// The arrays a comparison reads, and the rank of each kind class by its
/// label.
struct Columns<'a> {
    names: &'a [u8],
    folded: &'a [u8],
    offsets: &'a [u32],
    parents: &'a [u32],
    sizes: &'a [u64],
    times: &'a [i64],
    rsizes: &'a [u32],
    flags: &'a [u16],
    /// Position of each `KindClass` (by its bits) in label order.
    kind_rank: [u8; 16],
}

impl<'a> Columns<'a> {
    fn new(index: &'a Index) -> Self {
        let mut by_label = KindClass::ALL;
        by_label.sort_by_key(|c| c.label());
        let mut kind_rank = [u8::MAX; 16];
        for (rank, class) in (0u8..).zip(by_label) {
            if let Some(slot) = kind_rank.get_mut(usize::from(class.bits())) {
                *slot = rank;
            }
        }
        Self {
            names: index.names_bytes(),
            folded: index.folded_bytes(),
            offsets: index.offsets(),
            parents: index.parents(),
            sizes: index.sizes(),
            times: index.modified_times(),
            rsizes: index.rsizes(),
            flags: index.flag_words(),
            kind_rank,
        }
    }

    /// The bytes of name `e` in `buf` (`names` or `folded`); empty when out
    /// of range.
    fn name<'b>(&self, buf: &'b [u8], e: u32) -> &'b [u8] {
        let k = e as usize;
        let start = self.offsets.get(k).map(|&o| o as usize);
        let end = k
            .checked_add(1)
            .and_then(|k| self.offsets.get(k))
            .and_then(|&o| (o as usize).checked_sub(1));
        match (start, end) {
            (Some(s), Some(t)) => buf.get(s..t).unwrap_or(&[]),
            _ => &[],
        }
    }

    fn flags(&self, e: u32) -> u16 {
        self.flags.get(e as usize).copied().unwrap_or(0)
    }

    /// Folders and symbolic links to folders.
    fn browsable(&self, e: u32) -> bool {
        let f = self.flags(e);
        match f & IndexFlags::KIND_MASK {
            IndexFlags::KIND_FOLDER => true,
            IndexFlags::KIND_SYMLINK => f & IndexFlags::LINKS_TO_FOLDER != 0,
            _ => false,
        }
    }

    /// Size as `Option`'s order sees it: unknown lowest.
    fn size(&self, e: u32) -> Option<u64> {
        self.sizes
            .get(e as usize)
            .copied()
            .filter(|&s| s != NO_SIZE)
    }

    fn rsize(&self, e: u32) -> Option<u32> {
        self.rsizes
            .get(e as usize)
            .copied()
            .filter(|&s| s != NO_RSIZE)
    }

    /// `NO_TIME` is `i64::MIN`, already the lowest value.
    fn time(&self, e: u32) -> i64 {
        self.times.get(e as usize).copied().unwrap_or(i64::MIN)
    }

    fn kind_rank(&self, e: u32) -> u8 {
        let class = IndexFlags::from_bits(self.flags(e)).kind_class();
        self.kind_rank
            .get(usize::from(class.bits()))
            .copied()
            .unwrap_or(u8::MAX)
    }

    /// The folded bytes after the last dot, when the dot is not the first
    /// byte (as `kind::classify` reads an extension); empty otherwise.
    fn extension(&self, e: u32) -> &'a [u8] {
        let name = self.name(self.folded, e);
        match memchr::memrchr(b'.', name) {
            Some(dot) if dot > 0 => name.get(dot.saturating_add(1)..).unwrap_or(&[]),
            _ => &[],
        }
    }

    /// Name order: natural over the folded bytes, then the original bytes,
    /// as the app's `compare_names`.
    fn by_name(&self, a: u32, b: u32) -> Ordering {
        natural(self.name(self.folded, a), self.name(self.folded, b))
            .then_with(|| self.name(self.names, a).cmp(self.name(self.names, b)))
    }

    fn compare(&self, by: HitOrder, descending: bool, a: u32, b: u32) -> Ordering {
        let first = self.browsable(b).cmp(&self.browsable(a));
        if first != Ordering::Equal {
            return first;
        }
        let column = match by {
            HitOrder::Name => self.by_name(a, b),
            HitOrder::Modified => self.time(a).cmp(&self.time(b)),
            HitOrder::Size => self.size(a).cmp(&self.size(b)),
            HitOrder::ResourceSize => self.rsize(a).cmp(&self.rsize(b)),
            HitOrder::Kind => self
                .kind_rank(a)
                .cmp(&self.kind_rank(b))
                .then_with(|| self.extension(a).cmp(self.extension(b))),
            HitOrder::Folder => {
                let parent = |e: u32| self.parents.get(e as usize).copied().unwrap_or(u32::MAX);
                parent(a).cmp(&parent(b))
            }
        };
        let column = if descending { column.reverse() } else { column };
        column
            .then_with(|| self.by_name(a, b))
            .then_with(|| a.cmp(&b))
    }
}

/// End of the run of ASCII digits starting at `i`.
fn digit_run_end(s: &[u8], i: usize) -> usize {
    let run = s.get(i..).map_or(0, |rest| {
        rest.iter().take_while(|b| b.is_ascii_digit()).count()
    });
    i.saturating_add(run)
}

/// The digits of `s[i..end]` without leading zeros.
fn significant(s: &[u8], i: usize, end: usize) -> &[u8] {
    let digits = s.get(i..end).unwrap_or(&[]);
    let zeros = digits.iter().take_while(|&&d| d == b'0').count();
    digits.get(zeros..).unwrap_or(&[])
}

/// Natural order over folded bytes, the rule of the app's `compare_tokens`:
/// two ASCII digit runs compare by value (length after leading zeros, then
/// digits), anything else byte by byte, a shorter sequence first. Comparing
/// UTF-8 byte by byte is comparing characters by code point, and a digit
/// never occurs inside a multi-byte character, so this agrees with the
/// app's character-wise rule on the same folded text.
fn natural(a: &[u8], b: &[u8]) -> Ordering {
    let (mut i, mut j) = (0usize, 0usize);
    loop {
        let (x, y) = match (a.get(i), b.get(j)) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(&x), Some(&y)) => (x, y),
        };
        if x.is_ascii_digit() && y.is_ascii_digit() {
            let (end_a, end_b) = (digit_run_end(a, i), digit_run_end(b, j));
            let (num_a, num_b) = (significant(a, i, end_a), significant(b, j, end_b));
            let by_value = num_a.len().cmp(&num_b.len()).then_with(|| num_a.cmp(num_b));
            if by_value != Ordering::Equal {
                return by_value;
            }
            i = end_a;
            j = end_b;
        } else {
            if x != y {
                return x.cmp(&y);
            }
            i = i.saturating_add(1);
            j = j.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::query::Query;
    use crate::search::query::fixtures::*;

    fn all_hits(index: &Index) -> Hits {
        let never = AtomicBool::new(false);
        // Every entry but the root: a filter-only query scoped to the root.
        index
            .search(
                &Query {
                    scope: Some(0),
                    include_hidden: true,
                    ..Query::default()
                },
                None,
                &never,
            )
            .unwrap()
    }

    fn ordered(index: &Index, by: HitOrder, descending: bool) -> Vec<String> {
        let never = AtomicBool::new(false);
        order_hits(index, &all_hits(index), by, descending, &never)
            .unwrap()
            .iter()
            .map(|&e| index.name(e).to_owned())
            .collect()
    }

    fn sample() -> Index {
        index(&[
            root("vol"),
            dir("b folder", 0),                               // 1
            file("file10.txt", 1).size(50).time(30).rsize(5), // 2
            file("File2.txt", 1).size(500).time(10),          // 3
            dir("A folder", 0),                               // 4
            file("file1.png", 4).no_size().time(20).rsize(0), // 5
            link("link", 0).links_to_folder(),                // 6
            link("plain link", 0),                            // 7
            file("zeta.rs", 0).size(7),                       // 8
            file("file02.txt", 0).size(50).time(30),          // 9
        ])
    }

    #[test]
    fn name_natural_folders_first() {
        let index = sample();
        assert_eq!(
            ordered(&index, HitOrder::Name, false),
            [
                "A folder",
                "b folder",
                "link",
                "file1.png",
                "File2.txt",
                "file02.txt",
                "file10.txt",
                "plain link",
                "zeta.rs"
            ]
        );
        assert_eq!(
            ordered(&index, HitOrder::Name, true),
            [
                "link",
                "b folder",
                "A folder",
                "zeta.rs",
                "plain link",
                "file10.txt",
                "file02.txt",
                "File2.txt",
                "file1.png"
            ]
        );
    }

    #[test]
    fn natural_rule() {
        let n = |a: &str, b: &str| natural(a.as_bytes(), b.as_bytes());
        assert_eq!(n("file2", "file10"), Ordering::Less);
        assert_eq!(n("file02", "file2"), Ordering::Equal);
        assert_eq!(n("file002x", "file2y"), Ordering::Less);
        assert_eq!(n("a", "a1"), Ordering::Less);
        assert_eq!(n("1", "a"), Ordering::Less);
        assert_eq!(n("z", "\u{e9}"), Ordering::Less);
        assert_eq!(n("\u{e9}1", "\u{e9}01"), Ordering::Equal);
        assert_eq!(
            n("99999999999999999999999", "100000000000000000000000"),
            Ordering::Less
        );
        assert_eq!(n("", ""), Ordering::Equal);
    }

    #[test]
    fn numeric_columns_unknown_lowest() {
        let index = sample();
        // Folders first (no size, so by name), then the rest: unknown
        // (file1.png and the plain link), 7, 50, 50, 500.
        assert_eq!(
            ordered(&index, HitOrder::Size, false),
            [
                "A folder",
                "b folder",
                "link",
                "file1.png",
                "plain link",
                "zeta.rs",
                "file02.txt",
                "file10.txt",
                "File2.txt"
            ]
        );
        assert_eq!(
            ordered(&index, HitOrder::Size, true),
            [
                "A folder",
                "b folder",
                "link",
                "File2.txt",
                "file02.txt",
                "file10.txt",
                "zeta.rs",
                "file1.png",
                "plain link"
            ]
        );
        // Unknown dates lowest; ties by name.
        assert_eq!(
            ordered(&index, HitOrder::Modified, false),
            [
                "A folder",
                "b folder",
                "link",
                "plain link",
                "zeta.rs",
                "File2.txt",
                "file1.png",
                "file02.txt",
                "file10.txt"
            ]
        );
        assert_eq!(
            ordered(&index, HitOrder::Modified, true),
            [
                "A folder",
                "b folder",
                "link",
                "file02.txt",
                "file10.txt",
                "file1.png",
                "File2.txt",
                "plain link",
                "zeta.rs"
            ]
        );
        // None lowest, then 0, then 5.
        assert_eq!(
            ordered(&index, HitOrder::ResourceSize, false),
            [
                "A folder",
                "b folder",
                "link",
                "File2.txt",
                "file02.txt",
                "plain link",
                "zeta.rs",
                "file1.png",
                "file10.txt"
            ]
        );
        assert_eq!(
            ordered(&index, HitOrder::ResourceSize, true)[3..5],
            ["file10.txt", "file1.png"]
        );
    }

    #[test]
    fn kind_by_label_then_extension() {
        let index = sample();
        // Files: Code (zeta.rs), Document (txt ×3), Image (png), Other
        // (links). Labels sort "Code and data" < "Document" < "Other" <
        // "Picture".
        assert_eq!(
            ordered(&index, HitOrder::Kind, false),
            [
                "A folder",
                "b folder",
                "link",
                "zeta.rs",
                "File2.txt",
                "file02.txt",
                "file10.txt",
                "plain link",
                "file1.png"
            ]
        );
        assert_eq!(
            ordered(&index, HitOrder::Kind, true),
            [
                "link",
                "A folder",
                "b folder",
                "file1.png",
                "plain link",
                "File2.txt",
                "file02.txt",
                "file10.txt",
                "zeta.rs"
            ]
        );
        let ext = crate::search::query::fixtures::index(&[
            root("vol"),
            file("b.TXT", 0),
            file("a.txt", 0),
            file("c.md", 0),
            file(".txt", 0),
        ]);
        // Same class (Document) apart from ".txt" (no extension: Other).
        assert_eq!(
            ordered(&ext, HitOrder::Kind, false),
            ["c.md", "a.txt", "b.TXT", ".txt"]
        );
    }

    #[test]
    fn folder_is_path_order() {
        let index = sample();
        assert_eq!(
            ordered(&index, HitOrder::Folder, false),
            [
                "A folder",
                "b folder",
                "link",
                "file02.txt",
                "plain link",
                "zeta.rs",
                "File2.txt",
                "file10.txt",
                "file1.png"
            ]
        );
        let down = ordered(&index, HitOrder::Folder, true);
        assert_eq!(
            &down[3..],
            [
                "file1.png",
                "File2.txt",
                "file10.txt",
                "file02.txt",
                "plain link",
                "zeta.rs"
            ]
        );
    }

    #[test]
    fn ties_by_entry_and_only_hits() {
        // Same name in two folders: Name ties go to the entry number.
        let index = index(&[
            root("vol"),
            dir("x", 0),
            file("same", 1),
            dir("y", 0),
            file("same", 3),
            file("other", 3),
        ]);
        let never = AtomicBool::new(false);
        let hits = index
            .search(
                &Query {
                    text: "same".into(),
                    ..Query::default()
                },
                None,
                &never,
            )
            .unwrap();
        let up = order_hits(&index, &hits, HitOrder::Name, false, &never).unwrap();
        let down = order_hits(&index, &hits, HitOrder::Name, true, &never).unwrap();
        assert_eq!(&up[..], [2, 4]);
        assert_eq!(&down[..], [2, 4]);
        let by_size = order_hits(&index, &hits, HitOrder::Size, true, &never).unwrap();
        assert_eq!(&by_size[..], [2, 4]);
    }

    #[test]
    fn matches_a_reference_sort() {
        let index = index(&generated(5_000, 5));
        let hits = all_hits(&index);
        let never = AtomicBool::new(false);
        for by in [
            HitOrder::Name,
            HitOrder::Modified,
            HitOrder::Size,
            HitOrder::ResourceSize,
            HitOrder::Kind,
            HitOrder::Folder,
        ] {
            for descending in [false, true] {
                let got = order_hits(&index, &hits, by, descending, &never).unwrap();
                assert_eq!(got.len(), hits.len());
                for w in got.windows(2) {
                    let (a, b) = (index.entry(w[0]).unwrap(), index.entry(w[1]).unwrap());
                    assert!(a.is_browsable() >= b.is_browsable(), "{by:?}");
                    if a.is_browsable() != b.is_browsable() {
                        continue;
                    }
                    let column = match by {
                        HitOrder::Size => a.size.cmp(&b.size),
                        HitOrder::Modified => a.modified.cmp(&b.modified),
                        HitOrder::ResourceSize => a.resource_size.cmp(&b.resource_size),
                        HitOrder::Folder => index.parent(w[0]).cmp(&index.parent(w[1])),
                        HitOrder::Kind => index
                            .kind_class(w[0])
                            .label()
                            .cmp(index.kind_class(w[1]).label()),
                        HitOrder::Name => Ordering::Equal,
                    };
                    let column = if descending { column.reverse() } else { column };
                    assert_ne!(
                        column,
                        Ordering::Greater,
                        "{by:?} {descending} {} {}",
                        a.name,
                        b.name
                    );
                }
                // A strict order: every neighbour pair compares Less.
                let columns = Columns::new(&index);
                for w in got.windows(2) {
                    assert_eq!(columns.compare(by, descending, w[0], w[1]), Ordering::Less);
                }
            }
        }
    }

    #[test]
    fn cancellation() {
        let index = sample();
        let hits = all_hits(&index);
        let cancel = AtomicBool::new(true);
        assert!(order_hits(&index, &hits, HitOrder::Name, false, &cancel).is_none());
        let empty = Hits::default();
        let never = AtomicBool::new(false);
        assert!(
            order_hits(&index, &empty, HitOrder::Size, false, &never)
                .unwrap()
                .is_empty()
        );
    }
}
