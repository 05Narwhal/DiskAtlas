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

//! Text for the interface: sizes, dates, kinds and counts. Pure functions, no GPUI types.

use std::borrow::Cow;

use chrono::{DateTime, FixedOffset, Local, TimeZone};
use disk_atlas_lib::fs::{FinderCodes, NodeKind, Owner, Timestamp};

const UNITS: [&str; 6] = ["KB", "MB", "GB", "TB", "PB", "EB"];

/// "0 bytes", "1 byte", "18 bytes", "2 KB", "612 KB", "4.2 MB", "148.3 MB", "931.5 GB", "1.8 TB".
///
/// Binary steps (1024). Bytes below 1024 are exact. KB is a whole number, rounded, never below 1.
/// MB and above have one decimal. If rounding reaches 1024 of a unit, the next unit is used.
pub fn size(bytes: u64) -> String {
    match bytes {
        0 => return "0 bytes".to_string(),
        1 => return "1 byte".to_string(),
        2..=1023 => return format!("{bytes} bytes"),
        _ => {}
    }
    let bytes = u128::from(bytes);
    for (step, unit) in UNITS.iter().enumerate() {
        let divisor = 1u128 << (10 * (step + 1));
        if step == 0 {
            let whole = (bytes + divisor / 2) / divisor;
            if whole < 1024 {
                return format!("{whole} KB");
            }
        } else {
            let tenths = (bytes * 10 + divisor / 2) / divisor;
            // The last unit has no successor, so it always ends the loop.
            if tenths < 10240 || step == UNITS.len() - 1 {
                return format!("{}.{} {unit}", tenths / 10, tenths % 10);
            }
        }
    }
    // Not reached: the last unit always returns inside the loop.
    format!("{bytes} bytes")
}

/// "12 Jan 2020, 09:41" in the computer's local time zone. 24-hour clock, day without leading
/// zero, English three-letter month. A time that cannot be shown is "Unknown".
pub fn date_time(time: Timestamp) -> String {
    let Some(utc) = DateTime::from_timestamp(time.unix_seconds(), 0) else {
        return "Unknown".to_string();
    };
    let offset = Local.offset_from_utc_datetime(&utc.naive_utc());
    date_time_at_offset(time, offset.local_minus_utc())
}

/// The same text for an explicit UTC offset in seconds. `date_time` calls this; tests use it.
pub fn date_time_at_offset(time: Timestamp, utc_offset_seconds: i32) -> String {
    let shown = DateTime::from_timestamp(time.unix_seconds(), 0)
        .zip(FixedOffset::east_opt(utc_offset_seconds))
        .map(|(utc, offset)| utc.with_timezone(&offset));
    match shown {
        Some(local) => local.format("%-d %b %Y, %H:%M").to_string(),
        None => "Unknown".to_string(),
    }
}

/// Plain-words description of an extension. Matched ASCII case-insensitively.
static KINDS: &[(&str, &str)] = &[
    // Images
    ("png", "PNG image"),
    ("jpg", "JPEG image"),
    ("jpeg", "JPEG image"),
    ("gif", "GIF image"),
    ("tif", "TIFF image"),
    ("tiff", "TIFF image"),
    ("bmp", "BMP image"),
    ("psd", "Photoshop image"),
    ("svg", "SVG image"),
    ("webp", "WebP image"),
    ("heic", "HEIC image"),
    ("ico", "Icon image"),
    ("icns", "Icon image"),
    ("pict", "PICT image"),
    ("pct", "PICT image"),
    ("raw", "Raw image"),
    // Documents
    ("pdf", "PDF document"),
    ("txt", "Plain text"),
    ("text", "Plain text"),
    ("rtf", "Rich text"),
    ("md", "Markdown text"),
    ("markdown", "Markdown text"),
    ("doc", "Word document"),
    ("docx", "Word document"),
    ("xls", "Excel spreadsheet"),
    ("xlsx", "Excel spreadsheet"),
    ("csv", "CSV document"),
    ("ppt", "PowerPoint presentation"),
    ("pptx", "PowerPoint presentation"),
    ("pages", "Pages document"),
    ("numbers", "Numbers spreadsheet"),
    ("key", "Keynote presentation"),
    ("odt", "OpenDocument text"),
    ("ods", "OpenDocument spreadsheet"),
    ("odp", "OpenDocument presentation"),
    ("epub", "EPUB book"),
    ("html", "HTML document"),
    ("htm", "HTML document"),
    ("css", "Stylesheet"),
    ("json", "JSON document"),
    ("xml", "XML document"),
    ("yaml", "YAML document"),
    ("yml", "YAML document"),
    ("toml", "TOML document"),
    ("ini", "Settings file"),
    ("cfg", "Settings file"),
    // Archives
    ("zip", "ZIP archive"),
    ("sit", "StuffIt archive"),
    ("sitx", "StuffIt archive"),
    ("hqx", "BinHex archive"),
    ("cpt", "Compact Pro archive"),
    ("tar", "Tar archive"),
    ("gz", "Gzip archive"),
    ("tgz", "Gzip archive"),
    ("bz2", "Bzip2 archive"),
    ("xz", "XZ archive"),
    ("7z", "7-Zip archive"),
    ("rar", "RAR archive"),
    // Disk images
    ("dmg", "Disk image"),
    ("img", "Disk image"),
    ("iso", "Disk image"),
    ("sparseimage", "Disk image"),
    ("sparsebundle", "Disk image"),
    ("toast", "Disk image"),
    ("dc42", "Disk image"),
    ("vhd", "Disk image"),
    ("vhdx", "Disk image"),
    ("vmdk", "Disk image"),
    // Audio
    ("aif", "AIFF audio"),
    ("aiff", "AIFF audio"),
    ("mp3", "MP3 audio"),
    ("wav", "WAV audio"),
    ("flac", "FLAC audio"),
    ("m4a", "MPEG-4 audio"),
    ("aac", "AAC audio"),
    ("ogg", "Ogg audio"),
    ("mid", "MIDI audio"),
    ("midi", "MIDI audio"),
    // Video
    ("mov", "QuickTime movie"),
    ("qt", "QuickTime movie"),
    ("mp4", "MPEG-4 movie"),
    ("m4v", "MPEG-4 movie"),
    ("mkv", "Matroska video"),
    ("avi", "AVI movie"),
    ("webm", "WebM video"),
    ("mpg", "MPEG movie"),
    ("mpeg", "MPEG movie"),
    // Source code
    ("rs", "Rust source"),
    ("c", "C source"),
    ("h", "C header"),
    ("cpp", "C++ source"),
    ("cc", "C++ source"),
    ("hpp", "C++ header"),
    ("m", "Objective-C source"),
    ("swift", "Swift source"),
    ("java", "Java source"),
    ("js", "JavaScript source"),
    ("ts", "TypeScript source"),
    ("go", "Go source"),
    ("py", "Python script"),
    ("rb", "Ruby script"),
    ("pl", "Perl script"),
    ("lua", "Lua script"),
    ("sh", "Shell script"),
    ("bat", "Batch file"),
    ("ps1", "PowerShell script"),
    ("p", "Pascal source"),
    ("pas", "Pascal source"),
    // Programs and system files
    ("exe", "Application"),
    ("app", "Application"),
    ("pkg", "Installer package"),
    ("msi", "Installer package"),
    ("dll", "Library"),
    ("dylib", "Library"),
    ("so", "Library"),
    ("ttf", "Font"),
    ("otf", "Font"),
    ("woff", "Font"),
    ("woff2", "Font"),
    ("dfont", "Font"),
    ("suit", "Font"),
    ("textclipping", "Text clipping"),
    ("webloc", "Web location"),
    ("url", "Web location"),
    ("lnk", "Shortcut"),
    ("plist", "Property list"),
    ("log", "Log file"),
    ("bak", "Backup file"),
    ("tmp", "Temporary file"),
];

/// What kind of thing an entry is, in plain words: "Folder", "Link", "PNG image", "Plain text",
/// "Document".
///
/// A file's kind comes from the text after the last dot, matched ASCII case-insensitively without
/// allocating. A name with no extension, a leading-dot name such as `.gitignore`, or an unknown
/// extension is a "Document".
pub fn kind(name: &str, kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Directory => "Folder",
        NodeKind::Symlink => "Link",
        NodeKind::File => file_kind(name),
        _ => "Special file",
    }
}

fn file_kind(name: &str) -> &'static str {
    // '.' is ASCII, so the split points are always on character boundaries.
    let extension = match name.rfind('.') {
        Some(dot) if dot > 0 => &name[dot + 1..],
        _ => "",
    };
    if extension.is_empty() {
        return "Document";
    }
    KINDS
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(extension))
        .map_or("Document", |(_, kind)| kind)
}

/// "1,234,567"
pub fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// "1 item", "17 items", "1,204 items"
pub fn items(n: usize) -> String {
    if n == 1 {
        "1 item".to_string()
    } else {
        format!("{} items", grouped(n as u64))
    }
}

/// One four-byte code: its four characters when every byte is printable ASCII, otherwise
/// `0x` and eight uppercase hex digits.
fn code(bytes: [u8; 4]) -> String {
    if bytes.iter().all(|byte| (0x20..=0x7E).contains(byte)) {
        bytes.iter().map(|&byte| char::from(byte)).collect()
    } else {
        format!("0x{:08X}", u32::from_be_bytes(bytes))
    }
}

/// "PNGf, prvw": the type and creator codes, each as text or, when it holds a byte that is not
/// printable ASCII, as hex.
pub fn finder_codes(codes: &FinderCodes) -> String {
    format!("{}, {}", code(codes.file_type), code(codes.creator))
}

/// "User 501, read and write": the owner and what the owner may do. Only the owner's read and
/// write bits count; execute is left out.
pub fn owner(owner: &Owner) -> String {
    let bits = (owner.mode >> 6) & 0o7;
    let access = if bits & 0o6 == 0o6 {
        "read and write"
    } else if bits & 0o4 != 0 {
        "read only"
    } else if bits & 0o2 != 0 {
        "write only"
    } else {
        "no access"
    };
    format!("User {}, {access}", owner.user)
}

/// The ellipsis that stands for the components `elide_path_middle` removes.
const ELLIPSIS: &str = "\u{2026}";

/// Cuts whole components from the middle of `path` until it fits `available`, putting `…`
/// where they were: `Disk/Users/ann/Documents/Taxes/2024` becomes `Disk/…/Taxes/2024`.
///
/// `measure` returns the width of a piece of text in the unit of `available`; it is called for
/// the whole path first, and for the pieces only when the path does not fit. The ellipsis and
/// the separators count in the budget. The rules, in order:
///
/// 1. A path that fits, an empty one, and one with a single component come back unchanged.
/// 2. Otherwise the first component (the empty one before a leading separator, so a root stays
///    a root) is kept with as many of the last components as fit, around the ellipsis.
/// 3. When even the first component and the last one do not fit together, the first goes too:
///    `…` and as many of the last components as fit.
/// 4. When the ellipsis and the last component do not fit, the last component alone is
///    returned and the caller's end truncation does the rest.
///
/// A component is never cut. One trailing separator is kept at the end of the result.
pub fn elide_path_middle<'a>(
    path: &'a str,
    separator: &str,
    available: f32,
    mut measure: impl FnMut(&str) -> f32,
) -> Cow<'a, str> {
    if separator.is_empty() || path.is_empty() || path == separator || measure(path) <= available {
        return Cow::Borrowed(path);
    }
    let (body, tail) = match path.strip_suffix(separator) {
        Some(body) if !body.is_empty() => (body, separator),
        _ => (path, ""),
    };
    let parts: Vec<&str> = body.split(separator).collect();
    let count = parts.len();
    if count < 2 {
        return Cow::Borrowed(path);
    }
    let widths: Vec<f32> = parts.iter().map(|part| measure(part)).collect();
    let separator_width = measure(separator);
    let ellipsis_width = measure(ELLIPSIS);
    let tail_width = if tail.is_empty() {
        0.0
    } else {
        separator_width
    };

    // The most trailing components, at most `most`, that fit after `lead` (the width of what
    // comes before them). Widths only grow with `keep`, so the first miss ends the search.
    let trailing_that_fit = |lead: f32, most: usize| -> Option<usize> {
        let mut used = lead + tail_width;
        let mut best = None;
        for keep in 1..=most {
            used += separator_width + widths[count - keep];
            if used > available {
                break;
            }
            best = Some(keep);
        }
        best
    };
    let build = |with_first: bool, keep: usize| -> Cow<'a, str> {
        let mut out = String::with_capacity(path.len());
        if with_first {
            out.push_str(parts[0]);
            out.push_str(separator);
        }
        out.push_str(ELLIPSIS);
        for part in &parts[count - keep..] {
            out.push_str(separator);
            out.push_str(part);
        }
        out.push_str(tail);
        Cow::Owned(out)
    };

    if count >= 3 {
        let lead = widths[0] + separator_width + ellipsis_width;
        if let Some(keep) = trailing_that_fit(lead, count - 2) {
            return build(true, keep);
        }
    }
    if let Some(keep) = trailing_that_fit(ellipsis_width, count - 1) {
        return build(false, keep);
    }
    Cow::Borrowed(parts[count - 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    const TB: u64 = GB * 1024;

    #[test]
    fn size_documented_examples() {
        assert_eq!(size(0), "0 bytes");
        assert_eq!(size(1), "1 byte");
        assert_eq!(size(18), "18 bytes");
        assert_eq!(size(2 * KB), "2 KB");
        assert_eq!(size(612 * KB), "612 KB");
        assert_eq!(size(MB * 42 / 10), "4.2 MB");
        assert_eq!(size(MB * 1483 / 10), "148.3 MB");
        assert_eq!(size(GB * 9315 / 10), "931.5 GB");
        assert_eq!(size(TB * 18 / 10), "1.8 TB");
    }

    #[test]
    fn size_edges() {
        assert_eq!(size(1023), "1023 bytes");
        assert_eq!(size(1024), "1 KB");
        assert_eq!(size(1536), "2 KB");
        assert_eq!(size(MB - 1), "1.0 MB");
        assert_eq!(size(MB), "1.0 MB");
        assert_eq!(size(GB - 1), "1.0 GB");
        assert_eq!(size(u64::MAX), "16.0 EB");
    }

    #[test]
    fn date_time_at_epoch_with_offsets() {
        let epoch = Timestamp::from_unix(0, 0);
        assert_eq!(date_time_at_offset(epoch, 0), "1 Jan 1970, 00:00");
        assert_eq!(date_time_at_offset(epoch, 3600), "1 Jan 1970, 01:00");
        assert_eq!(date_time_at_offset(epoch, -3600), "31 Dec 1969, 23:00");
    }

    #[test]
    fn date_time_other_dates() {
        let example = Timestamp::from_unix(1_578_822_060, 0);
        assert_eq!(date_time_at_offset(example, 0), "12 Jan 2020, 09:41");
        let before_epoch = Timestamp::from_unix(-1, 0);
        assert_eq!(date_time_at_offset(before_epoch, 0), "31 Dec 1969, 23:59");
        let leap_day = Timestamp::from_unix(1_582_977_600, 0);
        assert_eq!(date_time_at_offset(leap_day, 0), "29 Feb 2020, 12:00");
    }

    #[test]
    fn date_time_never_panics() {
        let far = Timestamp::from_unix_nanos(i64::MIN);
        // Either a date or the placeholder, never a panic.
        assert!(!date_time(far).is_empty());
        assert!(!date_time(Timestamp::from_unix_nanos(i64::MAX)).is_empty());
        assert_eq!(
            date_time_at_offset(Timestamp::from_unix(0, 0), i32::MAX),
            "Unknown"
        );
    }

    #[test]
    fn kind_non_files() {
        assert_eq!(kind("x.png", NodeKind::Directory), "Folder");
        assert_eq!(kind("x.png", NodeKind::Symlink), "Link");
        assert_eq!(kind("x.png", NodeKind::Other), "Special file");
    }

    #[test]
    fn kind_by_extension() {
        assert_eq!(kind("a.png", NodeKind::File), "PNG image");
        assert_eq!(kind("a.PNG", NodeKind::File), "PNG image");
        assert_eq!(kind("a.JpG", NodeKind::File), "JPEG image");
        assert_eq!(kind("a.tar.gz", NodeKind::File), "Gzip archive");
        assert_eq!(kind("Note.textClipping", NodeKind::File), "Text clipping");
        assert_eq!(kind("notes.txt", NodeKind::File), "Plain text");
    }

    #[test]
    fn kind_falls_back_to_document() {
        assert_eq!(kind("README", NodeKind::File), "Document");
        assert_eq!(kind(".gitignore", NodeKind::File), "Document");
        assert_eq!(kind("name.", NodeKind::File), "Document");
        assert_eq!(kind("a.unknownext", NodeKind::File), "Document");
        assert_eq!(kind("", NodeKind::File), "Document");
        assert_eq!(kind(".png", NodeKind::File), "Document");
    }

    #[test]
    fn kind_multibyte_names() {
        assert_eq!(kind("Café Menü.pdf", NodeKind::File), "PDF document");
        assert_eq!(kind("日本語.PNG", NodeKind::File), "PNG image");
        assert_eq!(kind("日本語.画像", NodeKind::File), "Document");
        assert_eq!(kind("é.", NodeKind::File), "Document");
    }

    #[test]
    fn kind_table_has_no_duplicates() {
        for (i, (a, _)) in KINDS.iter().enumerate() {
            for (b, _) in &KINDS[i + 1..] {
                assert!(!a.eq_ignore_ascii_case(b), "duplicate extension {a}");
            }
        }
    }

    #[test]
    fn grouped_digits() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1000), "1,000");
        assert_eq!(grouped(1_234_567), "1,234,567");
        assert_eq!(grouped(u64::MAX), "18,446,744,073,709,551,615");
    }

    #[test]
    fn finder_codes_as_text_or_hex() {
        let codes = |file_type: &[u8; 4], creator: &[u8; 4]| FinderCodes {
            file_type: *file_type,
            creator: *creator,
        };
        assert_eq!(finder_codes(&codes(b"PNGf", b"prvw")), "PNGf, prvw");
        // Spaces are printable; the ends of the printable range too.
        assert_eq!(finder_codes(&codes(b"a b ", b" ~ ~")), "a b ,  ~ ~");
        // A zero code, a control byte and a byte past ASCII are hex.
        assert_eq!(finder_codes(&codes(&[0; 4], b"ttxt")), "0x00000000, ttxt");
        assert_eq!(
            finder_codes(&codes(b"TEXT", &[0x00, 0x1F, 0x20, 0x41])),
            "TEXT, 0x001F2041"
        );
        assert_eq!(
            finder_codes(&codes(&[b'a', b'b', b'c', 0x7F], &[0xFE, 0xED, 0xFA, 0xCE])),
            "0x6162637F, 0xFEEDFACE"
        );
        assert_eq!(
            finder_codes(&codes(&[0xC4, 0x20, 0x20, 0x20], b"1234")),
            "0xC4202020, 1234"
        );
    }

    #[test]
    fn owner_with_each_permission_combination() {
        let of = |mode| {
            owner(&Owner {
                user: 501,
                group: 20,
                mode,
            })
        };
        assert_eq!(of(0o100_644), "User 501, read and write");
        assert_eq!(of(0o100_600), "User 501, read and write");
        assert_eq!(of(0o100_755), "User 501, read and write");
        assert_eq!(of(0o100_444), "User 501, read only");
        assert_eq!(of(0o100_544), "User 501, read only");
        assert_eq!(of(0o100_244), "User 501, write only");
        assert_eq!(of(0o100_344), "User 501, write only");
        assert_eq!(of(0o100_044), "User 501, no access");
        assert_eq!(of(0o100_144), "User 501, no access");
        assert_eq!(of(0o100_000), "User 501, no access");
        // Group and other bits do not count.
        assert_eq!(of(0o100_066), "User 501, no access");
        assert_eq!(
            owner(&Owner {
                user: 0,
                group: 0,
                mode: 0o040_700,
            }),
            "User 0, read and write"
        );
    }

    #[test]
    fn item_counts() {
        assert_eq!(items(0), "0 items");
        assert_eq!(items(1), "1 item");
        assert_eq!(items(17), "17 items");
        assert_eq!(items(1204), "1,204 items");
    }

    /// One unit per character, so the ellipsis and each separator cost 1.
    fn chars(text: &str) -> f32 {
        text.chars().count() as f32
    }

    fn elide(path: &str, available: f32) -> String {
        elide_path_middle(path, "/", available, chars).into_owned()
    }

    /// 35 units: Disk 4, Users 5, ann 3, Documents 9, Taxes 5, 2024 4, and five separators.
    const DEEP: &str = "Disk/Users/ann/Documents/Taxes/2024";

    #[test]
    fn a_path_that_fits_is_returned_borrowed() {
        let result = elide_path_middle("Disk/Users/ann", "/", 14.0, chars);
        assert!(matches!(result, Cow::Borrowed("Disk/Users/ann")));
        assert!(matches!(
            elide_path_middle(DEEP, "/", 35.0, chars),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn empty_and_single_component_paths_are_unchanged() {
        assert_eq!(elide("", 0.0), "");
        assert_eq!(elide("Documents", 3.0), "Documents");
        assert_eq!(elide("/", 0.0), "/");
        assert!(matches!(
            elide_path_middle("Documents", "/", 3.0, chars),
            Cow::Borrowed("Documents")
        ));
    }

    #[test]
    fn a_deep_path_keeps_the_first_and_the_most_trailing_components() {
        assert_eq!(elide(DEEP, 34.0), "Disk/\u{2026}/ann/Documents/Taxes/2024");
        assert_eq!(elide(DEEP, 31.0), "Disk/\u{2026}/ann/Documents/Taxes/2024");
        assert_eq!(elide(DEEP, 30.0), "Disk/\u{2026}/Documents/Taxes/2024");
        assert_eq!(elide(DEEP, 27.0), "Disk/\u{2026}/Documents/Taxes/2024");
    }

    #[test]
    fn the_ellipsis_and_separators_count_in_the_budget() {
        // Disk/…/Taxes/2024 is 4 + 1 + 1 + 1 + 5 + 1 + 4 = 17 units.
        assert_eq!(elide(DEEP, 17.0), "Disk/\u{2026}/Taxes/2024");
        assert_eq!(elide(DEEP, 26.0), "Disk/\u{2026}/Taxes/2024");
        let fitted = elide(DEEP, 17.0);
        assert_eq!(chars(&fitted), 17.0);
    }

    #[test]
    fn first_and_last_only() {
        // One unit short of the 17 that Taxes needs.
        assert_eq!(elide(DEEP, 16.0), "Disk/\u{2026}/2024");
        assert_eq!(elide(DEEP, 11.0), "Disk/\u{2026}/2024");
    }

    #[test]
    fn a_long_first_component_is_dropped_before_the_last_is_cut() {
        // Volume/…/last is 13 units; …/last is 6.
        assert_eq!(elide("Volume/middle/last", 13.0), "Volume/\u{2026}/last");
        assert_eq!(elide("Volume/middle/last", 12.0), "\u{2026}/last");
        assert_eq!(elide(DEEP, 10.0), "\u{2026}/2024");
        assert_eq!(elide("Volume/middle/last", 6.0), "\u{2026}/last");
    }

    #[test]
    fn when_the_last_component_alone_does_not_fit_it_is_kept_whole() {
        let result = elide_path_middle("Disk/Users/averylongfoldername", "/", 5.0, chars);
        assert!(matches!(result, Cow::Borrowed("averylongfoldername")));
        assert_eq!(
            elide("Disk/averylongfoldername", 5.0),
            "averylongfoldername"
        );
        assert_eq!(
            elide("Disk/averylongfoldername", 21.0),
            "\u{2026}/averylongfoldername"
        );
    }

    #[test]
    fn a_leading_separator_keeps_the_root() {
        let path = "/Users/ann/Documents/Taxes";
        assert_eq!(elide(path, 14.0), "/\u{2026}/Taxes");
        assert_eq!(elide(path, 18.0), "/\u{2026}/Documents/Taxes");
        assert_eq!(elide(path, 25.0), "/\u{2026}/ann/Documents/Taxes");
    }

    #[test]
    fn a_trailing_separator_stays_at_the_end() {
        let path = "Disk/Users/ann/Documents/";
        assert_eq!(elide(path, 17.0), "Disk/\u{2026}/Documents/");
        assert_eq!(elide(path, 16.0), "\u{2026}/ann/Documents/");
    }

    #[test]
    fn components_with_unicode_are_never_cut() {
        let path = "Disque/Users/Ren\u{e9}e/Documents/caf\u{e9}/\u{65e5}\u{672c}\u{8a9e}";
        assert_eq!(
            elide(path, 17.0),
            "Disque/\u{2026}/caf\u{e9}/\u{65e5}\u{672c}\u{8a9e}"
        );
        assert_eq!(
            elide(path, 11.0),
            "\u{2026}/caf\u{e9}/\u{65e5}\u{672c}\u{8a9e}"
        );
    }

    #[test]
    fn a_path_that_starts_with_an_ellipsis_keeps_it_as_its_first_component() {
        // The index writes a leading … when a chain is too deep.
        assert_eq!(elide("\u{2026}/a/bbbbbbbb/c", 8.0), "\u{2026}/\u{2026}/c");
    }

    #[test]
    fn widths_come_from_the_measure_not_from_character_counts() {
        // Every piece costs twice its length: the same path needs twice the budget.
        let double = |text: &str| chars(text) * 2.0;
        assert_eq!(
            elide_path_middle(DEEP, "/", 34.0, double),
            "Disk/\u{2026}/Taxes/2024"
        );
    }

    #[test]
    fn a_multi_character_separator_is_honoured() {
        assert_eq!(
            elide_path_middle("a::bbbb::cccc::d", "::", 10.0, chars),
            "a::\u{2026}::d"
        );
    }
}
