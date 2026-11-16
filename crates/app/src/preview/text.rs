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

//! The text provider: decides whether the sniffed bytes look like text,
//! reads up to `MAX_TEXT_BYTES`, picks the encoding and normalises line
//! endings (`.claude/docs/design/m5-quicklook-copy.md` §3).
//!
//! What it never does is guess UTF-16 without a byte-order mark, or trust
//! the extension: a file is text because of what is in it, or because the
//! disk says its classic type is `TEXT`.

use std::sync::atomic::AtomicBool;

use disk_atlas_lib::fs::hfs::encoding::Encoding;
use disk_atlas_lib::io::block::BlockSource;

use super::{Content, MAX_TEXT_BYTES, PreviewProvider, Sniff, Subject, TextPreview, read_prefix};

const TEXT_TYPE: [u8; 4] = *b"TEXT";
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];
const UTF16_LE_BOM: [u8; 2] = [0xFF, 0xFE];
const UTF16_BE_BOM: [u8; 2] = [0xFE, 0xFF];

/// Previews text.
pub struct TextProvider;

impl PreviewProvider for TextProvider {
    fn name(&self) -> &'static str {
        "text"
    }

    fn accepts(&self, subject: &Subject, sniff: &Sniff) -> bool {
        if subject
            .finder_codes
            .is_some_and(|codes| codes.file_type == TEXT_TYPE)
        {
            return true;
        }
        if bom(&sniff.head).is_some() {
            return true;
        }
        looks_like_text(&sniff.head)
    }

    fn load(
        &self,
        subject: &Subject,
        sniff: &Sniff,
        source: &dyn BlockSource,
        cancel: &AtomicBool,
    ) -> anyhow::Result<Option<Content>> {
        let Some(bytes) = read_prefix(source, MAX_TEXT_BYTES, cancel)? else {
            return Ok(None);
        };
        let truncated = sniff.len > bytes.len() as u64;
        let (decoded, encoding) = decode(&bytes, truncated, fallback(subject));
        let text = normalise(&decoded);
        Ok(Some(Content::Text(TextPreview::new(
            text, encoding, truncated,
        ))))
    }
}

/// A byte-order mark, and what it says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bom {
    Utf8,
    Utf16Le,
    Utf16Be,
}

impl Bom {
    fn len(self) -> usize {
        match self {
            Self::Utf8 => UTF8_BOM.len(),
            Self::Utf16Le | Self::Utf16Be => 2,
        }
    }
}

fn bom(bytes: &[u8]) -> Option<Bom> {
    if bytes.starts_with(&UTF8_BOM) {
        Some(Bom::Utf8)
    } else if bytes.starts_with(&UTF16_LE_BOM) {
        Some(Bom::Utf16Le)
    } else if bytes.starts_with(&UTF16_BE_BOM) {
        Some(Bom::Utf16Be)
    } else {
        None
    }
}

/// No NUL and fewer than 5% of the bytes below 0x20 other than tab, LF, FF
/// and CR.
fn looks_like_text(head: &[u8]) -> bool {
    if head.contains(&0) {
        return false;
    }
    let controls = head
        .iter()
        .filter(|&&b| b < 0x20 && !matches!(b, b'\t' | b'\n' | 0x0C | b'\r'))
        .count();
    // controls / len < 5 / 100
    controls.saturating_mul(20) < head.len()
}

/// What to read bytes that are neither UTF-8 nor marked as UTF-16.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SingleByte {
    Windows1252,
    MacRoman,
}

impl SingleByte {
    fn name(self) -> &'static str {
        match self {
            Self::Windows1252 => "Western (Windows-1252)",
            Self::MacRoman => Encoding::Roman.name(),
        }
    }
}

/// The single-byte encoding to fall back to. The one place the choice is
/// made.
fn fallback(subject: &Subject) -> SingleByte {
    // A classic HFS volume, or a file whose type code is TEXT, is Mac OS
    // Roman.
    let roman = subject.format == Some("HFS")
        || subject
            .finder_codes
            .is_some_and(|codes| codes.file_type == TEXT_TYPE);
    if roman {
        SingleByte::MacRoman
    } else {
        SingleByte::Windows1252
    }
}

/// Decodes `bytes`; the first of: a byte-order mark, UTF-8, the fallback.
/// `truncated` allows a character cut off at the end of the bytes.
fn decode(bytes: &[u8], truncated: bool, fallback: SingleByte) -> (String, &'static str) {
    match bom(bytes) {
        Some(Bom::Utf8) => {
            let body = bytes.get(Bom::Utf8.len()..).unwrap_or_default();
            return (String::from_utf8_lossy(body).into_owned(), "UTF-8");
        }
        Some(mark @ (Bom::Utf16Le | Bom::Utf16Be)) => {
            let body = bytes.get(mark.len()..).unwrap_or_default();
            let little = mark == Bom::Utf16Le;
            // A trailing odd byte is dropped.
            let units = body.as_chunks::<2>().0.iter().map(|&pair| {
                if little {
                    u16::from_le_bytes(pair)
                } else {
                    u16::from_be_bytes(pair)
                }
            });
            let text = char::decode_utf16(units)
                .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
                .collect();
            return (text, if little { "UTF-16 LE" } else { "UTF-16 BE" });
        }
        None => {}
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => return (text.to_owned(), "UTF-8"),
        // Only the end is bad, and the end is not the file's: a character
        // that the read cut in two.
        Err(error) if truncated && error.error_len().is_none() => {
            let whole = bytes.get(..error.valid_up_to()).unwrap_or_default();
            if let Ok(text) = std::str::from_utf8(whole) {
                return (text.to_owned(), "UTF-8");
            }
        }
        Err(_) => {}
    }
    let text = match fallback {
        SingleByte::Windows1252 => bytes.iter().map(|&b| windows_1252(b)).collect(),
        SingleByte::MacRoman => {
            let mut text = String::new();
            Encoding::Roman.decode_into(bytes, &mut text);
            text
        }
    };
    (text, fallback.name())
}

/// The character Windows-1252 gives `byte`. The five bytes it leaves
/// undefined are U+FFFD.
fn windows_1252(byte: u8) -> char {
    const HIGH: [char; 32] = [
        '\u{20AC}', '\u{FFFD}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{FFFD}',
        '\u{017D}', '\u{FFFD}', '\u{FFFD}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}',
        '\u{2022}', '\u{2013}', '\u{2014}', '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}',
        '\u{0153}', '\u{FFFD}', '\u{017E}', '\u{0178}',
    ];
    match byte {
        0x80..=0x9F => HIGH
            .get(usize::from(byte - 0x80))
            .copied()
            .unwrap_or(char::REPLACEMENT_CHARACTER),
        // ASCII and Latin-1 agree with Unicode.
        _ => char::from(byte),
    }
}

/// Lone CR and CRLF become LF; control characters other than tab and LF
/// become U+FFFD.
fn normalise(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' | '\t' => out.push(c),
            c if c.is_control() => out.push(char::REPLACEMENT_CHARACTER),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use disk_atlas_lib::error::{Error, Result};
    use disk_atlas_lib::fs::{
        DirStream, FileSystem, FinderCodes, Metadata, NodeId, Stream, VolumeInfo,
    };
    use disk_atlas_lib::io::block::MemSource;

    use super::*;

    struct NoFs;

    impl FileSystem for NoFs {
        fn info(&self) -> VolumeInfo {
            VolumeInfo::new("none")
        }
        fn root(&self) -> NodeId {
            NodeId(1)
        }
        fn metadata(&self, node: NodeId) -> Result<Metadata> {
            Err(Error::NotFound { node: node.0 })
        }
        fn lookup(&self, _dir: NodeId, _name: &str) -> Result<Option<NodeId>> {
            Ok(None)
        }
        fn read_dir(&self, dir: NodeId) -> Result<Box<dyn DirStream>> {
            Err(Error::NotADirectory { node: dir.0 })
        }
    }

    fn subject(file_type: Option<&[u8; 4]>, format: Option<&'static str>) -> Subject {
        Subject {
            fs: Arc::new(NoFs),
            node: NodeId(2),
            name: "file".into(),
            stream: Stream::Data,
            finder_codes: file_type.map(|&file_type| FinderCodes {
                file_type,
                creator: *b"ttxt",
            }),
            format,
        }
    }

    fn sniff_of(bytes: &[u8]) -> Sniff {
        Sniff {
            head: bytes
                .iter()
                .copied()
                .take(super::super::SNIFF_BYTES)
                .collect(),
            len: bytes.len() as u64,
        }
    }

    fn accepts(bytes: &[u8]) -> bool {
        TextProvider.accepts(&subject(None, None), &sniff_of(bytes))
    }

    /// Runs the provider over `bytes` as a file of `len` bytes (the source
    /// is as long as the bytes; `len` only says how much the file has
    /// beyond them when the test wants a truncation).
    fn load_subject(subject: &Subject, bytes: &[u8], len: u64) -> TextPreview {
        let source = MemSource::new(bytes.to_vec());
        let sniff = Sniff {
            head: bytes
                .iter()
                .copied()
                .take(super::super::SNIFF_BYTES)
                .collect(),
            len,
        };
        match TextProvider
            .load(subject, &sniff, &source, &AtomicBool::new(false))
            .unwrap()
        {
            Some(Content::Text(text)) => text,
            _ => panic!("expected text"),
        }
    }

    fn load(bytes: &[u8]) -> TextPreview {
        load_subject(&subject(None, None), bytes, bytes.len() as u64)
    }

    #[test]
    fn utf8_is_utf8() {
        let text = load("héllo\nwörld ✓".as_bytes());
        assert_eq!(text.encoding, "UTF-8");
        assert!(!text.truncated);
        assert_eq!(text.text(), "héllo\nwörld ✓");
        assert_eq!(text.row_count(), 2);
    }

    #[test]
    fn utf8_bom_is_dropped() {
        let text = load(b"\xEF\xBB\xBFhello");
        assert_eq!(text.encoding, "UTF-8");
        assert_eq!(text.text(), "hello");
    }

    #[test]
    fn utf16_with_a_bom_is_decoded_in_both_orders() {
        let units: Vec<u16> = "héllo\nwörld 😀".encode_utf16().collect();
        let mut le = vec![0xFF, 0xFE];
        let mut be = vec![0xFE, 0xFF];
        for unit in &units {
            le.extend_from_slice(&unit.to_le_bytes());
            be.extend_from_slice(&unit.to_be_bytes());
        }
        // They hold NULs, and are accepted anyway because of the mark.
        assert!(accepts(&le));
        assert!(accepts(&be));
        let text = load(&le);
        assert_eq!(
            (text.encoding, text.text()),
            ("UTF-16 LE", "héllo\nwörld 😀")
        );
        let text = load(&be);
        assert_eq!(
            (text.encoding, text.text()),
            ("UTF-16 BE", "héllo\nwörld 😀")
        );
    }

    #[test]
    fn utf16_odd_byte_and_lone_surrogate_are_tolerated() {
        // 'a', a lone high surrogate, 'b', then a stray byte.
        let text = load(&[0xFF, 0xFE, b'a', 0, 0x00, 0xD8, b'b', 0, 0x7F]);
        assert_eq!(text.text(), "a\u{FFFD}b");
    }

    #[test]
    fn line_endings_become_lf() {
        assert_eq!(load(b"a\r\nb\rc\nd").text(), "a\nb\nc\nd");
        assert_eq!(load(b"a\r\n\r\nb").text(), "a\n\nb");
        assert_eq!(load(b"a\r").text(), "a\n");
        assert_eq!(load(b"a\r\r\nb").text(), "a\n\nb");
    }

    #[test]
    fn control_characters_are_replaced_except_tab_and_lf() {
        let text = load(b"a\tb\x1Bc\x0Cd\x7Fe\nf");
        assert_eq!(text.text(), "a\tb\u{FFFD}c\u{FFFD}d\u{FFFD}e\nf");
        // C1 controls from UTF-8 too.
        assert_eq!(load("a\u{85}b".as_bytes()).text(), "a\u{FFFD}b");
    }

    #[test]
    fn nul_bytes_are_not_text() {
        assert!(!accepts(b"hello\0world"));
        assert!(!accepts(b"\0\0\0\0"));
        // Only the type code makes a file with NULs text.
        let sniff = sniff_of(b"hello\0world");
        assert!(TextProvider.accepts(&subject(Some(b"TEXT"), None), &sniff));
        assert!(!TextProvider.accepts(&subject(Some(b"APPL"), None), &sniff));
    }

    #[test]
    fn accepts_follows_the_control_character_rule() {
        assert!(accepts(b"plain text\n\twith tabs\r\n\x0C"));
        assert!(accepts("héllo".as_bytes()));
        // 100 bytes, 4 controls: under 5%; 5 controls: not.
        let mut few = vec![b'a'; 96];
        few.extend_from_slice(&[1, 2, 3, 4]);
        assert!(accepts(&few));
        let mut many = vec![b'a'; 95];
        many.extend_from_slice(&[1, 2, 3, 4, 5]);
        assert!(!accepts(&many));
        assert!(!accepts(&[0x1B; 20]));
    }

    #[test]
    fn text_type_code_is_accepted_whatever_the_bytes() {
        let sniff = sniff_of(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(TextProvider.accepts(&subject(Some(b"TEXT"), None), &sniff));
        assert!(!TextProvider.accepts(&subject(Some(b"PICT"), None), &sniff));
        assert!(!TextProvider.accepts(&subject(None, Some("HFS")), &sniff));
    }

    #[test]
    fn invalid_utf8_falls_back_to_windows_1252() {
        // 0x93 and 0x94 are curly quotes, 0xE9 is é, 0x80 the euro sign,
        // 0x81 is undefined.
        let text = load(b"\x93caf\xE9\x94 \x80 \x81");
        assert_eq!(text.encoding, "Western (Windows-1252)");
        assert_eq!(text.text(), "\u{201C}caf\u{E9}\u{201D} \u{20AC} \u{FFFD}");
    }

    #[test]
    fn classic_mac_text_reads_as_mac_os_roman() {
        // 0x8E is é and 0xA5 the bullet in Roman.
        let hfs = load_subject(&subject(Some(b"TEXT"), Some("HFS")), b"caf\x8E \xA5", 8);
        assert_eq!(hfs.encoding, "Mac OS Roman");
        assert_eq!(hfs.text(), "caf\u{E9} \u{2022}");
    }

    #[test]
    fn windows_1252_table_is_complete() {
        for byte in 0..=255u8 {
            let c = windows_1252(byte);
            match byte {
                0x81 | 0x8D | 0x8F | 0x90 | 0x9D => assert_eq!(c, '\u{FFFD}'),
                0x80..=0x9F => assert!(!c.is_control() && c != '\u{FFFD}', "{byte:#x}"),
                _ => assert_eq!(c, char::from(byte)),
            }
        }
        assert_eq!(windows_1252(0x9F), '\u{0178}');
        assert_eq!(windows_1252(0x8A), '\u{0160}');
    }

    #[test]
    fn a_character_cut_at_the_truncated_end_is_tolerated() {
        // "é" is C3 A9; the read ends after C3, and the file goes on.
        let cut = b"abc\xC3";
        let text = load_subject(&subject(None, None), cut, 100);
        assert_eq!(text.encoding, "UTF-8");
        assert_eq!(text.text(), "abc");
        assert!(text.truncated);
        // The same bytes as a whole file are not UTF-8.
        let whole = load(cut);
        assert_eq!(whole.encoding, "Western (Windows-1252)");
        assert!(!whole.truncated);
        // A bad byte in the middle is not a cut character, truncated or not.
        let bad = load_subject(&subject(None, None), b"a\xFFb\xC3", 100);
        assert_eq!(bad.encoding, "Western (Windows-1252)");
    }

    #[test]
    fn long_text_is_cut_and_says_so() {
        let line = "0123456789abcdef\n";
        let all = line.repeat(MAX_TEXT_BYTES / line.len() + 100);
        let source = MemSource::new(all.clone().into_bytes());
        let sniff = Sniff {
            head: all.bytes().take(super::super::SNIFF_BYTES).collect(),
            len: all.len() as u64,
        };
        let loaded = TextProvider
            .load(
                &subject(None, None),
                &sniff,
                &source,
                &AtomicBool::new(false),
            )
            .unwrap();
        let Some(Content::Text(text)) = loaded else {
            panic!("expected text");
        };
        assert!(text.truncated);
        assert_eq!(text.text().len(), MAX_TEXT_BYTES);
        // Exactly the limit is not truncated.
        let exact = "a".repeat(MAX_TEXT_BYTES);
        let text = load(exact.as_bytes());
        assert!(!text.truncated);
        assert_eq!(text.text().len(), MAX_TEXT_BYTES);
    }

    #[test]
    fn cancel_reads_nothing() {
        let source = MemSource::new(b"hello".to_vec());
        let loaded = TextProvider.load(
            &subject(None, None),
            &sniff_of(b"hello"),
            &source,
            &AtomicBool::new(true),
        );
        assert!(matches!(loaded, Ok(None)));
    }

    #[test]
    fn random_bytes_never_panic() {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for round in 0..2000usize {
            let len = (next() % 200) as usize;
            let prefix: &[u8] = match round % 4 {
                0 => &[0xFF, 0xFE],
                1 => &[0xFE, 0xFF],
                2 => &[0xEF, 0xBB, 0xBF],
                _ => &[],
            };
            let mut bytes = prefix.to_vec();
            bytes.extend((0..len).map(|_| next() as u8));
            let _ = accepts(&bytes);
            let truncated = round % 2 == 0;
            let (decoded, _) = decode(&bytes, truncated, SingleByte::Windows1252);
            let _ = TextPreview::new(normalise(&decoded), "x", truncated);
        }
    }
}
