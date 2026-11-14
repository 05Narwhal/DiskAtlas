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

//! Case folding that keeps every name's byte length.
//!
//! The index stores every name twice: `names` as it is on disk, `folded` as
//! the query engine searches it. Both buffers share one offset array, so a
//! match range found in `folded` is, byte for byte, the highlight range in the
//! original name. That only works if folding never changes a name's byte
//! length, so the fold lowercases a character only when its lowercase form is
//! a single character of the same UTF-8 length. Every other character is kept
//! as it is.
//!
//! The consequence is intended: a handful of characters whose lowercase form
//! differs in length (`İ` U+0130, the KELVIN SIGN `K` U+212A, `ẞ` U+1E9E) fold
//! to themselves and so match only themselves.

/// The folded form of one character, always the same `len_utf8()` as `c`.
fn fold_char(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(l), None) if l.len_utf8() == c.len_utf8() => l,
        _ => c,
    }
}

/// Appends the folded form of `name` to `out`. Always appends exactly
/// `name.len()` bytes.
pub fn fold_into(name: &str, out: &mut Vec<u8>) {
    out.reserve(name.len());
    let mut rest = name;
    loop {
        // A run of ASCII bytes needs no decoding.
        let run_len = rest
            .bytes()
            .position(|b| !b.is_ascii())
            .unwrap_or(rest.len());
        let Some((run, tail)) = rest.split_at_checked(run_len) else {
            return;
        };
        out.extend(run.bytes().map(|b| b.to_ascii_lowercase()));
        let Some(c) = tail.chars().next() else {
            return;
        };
        let mut buf = [0u8; 4];
        out.extend_from_slice(fold_char(c).encode_utf8(&mut buf).as_bytes());
        match tail.get(c.len_utf8()..) {
            Some(next) => rest = next,
            None => return,
        }
    }
}

/// The folded form of `text`, with the same rule as [`fold_into`]. For
/// queries.
pub fn fold(text: &str) -> String {
    text.chars().map(fold_char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folded(name: &str) -> String {
        let mut out = Vec::new();
        fold_into(name, &mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn every_scalar_value_keeps_its_length() {
        let mut buf = Vec::new();
        let mut text = String::new();
        for cp in 0..=0x10FFFFu32 {
            let Some(c) = char::from_u32(cp) else {
                continue;
            };
            text.clear();
            text.push(c);
            buf.clear();
            fold_into(&text, &mut buf);
            assert_eq!(buf.len(), c.len_utf8(), "U+{cp:04X}");
            assert!(std::str::from_utf8(&buf).is_ok(), "U+{cp:04X}");
            assert_eq!(fold(&text).as_bytes(), buf.as_slice(), "U+{cp:04X}");
        }
    }

    #[test]
    fn ascii() {
        assert_eq!(folded("Hello WORLD.TXT"), "hello world.txt");
        assert_eq!(folded("0123 -_.~!@#"), "0123 -_.~!@#");
        assert_eq!(fold("Hello WORLD.TXT"), "hello world.txt");
    }

    #[test]
    fn latin1() {
        assert_eq!(folded("ÉCOLE Ñandú"), "école ñandú");
        assert_eq!(fold("ÉCOLE Ñandú"), "école ñandú");
    }

    #[test]
    fn greek() {
        // Per character: capital sigma becomes σ, never the final form ς.
        assert_eq!(folded("ΣΟΦΙΑ"), "σοφια");
        assert_eq!(folded("ΟΔΥΣΣΕΥΣ"), "οδυσσευσ");
        assert_eq!(fold("ΣΟΦΙΑ"), "σοφια");
    }

    #[test]
    fn cyrillic() {
        assert_eq!(folded("ПРИВЕТ"), "привет");
        assert_eq!(fold("ПРИВЕТ"), "привет");
    }

    #[test]
    fn length_changing_characters_fold_to_themselves() {
        for s in ["İ", "\u{212A}", "ẞ"] {
            assert_eq!(folded(s), s);
            assert_eq!(fold(s), s);
        }
        assert_eq!(folded("İSTANBUL"), "İstanbul");
        assert_eq!(folded("STRAẞE"), "straẞe");
    }

    #[test]
    fn mixed() {
        assert_eq!(
            folded("Café ПРИВЕТ 日本語 ΣΟΦΙΑ.Txt"),
            "café привет 日本語 σοφια.txt"
        );
        assert_eq!(folded("a😀B"), "a😀b");
        let name = "Ünïcödé İ K ẞ Файл.DOC";
        assert_eq!(folded(name).len(), name.len());
        assert_eq!(fold(name), folded(name));
    }

    #[test]
    fn appends_without_clearing() {
        let mut out = b"keep".to_vec();
        fold_into("AbC", &mut out);
        fold_into("Ñ", &mut out);
        assert_eq!(out, "keepabcñ".as_bytes());
    }

    #[test]
    fn empty_input() {
        let mut out = Vec::new();
        fold_into("", &mut out);
        assert!(out.is_empty());
        assert_eq!(fold(""), "");
    }
}
