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

//! Making volume names legal and unique on the host. Built in M5 task 3b.
//!
//! A name read from a volume is hostile: it can hold path separators, NUL,
//! `..`, a drive letter or a Windows device name. [`sanitize`] is what stands
//! between such a name and a path outside the destination folder, and
//! [`NameSet::unique`] keeps two volume names from landing on one host name.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;

use super::HostRules;

/// Longest name, in UTF-16 code units (Windows, macOS) or bytes (Unix).
pub const MAX_NAME: usize = 255;

/// Longest extension, in characters, that shortening keeps.
const MAX_EXTENSION_CHARS: usize = 16;

/// Windows device names. A name whose part before the first dot is one of
/// these (any case) opens the device instead of a file.
const RESERVED: [&str; 30] = [
    "CON",
    "PRN",
    "AUX",
    "NUL",
    "COM0",
    "COM1",
    "COM2",
    "COM3",
    "COM4",
    "COM5",
    "COM6",
    "COM7",
    "COM8",
    "COM9",
    "LPT0",
    "LPT1",
    "LPT2",
    "LPT3",
    "LPT4",
    "LPT5",
    "LPT6",
    "LPT7",
    "LPT8",
    "LPT9",
    "COM\u{B9}",
    "COM\u{B2}",
    "COM\u{B3}",
    "LPT\u{B9}",
    "LPT\u{B2}",
    "LPT\u{B3}",
];

/// The console names, reserved the same way.
const RESERVED_CONSOLE: [&str; 2] = ["CONIN$", "CONOUT$"];

/// `name` made legal as one path component under `rules`.
///
/// In this order: forbidden characters replaced, trailing dots and spaces
/// replaced (Windows), `.` and `..` replaced, an empty name replaced,
/// Windows device names prefixed with `_`, and the result shortened to
/// [`MAX_NAME`] units, keeping a short extension. Applying it twice gives
/// the same result as applying it once.
pub fn sanitize(name: &str, rules: HostRules) -> Cow<'_, str> {
    let mut out: Cow<'_, str> = Cow::Borrowed(name);

    if name.chars().any(|c| replacement(c, rules).is_some()) {
        out = Cow::Owned(
            name.chars()
                .map(|c| replacement(c, rules).unwrap_or(c))
                .collect(),
        );
    }

    if rules == HostRules::Windows {
        out = fix_trailing(out);
    }

    match &*out {
        "." => out = Cow::Borrowed("_"),
        ".." => out = Cow::Borrowed("__"),
        "" => out = Cow::Borrowed("_"),
        _ => {}
    }

    if rules == HostRules::Windows && is_reserved(&out) {
        out = Cow::Owned(format!("_{out}"));
    }

    if measure(&out, rules) > MAX_NAME {
        let (stem, extension) = split_extension(&out);
        let budget = MAX_NAME.saturating_sub(measure(extension, rules));
        let mut short = String::with_capacity(MAX_NAME);
        short.push_str(cut(stem, budget, rules));
        short.push_str(extension);
        let short = if rules == HostRules::Windows {
            // Cutting a name with no extension can leave a dot or a space
            // at the end.
            fix_trailing(Cow::Owned(short)).into_owned()
        } else {
            short
        };
        out = Cow::Owned(short);
    }
    out
}

/// What character `c` becomes under `rules`, or `None` when it is allowed.
fn replacement(c: char, rules: HostRules) -> Option<char> {
    match rules {
        HostRules::Windows => match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '\u{0}'..='\u{1F}' => Some('_'),
            _ => None,
        },
        HostRules::Unix => match c {
            '/' | '\u{0}' => Some('_'),
            _ => None,
        },
        HostRules::MacOs => match c {
            '\u{0}' => Some('_'),
            // The Finder shows a stored `:` as `/`, so this round-trips.
            '/' => Some(':'),
            _ => None,
        },
    }
}

/// Replaces a run of dots and spaces at the end of `name` by one `_`.
fn fix_trailing(name: Cow<'_, str>) -> Cow<'_, str> {
    let kept = name.trim_end_matches(['.', ' ']).len();
    if kept == name.len() {
        return name;
    }
    let mut fixed = String::with_capacity(kept.saturating_add(1));
    fixed.push_str(name.get(..kept).unwrap_or_default());
    fixed.push('_');
    Cow::Owned(fixed)
}

/// True when `name` would name a Windows device. The part before the first
/// dot is compared without trailing spaces, as Win32 path parsing drops them.
fn is_reserved(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or_default();
    let base = base.trim_end_matches(' ');
    RESERVED
        .iter()
        .chain(RESERVED_CONSOLE.iter())
        .any(|device| eq_ignore_ascii_case(base, device))
}

/// Case-insensitive comparison for the device names, which are ASCII apart
/// from the superscript digits (compared exactly).
fn eq_ignore_ascii_case(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.chars()
            .zip(b.chars())
            .all(|(x, y)| x.eq_ignore_ascii_case(&y))
}

/// True when `name` is no longer than [`MAX_NAME`] as `rules` counts it.
pub(crate) fn fits(name: &str, rules: HostRules) -> bool {
    measure(name, rules) <= MAX_NAME
}

/// Length of `s` as `rules` counts it.
fn measure(s: &str, rules: HostRules) -> usize {
    match rules {
        HostRules::Unix => s.len(),
        HostRules::Windows | HostRules::MacOs => s.encode_utf16().count(),
    }
}

/// Length of `c` as `rules` counts it.
fn char_measure(c: char, rules: HostRules) -> usize {
    match rules {
        HostRules::Unix => c.len_utf8(),
        HostRules::Windows | HostRules::MacOs => c.len_utf16(),
    }
}

/// The longest prefix of `s` that fits in `budget` units, cut at a
/// character boundary.
fn cut(s: &str, budget: usize, rules: HostRules) -> &str {
    let mut used: usize = 0;
    for (at, c) in s.char_indices() {
        let next = used.saturating_add(char_measure(c, rules));
        if next > budget {
            return s.get(..at).unwrap_or_default();
        }
        used = next;
    }
    s
}

/// Splits `name` into a stem and an extension that includes its dot. The
/// extension is the part after the last dot when it is 1 to 16 characters
/// and the dot is not the first character; otherwise it is empty.
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(dot) if dot > 0 => {
            let extension = name.get(dot..).unwrap_or_default();
            let chars = extension.chars().count().saturating_sub(1);
            if (1..=MAX_EXTENSION_CHARS).contains(&chars) {
                (name.get(..dot).unwrap_or_default(), extension)
            } else {
                (name, "")
            }
        }
        _ => (name, ""),
    }
}

/// A name folded for comparison: every host is treated as case-insensitive.
fn fold(name: &str) -> String {
    name.chars().flat_map(char::to_lowercase).collect()
}

/// Tracks the names used in one host folder and makes each new one unique.
#[derive(Debug, Clone, Default)]
pub struct NameSet {
    /// Folded names in use.
    taken: HashSet<String>,
    /// For a folded name asked for more than once, the next number to try,
    /// so a folder of many equal names does not retry from 2 each time.
    next: HashMap<String, u64>,
}

impl NameSet {
    /// An empty set, for a folder the copy has just made.
    pub fn new() -> Self {
        Self::default()
    }

    /// The names already in `dir`. Used for the destination folder only;
    /// a folder made by the copy starts empty.
    pub fn seeded_from_dir(dir: &Path) -> io::Result<Self> {
        let mut set = Self::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            set.taken.insert(fold(&entry.file_name().to_string_lossy()));
        }
        Ok(set)
    }

    /// `name` (already sanitized under `rules`) if it is free, or else the
    /// first free `stem (n).ext` for n from 2, shortened to fit. The result
    /// is recorded as taken.
    pub fn unique(&mut self, name: &str, rules: HostRules) -> String {
        self.unique_for(name, rules, rules)
    }

    /// [`NameSet::unique`] for a name that must also fit `also`'s length
    /// limit (the host's, when `rules` is another platform's).
    pub fn unique_for(&mut self, name: &str, rules: HostRules, also: HostRules) -> String {
        let folded = fold(name);
        if !self.taken.contains(&folded) {
            self.taken.insert(folded);
            return name.to_owned();
        }
        let (stem, extension) = split_extension(name);
        let mut n = self.next.get(&folded).copied().unwrap_or(2);
        let mut candidate = String::new();
        // Each try is a different name, so one of the first `len + 1` is free.
        for _ in 0..=self.taken.len() {
            candidate = numbered(stem, n, extension, rules);
            if also != rules && !fits(&candidate, also) {
                candidate = numbered(stem, n, extension, also);
            }
            n = n.saturating_add(1);
            let candidate_folded = fold(&candidate);
            if !self.taken.contains(&candidate_folded) {
                self.taken.insert(candidate_folded);
                break;
            }
        }
        self.next.insert(folded, n);
        candidate
    }

    /// Records `name` as taken. False, and nothing recorded, when it already is.
    pub(crate) fn claim(&mut self, name: &str) -> bool {
        self.taken.insert(fold(name))
    }
}

/// `stem (n)extension`, with the stem cut so the whole fits [`MAX_NAME`].
fn numbered(stem: &str, n: u64, extension: &str, rules: HostRules) -> String {
    let suffix = format!(" ({n})");
    let budget = MAX_NAME
        .saturating_sub(measure(&suffix, rules))
        .saturating_sub(measure(extension, rules));
    let stem = cut(stem, budget, rules);
    let mut out = String::with_capacity(
        stem.len()
            .saturating_add(suffix.len())
            .saturating_add(extension.len()),
    );
    out.push_str(stem);
    out.push_str(&suffix);
    out.push_str(extension);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [HostRules; 3] = [HostRules::Windows, HostRules::Unix, HostRules::MacOs];

    fn s(name: &str, rules: HostRules) -> String {
        sanitize(name, rules).into_owned()
    }

    #[test]
    fn character_table() {
        let cases: &[(&str, &str, &str, &str)] = &[
            // name, Windows, Unix, macOS
            ("plain.txt", "plain.txt", "plain.txt", "plain.txt"),
            ("a<b>c", "a_b_c", "a<b>c", "a<b>c"),
            ("a:b", "a_b", "a:b", "a:b"),
            ("a\"b|c?d*e", "a_b_c_d_e", "a\"b|c?d*e", "a\"b|c?d*e"),
            ("a/b", "a_b", "a_b", "a:b"),
            ("a\\b", "a_b", "a\\b", "a\\b"),
            ("a\0b", "a_b", "a_b", "a_b"),
            (
                "a\u{1}b\u{1F}c",
                "a_b_c",
                "a\u{1}b\u{1F}c",
                "a\u{1}b\u{1F}c",
            ),
            ("tab\there", "tab_here", "tab\there", "tab\there"),
            ("\u{7F}", "\u{7F}", "\u{7F}", "\u{7F}"),
            ("caf\u{E9}", "caf\u{E9}", "caf\u{E9}", "caf\u{E9}"),
        ];
        for &(name, win, unix, mac) in cases {
            assert_eq!(s(name, HostRules::Windows), win, "{name:?} windows");
            assert_eq!(s(name, HostRules::Unix), unix, "{name:?} unix");
            assert_eq!(s(name, HostRules::MacOs), mac, "{name:?} macos");
        }
    }

    #[test]
    fn dots_spaces_and_empty() {
        let cases: &[(&str, &str, &str)] = &[
            // name, Windows, Unix and macOS
            ("a.", "a_", "a."),
            ("a ", "a_", "a "),
            (" ", "_", " "),
            ("a. .", "a_", "a. ."),
            ("a..", "a_", "a.."),
            (".", "_", "_"),
            // Windows: the trailing rule comes first and turns ".." into "_".
            ("..", "_", "__"),
            ("...", "_", "..."),
            ("", "_", "_"),
            (".hidden", ".hidden", ".hidden"),
            (" lead", " lead", " lead"),
            ("a .b", "a .b", "a .b"),
        ];
        for &(name, win, other) in cases {
            assert_eq!(s(name, HostRules::Windows), win, "{name:?} windows");
            assert_eq!(s(name, HostRules::Unix), other, "{name:?} unix");
            assert_eq!(s(name, HostRules::MacOs), other, "{name:?} macos");
        }
    }

    #[test]
    fn every_reserved_name() {
        for device in RESERVED.iter().chain(RESERVED_CONSOLE.iter()) {
            let lower = device.to_lowercase();
            for name in [
                device.to_string(),
                lower.clone(),
                format!("{device}.txt"),
                format!("{lower}.tar.gz"),
                format!("{device} .txt"),
            ] {
                assert_eq!(
                    s(&name, HostRules::Windows),
                    format!("_{name}"),
                    "{name:?} windows"
                );
                assert_eq!(s(&name, HostRules::Unix), name, "{name:?} unix");
                assert_eq!(s(&name, HostRules::MacOs), name, "{name:?} macos");
            }
        }
        // The trailing dot is replaced first, which leaves no device name.
        assert_eq!(s("NUL.", HostRules::Windows), "NUL_");
        for name in [
            "COM10",
            "CONSOLE",
            "xCON",
            "LPT",
            "COM",
            "AUX1",
            "CON-1.txt",
            "COM\u{B4}",
        ] {
            assert_eq!(s(name, HostRules::Windows), name, "{name:?}");
        }
    }

    #[test]
    fn length_in_utf16_units_with_surrogates_at_the_cut() {
        for rules in [HostRules::Windows, HostRules::MacOs] {
            let fits = format!("{}\u{1D11E}", "a".repeat(253));
            assert_eq!(measure(&fits, rules), 255);
            assert_eq!(s(&fits, rules), fits);
            // 256 units, the last two one character: it goes whole.
            let over = format!("{}\u{1D11E}", "a".repeat(254));
            assert_eq!(s(&over, rules), "a".repeat(254));
            // The extension stays; the stem is cut at a character boundary.
            let stem = "\u{1D11E}".repeat(130);
            let long = format!("{stem}.txt");
            let got = s(&long, rules);
            assert!(got.ends_with(".txt"));
            assert_eq!(got, format!("{}.txt", "\u{1D11E}".repeat(125)));
            assert!(measure(&got, rules) <= 255);
        }
    }

    #[test]
    fn length_in_bytes_for_unix() {
        let fits = "\u{E9}".repeat(127) + "a";
        assert_eq!(fits.len(), 255);
        assert_eq!(s(&fits, HostRules::Unix), fits);
        let over = "\u{E9}".repeat(128);
        assert_eq!(over.len(), 256);
        assert_eq!(s(&over, HostRules::Unix), "\u{E9}".repeat(127));
        // 128 two-byte characters fit Windows' 255 units unchanged.
        assert_eq!(s(&over, HostRules::Windows), over);
        let long = format!("{}.jpeg", "\u{20AC}".repeat(100));
        let got = s(&long, HostRules::Unix);
        assert_eq!(got, format!("{}.jpeg", "\u{20AC}".repeat(83)));
    }

    #[test]
    fn long_extension_is_not_kept() {
        let long = format!("{}.{}", "a".repeat(250), "b".repeat(17));
        let got = s(&long, HostRules::Unix);
        assert_eq!(got.len(), 255);
        assert!(got.starts_with(&"a".repeat(250)));
    }

    #[test]
    fn windows_cut_never_ends_in_a_dot() {
        // The extension is too long to keep, so the cut lands after the dot.
        let name = format!("{}.{}", "a".repeat(254), "b".repeat(20));
        let got = s(&name, HostRules::Windows);
        assert_eq!(got, format!("{}_", "a".repeat(254)));
        // A short extension is kept, spaces and all.
        let name = format!("{}. x", "a".repeat(254));
        let got = s(&name, HostRules::Windows);
        assert_eq!(got, format!("{}. x", "a".repeat(252)));
    }

    #[test]
    fn collision_chains() {
        for rules in ALL {
            let mut set = NameSet::new();
            assert_eq!(set.unique("A", rules), "A");
            assert_eq!(set.unique("a", rules), "a (2)");
            assert_eq!(set.unique("a (2)", rules), "a (2) (2)");
            assert_eq!(set.unique("a", rules), "a (3)");
            assert_eq!(set.unique("A", rules), "A (4)");
            assert_eq!(set.unique("note.txt", rules), "note.txt");
            assert_eq!(set.unique("NOTE.TXT", rules), "NOTE (2).TXT");
            assert_eq!(set.unique("note (3).txt", rules), "note (3).txt");
            assert_eq!(set.unique("note.txt", rules), "note (4).txt");
            assert_eq!(set.unique(".bashrc", rules), ".bashrc");
            assert_eq!(set.unique(".bashrc", rules), ".bashrc (2)");
        }
    }

    #[test]
    fn unique_reshortens_the_stem() {
        let mut set = NameSet::new();
        let long = "x".repeat(255);
        assert_eq!(set.unique(&long, HostRules::Windows), long);
        let second = set.unique(&long, HostRules::Windows);
        assert_eq!(second, format!("{} (2)", "x".repeat(251)));
        let with_ext = format!("{}.txt", "y".repeat(251));
        assert_eq!(set.unique(&with_ext, HostRules::Unix), with_ext);
        let second = set.unique(&with_ext, HostRules::Unix);
        assert_eq!(second, format!("{} (2).txt", "y".repeat(247)));
        assert_eq!(second.len(), 255);
        // A run of a thousand equal names stays linear and stays unique.
        let mut seen = HashSet::new();
        for _ in 0..1000 {
            assert!(seen.insert(fold(&set.unique("same", HostRules::MacOs))));
        }
    }

    #[test]
    fn unique_for_fits_the_second_rule_set_too() {
        let mut set = NameSet::new();
        // 255 bytes, 85 UTF-16 units: legal under Windows rules and Unix rules.
        let name = "€".repeat(85);
        assert_eq!(
            set.unique_for(&name, HostRules::Windows, HostRules::Unix),
            name
        );
        let second = set.unique_for(&name, HostRules::Windows, HostRules::Unix);
        assert_ne!(second, name);
        assert!(fits(&second, HostRules::Unix), "{} bytes", second.len());
        assert!(second.ends_with(" (2)"));
        // Without the second rule set the same call overflows a Unix host.
        let mut plain = NameSet::new();
        plain.unique(&name, HostRules::Windows);
        assert!(!fits(
            &plain.unique(&name, HostRules::Windows),
            HostRules::Unix
        ));
    }

    #[test]
    fn seeded_from_dir_and_claim() {
        let dir = std::env::temp_dir().join(format!("disk-atlas-names-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("Report.pdf"), b"x").unwrap();
        std::fs::create_dir(dir.join("Folder")).unwrap();
        let mut set = NameSet::seeded_from_dir(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(set.unique("report.pdf", HostRules::Unix), "report (2).pdf");
        assert_eq!(set.unique("folder", HostRules::Unix), "folder (2)");
        assert!(set.claim("._x"));
        assert!(!set.claim("._X"));
        assert!(NameSet::seeded_from_dir(&dir.join("missing")).is_err());
    }

    /// xorshift64*: a fixed sequence, so a failure reproduces.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    #[test]
    fn sanitize_is_idempotent_and_legal() {
        const ALPHABET: &[char] = &[
            'a',
            'C',
            'O',
            'N',
            'L',
            'P',
            'T',
            'U',
            'X',
            '1',
            '$',
            '.',
            ' ',
            '/',
            '\\',
            ':',
            '\0',
            '\u{1}',
            '\u{1F}',
            '<',
            '*',
            '?',
            '"',
            '|',
            '\u{E9}',
            '\u{B9}',
            '\u{20AC}',
            '\u{1D11E}',
            '(',
            '2',
            ')',
        ];
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for round in 0..20_000 {
            let len = match round % 4 {
                0 => (rng.next() % 8) as usize,
                1 => (rng.next() % 40) as usize,
                _ => 240 + (rng.next() % 40) as usize,
            };
            let name: String = (0..len)
                .map(|_| ALPHABET[(rng.next() % ALPHABET.len() as u64) as usize])
                .collect();
            for rules in ALL {
                let once = s(&name, rules);
                let twice = s(&once, rules);
                assert_eq!(once, twice, "{name:?} {rules:?}");
                assert!(!once.is_empty() && once != "." && once != "..");
                assert!(measure(&once, rules) <= MAX_NAME, "{name:?} {rules:?}");
                assert!(!once.contains(['/', '\0']), "{name:?} {rules:?}");
                if rules == HostRules::Windows {
                    assert!(!once.ends_with(['.', ' ']));
                    assert!(!is_reserved(&once));
                    assert!(!once.contains(['\\', ':', '<', '*', '?', '"', '|', '\u{1}']));
                }
                let mut set = NameSet::new();
                set.unique(&once, rules);
                let again = set.unique(&once, rules);
                assert_eq!(s(&again, rules), again, "{name:?} {rules:?}");
            }
        }
    }
}
