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

//! Generates `crates/lib/src/fs/hfsplus/unicode/tables.rs`.
//!
//! Usage:
//!
//! ```text
//! hfs_unicode [--check] [--lower-case PATH] [--out PATH] UNICODE_DATA COMPOSITION_EXCLUSIONS
//! ```
//!
//! `UNICODE_DATA` and `COMPOSITION_EXCLUSIONS` are the Unicode 3.2 files
//! `UnicodeData-3.2.0.txt` and `CompositionExclusions-3.2.0.txt`. Both are
//! checked against pinned SHA-256 digests before they are parsed. The lower
//! case table defaults to `tn1150_lower_case.txt` next to this crate's
//! manifest, and the output to the checked-in `tables.rs`.
//!
//! With `--check` nothing is written: the tables are generated in memory and
//! compared with the file at the output path. Exit status 0 means identical,
//! 1 means different, 2 means an error.

mod sha256;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// SHA-256 of `UnicodeData-3.2.0.txt` from https://www.unicode.org/Public/3.2-Update/.
const UNICODE_DATA_SHA256: &str =
    "5e444028b6e76d96f9dc509609c5e3222bf609056f35e5fcde7e6fb8a58cd446";
/// SHA-256 of `CompositionExclusions-3.2.0.txt` from the same directory.
const COMPOSITION_EXCLUSIONS_SHA256: &str =
    "1d3a450d0f39902710df4972ac4a60ec31fbcb54ffd4d53cd812fc1200c732cb";
/// SHA-256 of the 2816 `gLowerCaseTable` values as big-endian `u16`s, computed
/// from `FastUnicodeCompare.c` as extracted from TN1150's download. Guards the
/// transcription in `tn1150_lower_case.txt` against edits.
const LOWER_CASE_VALUES_SHA256: &str =
    "01d87d06619efc3ec97f28daa8d93ef63b6335d9a131ff372a8881e1f7a4ff6c";

/// Entries in `gLowerCaseTable`: the 256-entry high-byte index and ten pages.
const LOWER_CASE_LEN: usize = 11 * 256;
/// Input files are a few hundred KiB; anything far larger is not ours.
const MAX_INPUT_BYTES: u64 = 4 << 20;
/// Canonical decompositions in Unicode 3.2 nest at most a few levels.
const MAX_DECOMPOSITION_DEPTH: u32 = 8;

const HANGUL_FIRST: u32 = 0xAC00;
const HANGUL_LAST: u32 = 0xD7A3;

type Result<T> = std::result::Result<T, String>;

struct Args {
    check: bool,
    lower_case: PathBuf,
    out: PathBuf,
    unicode_data: PathBuf,
    composition_exclusions: PathBuf,
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("hfs_unicode: {e}");
            ExitCode::from(2)
        }
    }
}

/// Returns `Ok(false)` only when `--check` finds a difference.
fn run() -> Result<bool> {
    let args = parse_args()?;

    let unicode_data = read_pinned(&args.unicode_data, UNICODE_DATA_SHA256)?;
    let exclusions = read_pinned(&args.composition_exclusions, COMPOSITION_EXCLUSIONS_SHA256)?;
    let lower_case_text = read_text(&args.lower_case)?;

    let lower_case = parse_lower_case(&lower_case_text)?;
    let ucd = parse_unicode_data(&unicode_data)?;
    let excluded = parse_exclusions(&exclusions)?;
    let tables = build(&ucd, &excluded)?;
    let generated = emit(&lower_case, &tables);

    if args.check {
        let current = read_text(&args.out)?;
        // Line endings may be rewritten on checkout; the content may not.
        if current.replace("\r\n", "\n") == generated {
            println!("{} is up to date", args.out.display());
            Ok(true)
        } else {
            eprintln!(
                "{} differs from the generator's output; regenerate it",
                args.out.display()
            );
            Ok(false)
        }
    } else {
        std::fs::write(&args.out, generated)
            .map_err(|e| format!("writing {}: {e}", args.out.display()))?;
        println!("wrote {}", args.out.display());
        Ok(true)
    }
}

fn parse_args() -> Result<Args> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut check = false;
    let mut lower_case = manifest_dir.join("tn1150_lower_case.txt");
    let mut out = manifest_dir.join("../../crates/lib/src/fs/hfsplus/unicode/tables.rs");
    let mut positional = Vec::new();

    let mut it = std::env::args_os().skip(1);
    while let Some(arg) = it.next() {
        match arg.to_str() {
            Some("--check") => check = true,
            Some("--lower-case") => {
                lower_case = it.next().ok_or("--lower-case needs a path")?.into();
            }
            Some("--out") => out = it.next().ok_or("--out needs a path")?.into(),
            Some(s) if s.starts_with("--") => return Err(format!("unknown option {s}")),
            _ => positional.push(PathBuf::from(arg)),
        }
    }
    let [unicode_data, composition_exclusions]: [PathBuf; 2] =
        positional.try_into().map_err(|_| {
            "usage: hfs_unicode [--check] [--lower-case PATH] [--out PATH] \
             UNICODE_DATA COMPOSITION_EXCLUSIONS"
                .to_string()
        })?;
    Ok(Args {
        check,
        lower_case,
        out,
        unicode_data,
        composition_exclusions,
    })
}

fn read_bytes(path: &Path) -> Result<Vec<u8>> {
    let len = std::fs::metadata(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .len();
    if len > MAX_INPUT_BYTES {
        return Err(format!("{}: {len} bytes is too large", path.display()));
    }
    std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn read_text(path: &Path) -> Result<String> {
    String::from_utf8(read_bytes(path)?).map_err(|_| format!("{}: not UTF-8", path.display()))
}

/// Reads a downloaded input and refuses it unless its digest is the pinned one.
fn read_pinned(path: &Path, sha256: &str) -> Result<String> {
    let bytes = read_bytes(path)?;
    let digest = sha256::hex_digest(&bytes);
    if digest != sha256 {
        return Err(format!(
            "{}: SHA-256 {digest} does not match the pinned {sha256}",
            path.display()
        ));
    }
    String::from_utf8(bytes).map_err(|_| format!("{}: not UTF-8", path.display()))
}

/// Parses a code point (surrogates allowed: UnicodeData lists their ranges).
fn parse_hex(token: &str, what: &str) -> Result<u32> {
    if token.is_empty() || token.len() > 6 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("{what}: bad hex value {token:?}"));
    }
    let v = u32::from_str_radix(token, 16).map_err(|e| format!("{what}: {token:?}: {e}"))?;
    if v > 0x10FFFF {
        return Err(format!("{what}: {token} is past U+10FFFF"));
    }
    Ok(v)
}

/// Parses a code point that must be a Unicode scalar value.
fn parse_scalar(token: &str, what: &str) -> Result<u32> {
    let v = parse_hex(token, what)?;
    if char::from_u32(v).is_none() {
        return Err(format!("{what}: {token} is not a Unicode scalar value"));
    }
    Ok(v)
}

/// Strips `//` line comments and `/* */` block comments.
fn strip_c_comments(text: &str) -> Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let line = rest.find("//");
        let block = rest.find("/*");
        match (line, block) {
            (None, None) => {
                out.push_str(rest);
                return Ok(out);
            }
            (Some(l), b) if b.is_none_or(|b| l < b) => {
                out.push_str(&rest[..l]);
                rest = rest[l..].find('\n').map_or("", |n| &rest[l + n..]);
            }
            (_, Some(b)) => {
                out.push_str(&rest[..b]);
                let end = rest[b + 2..]
                    .find("*/")
                    .ok_or("unterminated /* comment in lower case table")?;
                out.push(' ');
                rest = &rest[b + 2 + end + 2..];
            }
            (Some(_), None) => unreachable!("handled by the guard above"),
        }
    }
}

/// Parses the TN1150 `gLowerCaseTable` transcription and checks its shape.
fn parse_lower_case(text: &str) -> Result<Vec<u16>> {
    let body = strip_c_comments(text)?;
    let mut values = Vec::with_capacity(LOWER_CASE_LEN);
    for token in body
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
    {
        let hex = token
            .strip_prefix("0x")
            .filter(|h| h.len() == 4 && h.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| format!("lower case table: unexpected token {token:?}"))?;
        let v = u16::from_str_radix(hex, 16)
            .map_err(|e| format!("lower case table: {token:?}: {e}"))?;
        values.push(v);
    }
    if values.len() != LOWER_CASE_LEN {
        return Err(format!(
            "lower case table: {} values, expected {LOWER_CASE_LEN}",
            values.len()
        ));
    }
    let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_be_bytes()).collect();
    let digest = sha256::hex_digest(&bytes);
    if digest != LOWER_CASE_VALUES_SHA256 {
        return Err(format!(
            "lower case table: values hash to {digest}, expected {LOWER_CASE_VALUES_SHA256}"
        ));
    }
    // Every page index must start a whole page inside the table, so the
    // engine can form `page | low_byte` without arithmetic.
    for (hi, &page) in values.iter().take(256).enumerate() {
        if page != 0 && (page % 256 != 0 || usize::from(page) + 256 > LOWER_CASE_LEN) {
            return Err(format!(
                "lower case table: high byte {hi:#04x} has page index {page:#06x}"
            ));
        }
    }
    Ok(values)
}

/// The parts of `UnicodeData.txt` the tables need.
struct Ucd {
    /// Non-zero canonical combining classes.
    combining_class: BTreeMap<u32, u8>,
    /// One level of canonical (not compatibility) decomposition.
    decomposition: BTreeMap<u32, Vec<u32>>,
}

impl Ucd {
    fn class(&self, cp: u32) -> u8 {
        self.combining_class.get(&cp).copied().unwrap_or(0)
    }
}

fn parse_unicode_data(text: &str) -> Result<Ucd> {
    let mut ucd = Ucd {
        combining_class: BTreeMap::new(),
        decomposition: BTreeMap::new(),
    };
    let mut previous: Option<u32> = None;
    for (n, line) in text.lines().enumerate() {
        let what = format!("UnicodeData line {}", n + 1);
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(';').collect();
        if fields.len() != 15 {
            return Err(format!("{what}: {} fields, expected 15", fields.len()));
        }
        let cp = parse_hex(fields[0], &what)?;
        if previous.is_some_and(|p| p >= cp) {
            return Err(format!("{what}: code points out of order"));
        }
        previous = Some(cp);

        let class: u8 = fields[3]
            .parse()
            .map_err(|e| format!("{what}: combining class {:?}: {e}", fields[3]))?;
        if class != 0 {
            ucd.combining_class.insert(cp, class);
        }

        let decomposition = fields[5];
        if decomposition.is_empty() || decomposition.starts_with('<') {
            continue; // none, or a compatibility decomposition
        }
        let parts = decomposition
            .split_whitespace()
            .map(|t| parse_scalar(t, &what))
            .collect::<Result<Vec<u32>>>()?;
        if parts.is_empty() || parts.len() > 2 {
            return Err(format!(
                "{what}: canonical decomposition of {} parts",
                parts.len()
            ));
        }
        if (HANGUL_FIRST..=HANGUL_LAST).contains(&cp) {
            return Err(format!(
                "{what}: Hangul syllables are decomposed algorithmically"
            ));
        }
        ucd.decomposition.insert(cp, parts);
    }
    if ucd.decomposition.is_empty() {
        return Err("UnicodeData: no canonical decompositions found".to_string());
    }
    Ok(ucd)
}

fn parse_exclusions(text: &str) -> Result<BTreeSet<u32>> {
    let mut set = BTreeSet::new();
    for (n, line) in text.lines().enumerate() {
        let data = line.split('#').next().unwrap_or("").trim();
        if data.is_empty() {
            continue;
        }
        let cp = parse_scalar(data, &format!("CompositionExclusions line {}", n + 1))?;
        set.insert(cp);
    }
    Ok(set)
}

/// TN1150: u+2000-u+2FFF and u+F900-u+FAFF are left undecomposed. Nothing
/// outside the BMP is decomposed either: the macOS converter works on 16-bit
/// code units. Must match `hfs_excluded` in the engine's `unicode.rs`.
fn apple_excluded(cp: u32) -> bool {
    (0x2000..=0x2FFF).contains(&cp) || (0xF900..=0xFAFF).contains(&cp) || cp >= 0x10000
}

/// Full (recursive) canonical decomposition. With `apple`, excluded code
/// points are left as they are at every level.
fn full_decomposition(
    ucd: &Ucd,
    cp: u32,
    apple: bool,
    depth: u32,
    out: &mut Vec<u32>,
) -> Result<()> {
    if depth > MAX_DECOMPOSITION_DEPTH {
        return Err(format!("decomposition of {cp:04X} nests too deeply"));
    }
    match ucd.decomposition.get(&cp) {
        Some(parts) if !(apple && apple_excluded(cp)) => {
            for &p in parts {
                full_decomposition(ucd, p, apple, depth + 1, out)?;
            }
        }
        _ => out.push(cp),
    }
    Ok(())
}

/// Stable sort of each run of non-starters by combining class.
fn canonical_order(ucd: &Ucd, cps: &mut [u32]) {
    for run in cps.split_mut(|&c| ucd.class(c) == 0) {
        run.sort_by_key(|&c| ucd.class(c));
    }
}

struct Tables {
    decomp_keys: Vec<u32>,
    decomp_index: Vec<u16>,
    decomp_data: Vec<u16>,
    combining_class: Vec<(u32, u32, u8)>,
    compose: Vec<(u16, u16, u16)>,
}

fn build(ucd: &Ucd, excluded: &BTreeSet<u32>) -> Result<Tables> {
    // Decompositions: the full standard canonical decomposition of every code
    // point that has one. The engine applies Apple's exclusions at the top
    // level only, which is the same as applying them at every level as long
    // as no other decomposition passes through an excluded code point. Check
    // that here so the single table serves both `decompose` and NFC.
    let mut decomp_keys = Vec::new();
    let mut decomp_index = vec![0u16];
    let mut decomp_data: Vec<u16> = Vec::new();
    for &cp in ucd.decomposition.keys() {
        let mut standard = Vec::new();
        full_decomposition(ucd, cp, false, 0, &mut standard)?;
        if !apple_excluded(cp) {
            let mut apple = Vec::new();
            full_decomposition(ucd, cp, true, 0, &mut apple)?;
            if apple != standard {
                return Err(format!(
                    "{cp:04X}: decomposition passes through an Apple-excluded code point; \
                     the engine's single table cannot express it"
                ));
            }
        }
        canonical_order(ucd, &mut standard);
        for &p in &standard {
            let ch = char::from_u32(p).ok_or_else(|| format!("{p:04X} is not a scalar value"))?;
            let mut buf = [0u16; 2];
            decomp_data.extend_from_slice(ch.encode_utf16(&mut buf));
        }
        decomp_keys.push(cp);
        decomp_index.push(
            u16::try_from(decomp_data.len()).map_err(|_| "decomposition data over 65535 units")?,
        );
    }

    // Combining classes as ranges of consecutive code points with one class.
    let mut combining_class: Vec<(u32, u32, u8)> = Vec::new();
    for (&cp, &class) in &ucd.combining_class {
        match combining_class.last_mut() {
            Some((_, last, c)) if *c == class && last.checked_add(1) == Some(cp) => *last = cp,
            _ => combining_class.push((cp, cp, class)),
        }
    }

    // Primary composites: canonical pairs that are not fully excluded from
    // composition (explicit exclusions, singletons, non-starter decompositions).
    let mut compose = Vec::new();
    for (&cp, parts) in &ucd.decomposition {
        let &[first, second] = parts.as_slice() else {
            continue; // singleton
        };
        if excluded.contains(&cp) || ucd.class(cp) != 0 || ucd.class(first) != 0 {
            continue;
        }
        let narrow = |v: u32| {
            u16::try_from(v)
                .map_err(|_| format!("composition pair {first:04X} {second:04X} is not BMP"))
        };
        compose.push((narrow(first)?, narrow(second)?, narrow(cp)?));
    }
    compose.sort_unstable();
    if compose
        .windows(2)
        .any(|w| (w[0].0, w[0].1) == (w[1].0, w[1].1))
    {
        return Err("two composites share a decomposition".to_string());
    }

    Ok(Tables {
        decomp_keys,
        decomp_index,
        decomp_data,
        combining_class,
        compose,
    })
}

const HEADER: &str = "\
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
";

fn emit_list<T>(out: &mut String, items: &[T], per_line: usize, fmt: impl Fn(&T) -> String) {
    for chunk in items.chunks(per_line) {
        out.push_str("   ");
        for item in chunk {
            out.push(' ');
            out.push_str(&fmt(item));
            out.push(',');
        }
        out.push('\n');
    }
}

fn emit(lower_case: &[u16], t: &Tables) -> String {
    let mut s = String::new();
    s.push_str(HEADER);
    s.push_str(
        "\n\
//! Tables for HFS+ name handling. Generated by `tools/hfs_unicode`; do not edit.
//!
//! Inputs:
//! - `LOWER_CASE`: TN1150 `gLowerCaseTable` (`tools/hfs_unicode/tn1150_lower_case.txt`).
//! - Everything else: Unicode 3.2 `UnicodeData-3.2.0.txt` and
//!   `CompositionExclusions-3.2.0.txt` (digests pinned in the generator).
//!
//! Regenerate or verify with the commands in `tools/hfs_unicode/README.md`.
",
    );

    let _ = write!(
        s,
        "\n\
/// TN1150 `gLowerCaseTable`, unchanged: entries 0..256 map a high byte to the
/// start of its 256-entry page (0 means the byte's characters fold to
/// themselves); a folded value of 0 marks an ignorable character.
#[rustfmt::skip]
pub(super) static LOWER_CASE: [u16; {}] = [
",
        lower_case.len()
    );
    emit_list(&mut s, lower_case, 16, |v| format!("0x{v:04X}"));
    s.push_str("];\n");

    let _ = write!(
        s,
        "\n\
/// Code points with a canonical decomposition in Unicode 3.2 (Hangul
/// syllables excluded: they decompose algorithmically), ascending.
#[rustfmt::skip]
pub(super) static DECOMP_KEYS: [u32; {}] = [
",
        t.decomp_keys.len()
    );
    emit_list(&mut s, &t.decomp_keys, 8, |v| format!("0x{v:05X}"));
    s.push_str("];\n");

    let _ = write!(
        s,
        "\n\
/// `DECOMP_DATA[DECOMP_INDEX[i]..DECOMP_INDEX[i + 1]]` is the full canonical
/// decomposition of `DECOMP_KEYS[i]`, canonically ordered, in UTF-16.
#[rustfmt::skip]
pub(super) static DECOMP_INDEX: [u16; {}] = [
",
        t.decomp_index.len()
    );
    emit_list(&mut s, &t.decomp_index, 12, |v| format!("{v}"));
    s.push_str("];\n");

    let _ = write!(
        s,
        "\n\
/// UTF-16 code units of the decompositions indexed by `DECOMP_INDEX`.
#[rustfmt::skip]
pub(super) static DECOMP_DATA: [u16; {}] = [
",
        t.decomp_data.len()
    );
    emit_list(&mut s, &t.decomp_data, 12, |v| format!("0x{v:04X}"));
    s.push_str("];\n");

    let _ = write!(
        s,
        "\n\
/// Non-zero canonical combining classes as `(first, last, class)` ranges,
/// ascending and disjoint. Code points not covered have class 0.
#[rustfmt::skip]
pub(super) static COMBINING_CLASS: [(u32, u32, u8); {}] = [
",
        t.combining_class.len()
    );
    emit_list(&mut s, &t.combining_class, 4, |(a, b, c)| {
        format!("(0x{a:05X}, 0x{b:05X}, {c})")
    });
    s.push_str("];\n");

    let _ = write!(
        s,
        "\n\
/// Primary composites for NFC as `(first, second, composite)`, ascending by
/// `(first, second)`. All are in the BMP in Unicode 3.2.
#[rustfmt::skip]
pub(super) static COMPOSE: [(u16, u16, u16); {}] = [
",
        t.compose.len()
    );
    emit_list(&mut s, &t.compose, 4, |(a, b, c)| {
        format!("(0x{a:04X}, 0x{b:04X}, 0x{c:04X})")
    });
    s.push_str("];\n");
    s
}

#[cfg(test)]
mod tests {
    use super::strip_c_comments;

    #[test]
    fn strips_both_comment_kinds() {
        let s = strip_c_comments("a // x\n/* 0 */ b, /* y */c\n// z").unwrap();
        assert_eq!(s.split_whitespace().collect::<Vec<_>>(), ["a", "b,", "c"]);
    }
}
