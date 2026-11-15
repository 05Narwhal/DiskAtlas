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

//! Generates `crates/lib/src/fs/hfs/encoding/tables.rs`.
//!
//! Usage:
//!
//! ```text
//! mac_encodings [--check] [--out PATH] [--dir PATH]
//! ```
//!
//! `--dir` is the directory holding the eight Apple mapping files and
//! `SHA256SUMS` (default: this crate's directory). Every file is checked
//! against `SHA256SUMS` before it is parsed. `--out` defaults to the
//! checked-in `tables.rs`.
//!
//! With `--check` nothing is written: the tables are generated in memory and
//! compared with the file at the output path. Exit status 0 means identical,
//! 1 means different, 2 means an error.

mod sha256;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The licence header every Rust source file starts with, generated ones included.
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

/// One encoding: the prefix of its generated statics, its mapping file, and
/// its display name for the generated comments. Order is the declaration
/// order of `Encoding` in the engine.
const ENCODINGS: [(&str, &str, &str); 8] = [
    ("ROMAN", "ROMAN.TXT", "Mac OS Roman"),
    (
        "CENTRAL_EUROPEAN",
        "CENTEURO.TXT",
        "Mac OS Central European",
    ),
    ("CYRILLIC", "CYRILLIC.TXT", "Mac OS Cyrillic"),
    ("GREEK", "GREEK.TXT", "Mac OS Greek"),
    ("TURKISH", "TURKISH.TXT", "Mac OS Turkish"),
    ("CROATIAN", "CROATIAN.TXT", "Mac OS Croatian"),
    ("ICELANDIC", "ICELAND.TXT", "Mac OS Icelandic"),
    ("ROMANIAN", "ROMANIAN.TXT", "Mac OS Romanian"),
];

/// Sequences `(base, combining mark, precomposed character)` a mapping file
/// may give instead of one code point. The eight files contain none (checked
/// 2026-11-15); the parser fails on any sequence it meets, so adding a file
/// that has one means adding its exact entry here.
const SEQUENCES: &[(u32, u32, char)] = &[];

/// Bytes a file "defines" only as a private-use placeholder for an undefined
/// byte: `(file, byte, placeholder)`. They become U+FFFD like any undefined
/// byte. `TURKISH.TXT` maps 0xF5 to U+F8A0 with the comment `undefined1` and
/// says "0xF5 code point undefined in Mac OS Turkish". Nothing else may use
/// this: the placeholder must match the file exactly or the generator fails.
const UNDEFINED_PLACEHOLDERS: &[(&str, u8, u32)] = &[("TURKISH.TXT", 0xF5, 0xF8A0)];

/// Input files are about 14 KiB; anything far larger is not ours.
const MAX_INPUT_BYTES: u64 = 1 << 20;

/// The replacement character, for bytes a file leaves undefined.
const UNDEFINED: char = '\u{FFFD}';

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(message) => {
            eprintln!("mac_encodings: {message}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<ExitCode, String> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut check = false;
    let mut dir = manifest_dir.to_path_buf();
    let mut out = manifest_dir.join("../../crates/lib/src/fs/hfs/encoding/tables.rs");

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--check" => check = true,
            "--dir" => dir = PathBuf::from(args.next().ok_or("--dir needs a path")?),
            "--out" => out = PathBuf::from(args.next().ok_or("--out needs a path")?),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }

    let sums = read_text(&dir.join("SHA256SUMS"))?;
    let digests = parse_sums(&sums)?;

    let mut tables = Vec::new();
    for (_, file, _) in ENCODINGS {
        let text = read_verified(&dir, file, &digests)?;
        tables.push(parse_mapping(file, &text)?);
    }
    let generated = render(&tables);

    if check {
        let existing = std::fs::read_to_string(&out)
            .map_err(|e| format!("cannot read {}: {e}", out.display()))?;
        if existing == generated {
            println!("{} is up to date", out.display());
            Ok(ExitCode::SUCCESS)
        } else {
            println!("{} differs from the generated tables", out.display());
            Ok(ExitCode::from(1))
        }
    } else {
        std::fs::write(&out, generated)
            .map_err(|e| format!("cannot write {}: {e}", out.display()))?;
        println!("wrote {}", out.display());
        Ok(ExitCode::SUCCESS)
    }
}

fn read_bytes(path: &Path) -> Result<Vec<u8>, String> {
    let meta =
        std::fs::metadata(path).map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
    if meta.len() > MAX_INPUT_BYTES {
        return Err(format!(
            "{} is {} bytes, too large",
            path.display(),
            meta.len()
        ));
    }
    std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

fn read_text(path: &Path) -> Result<String, String> {
    String::from_utf8(read_bytes(path)?).map_err(|_| format!("{} is not UTF-8", path.display()))
}

/// Parses `sha256sum` output: `<64 hex digits> <space or *><name>` per line.
fn parse_sums(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut map = BTreeMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let (digest, rest) = line
            .split_once(' ')
            .ok_or_else(|| format!("SHA256SUMS: bad line {line:?}"))?;
        let name = rest.strip_prefix(['*', ' ']).unwrap_or(rest);
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("SHA256SUMS: bad digest in {line:?}"));
        }
        if map
            .insert(name.to_string(), digest.to_ascii_lowercase())
            .is_some()
        {
            return Err(format!("SHA256SUMS: {name} listed twice"));
        }
    }
    Ok(map)
}

fn read_verified(
    dir: &Path,
    file: &str,
    digests: &BTreeMap<String, String>,
) -> Result<String, String> {
    let expected = digests
        .get(file)
        .ok_or_else(|| format!("SHA256SUMS has no entry for {file}"))?;
    let bytes = read_bytes(&dir.join(file))?;
    let actual = sha256::hex_digest(&bytes);
    if &actual != expected {
        return Err(format!(
            "{file}: SHA-256 is {actual}, SHA256SUMS says {expected}"
        ));
    }
    String::from_utf8(bytes).map_err(|_| format!("{file} is not UTF-8"))
}

/// Parses `0xNN` into a byte.
fn parse_byte(token: &str) -> Option<u8> {
    let digits = token.strip_prefix("0x")?;
    if digits.len() != 2 {
        return None;
    }
    u8::from_str_radix(digits, 16).ok()
}

/// Parses `0xNNNN` (up to six digits) into a code point value.
fn parse_code_point(token: &str) -> Option<u32> {
    let digits = token.strip_prefix("0x")?;
    if digits.is_empty() || digits.len() > 6 {
        return None;
    }
    u32::from_str_radix(digits, 16).ok()
}

/// Resolves the second column of a mapping line to one character.
fn resolve(file: &str, token: &str) -> Result<char, String> {
    if token.contains('<') || token.contains('>') {
        return Err(format!("{file}: direction hint {token:?} not supported"));
    }
    if let Some((base, mark)) = token.split_once('+') {
        let (Some(b), Some(m)) = (parse_code_point(base), parse_code_point(mark)) else {
            return Err(format!("{file}: malformed sequence {token:?}"));
        };
        return SEQUENCES
            .iter()
            .find(|&&(sb, sm, _)| sb == b && sm == m)
            .map(|&(_, _, c)| c)
            .ok_or_else(|| format!("{file}: sequence {token} is not in SEQUENCES"));
    }
    let value = parse_code_point(token).ok_or_else(|| format!("{file}: bad value {token:?}"))?;
    // The Apple logo (U+F8FF, private use) is kept as the file gives it.
    char::from_u32(value).ok_or_else(|| format!("{file}: {token} is not a character"))
}

/// Parses one mapping file into a 256-entry byte -> char table.
///
/// The files list 0x20-0x7E and 0x80-0xFF and leave out the control bytes
/// 0x00-0x1F and 0x7F ("the standard control characters"), which are
/// identity here. 0x00-0x7F must map to themselves; a missing high byte is
/// undefined and becomes U+FFFD. Two bytes mapping to one character fail.
fn parse_mapping(file: &str, text: &str) -> Result<[char; 256], String> {
    let mut table: [Option<char>; 256] = [None; 256];
    for (n, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let at = |msg: String| format!("{file}:{}: {msg}", n + 1);
        let mut cols = line.split_whitespace();
        let byte_token = cols.next().ok_or_else(|| at("empty line".into()))?;
        let byte = parse_byte(byte_token).ok_or_else(|| at(format!("bad byte {byte_token:?}")))?;
        let value = match cols.next() {
            // A line with no value leaves the byte undefined.
            None => UNDEFINED,
            Some(token) => resolve(file, token).map_err(&at)?,
        };
        let value = if UNDEFINED_PLACEHOLDERS
            .iter()
            .any(|&(f, b, p)| f == file && b == byte && p == value as u32)
        {
            UNDEFINED
        } else {
            value
        };
        if let Some(extra) = cols.next() {
            return Err(at(format!("unexpected extra column {extra:?}")));
        }
        if table[byte as usize].replace(value).is_some() {
            return Err(at(format!("byte {byte_token} appears twice")));
        }
    }

    let mut out = [UNDEFINED; 256];
    for byte in 0..=255u8 {
        let i = byte as usize;
        out[i] = match (table[i], byte) {
            (Some(c), _) => c,
            (None, 0x00..=0x1F | 0x7F) => byte as char,
            (None, 0x20..=0x7E) => return Err(format!("{file}: ASCII byte {byte:#04x} missing")),
            (None, _) => UNDEFINED,
        };
        if byte < 0x80 && out[i] != byte as char {
            return Err(format!(
                "{file}: byte {byte:#04x} maps to U+{:04X}, not ASCII",
                out[i] as u32
            ));
        }
    }

    // The encoder must be a true inverse of the decoder.
    let mut seen: BTreeMap<char, u8> = BTreeMap::new();
    for byte in 0..=255u8 {
        let c = out[byte as usize];
        if c == UNDEFINED {
            continue;
        }
        if let Some(first) = seen.insert(c, byte) {
            return Err(format!(
                "{file}: bytes {first:#04x} and {byte:#04x} both map to U+{:04X}",
                c as u32
            ));
        }
    }
    Ok(out)
}

fn render(tables: &[[char; 256]]) -> String {
    let mut s = String::from(HEADER);
    s.push_str(
        "
//! Tables for the single-byte Mac OS encodings. Generated by `tools/mac_encodings`; do not edit.
//!
//! Inputs: the mapping files Apple published at
//! <https://www.unicode.org/Public/MAPPINGS/VENDORS/APPLE/>, kept with their
//! SHA-256 digests in `tools/mac_encodings/`.
//!
//! Per encoding, `<NAME>_DECODE` maps the bytes 0x80-0xFF to characters (U+FFFD
//! where the file leaves a byte undefined) and `<NAME>_ENCODE` is its inverse,
//! sorted by character for binary search, without the U+FFFD entries. Bytes
//! 0x00-0x7F are ASCII in every encoding and are not stored.
//!
//! Regenerate or verify with the commands in `tools/mac_encodings/README.md`.
",
    );
    for (&(prefix, file, name), table) in ENCODINGS.iter().zip(tables) {
        let high = &table[0x80..];
        let _ = write!(
            s,
            "\n/// {name} (`{file}`): bytes 0x80-0xFF to characters.\n#[rustfmt::skip]\npub(super) static {prefix}_DECODE: [char; 128] = [\n"
        );
        for (row, chunk) in high.chunks(8).enumerate() {
            s.push_str("   ");
            for c in chunk {
                let _ = write!(s, " '\\u{{{:04X}}}',", *c as u32);
            }
            let _ = writeln!(s, " // 0x{:02X}", 0x80 + row * 8);
        }
        s.push_str("];\n");

        let mut pairs: Vec<(char, u8)> = high
            .iter()
            .zip(0x80..=0xFFu8)
            .filter(|&(&c, _)| c != UNDEFINED)
            .map(|(&c, b)| (c, b))
            .collect();
        pairs.sort();
        let _ = write!(
            s,
            "\n/// {name}: characters to bytes, sorted by character.\n#[rustfmt::skip]\npub(super) static {prefix}_ENCODE: &[(char, u8)] = &[\n"
        );
        for chunk in pairs.chunks(6) {
            s.push_str("   ");
            for &(c, b) in chunk {
                let _ = write!(s, " ('\\u{{{:04X}}}', 0x{b:02X}),", c as u32);
            }
            s.push('\n');
        }
        s.push_str("];\n");
    }
    s
}
