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

//! Kind classes: the one place that decides what kind of file an entry is.
//!
//! A class comes from the extension first: the bytes after the last `.` of a
//! name, matched case-insensitively against a sorted static table. When that
//! gives [`KindClass::Other`] and the entry carries a classic Mac type code
//! (`FInfo.fdType`), the code is matched exactly against a second sorted
//! table. Neither lookup allocates. The class is stored in four bits of the
//! index flags, and [`KindSet`] filters on it.

/// The coarse kind of an entry, as the Kind filter shows it.
///
/// The discriminants are stored in saved indexes: never renumber them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum KindClass {
    Other = 0,
    Folder = 1,
    Document = 2,
    Image = 3,
    Audio = 4,
    Video = 5,
    Archive = 6,
    Application = 7,
    Code = 8,
    DiskImage = 9,
    Font = 10,
    Model = 11,
}

impl KindClass {
    /// Every class, in discriminant order.
    pub const ALL: [KindClass; 12] = [
        KindClass::Other,
        KindClass::Folder,
        KindClass::Document,
        KindClass::Image,
        KindClass::Audio,
        KindClass::Video,
        KindClass::Archive,
        KindClass::Application,
        KindClass::Code,
        KindClass::DiskImage,
        KindClass::Font,
        KindClass::Model,
    ];

    /// The 4-bit value stored in the index flags (bits 8-11).
    pub const fn bits(self) -> u16 {
        self as u8 as u16
    }

    /// The inverse of [`bits`](Self::bits); values past `Model` give `Other`.
    pub const fn from_bits(bits: u16) -> Self {
        match bits {
            1 => KindClass::Folder,
            2 => KindClass::Document,
            3 => KindClass::Image,
            4 => KindClass::Audio,
            5 => KindClass::Video,
            6 => KindClass::Archive,
            7 => KindClass::Application,
            8 => KindClass::Code,
            9 => KindClass::DiskImage,
            10 => KindClass::Font,
            11 => KindClass::Model,
            _ => KindClass::Other,
        }
    }

    /// Display name for the Kind filter menu and the storage bar.
    pub const fn label(self) -> &'static str {
        match self {
            KindClass::Other => "Other",
            KindClass::Folder => "Folder",
            KindClass::Document => "Document",
            KindClass::Image => "Picture",
            KindClass::Audio => "Audio",
            KindClass::Video => "Movie",
            KindClass::Archive => "Archive",
            KindClass::Application => "Application",
            KindClass::Code => "Code and data",
            KindClass::DiskImage => "Disk image",
            KindClass::Font => "Font",
            KindClass::Model => "3D model",
        }
    }
}

/// Longest extension, in bytes, that is looked up. Longer ones are `Other`.
const MAX_EXTENSION: usize = 16;

/// Extension to class, lower case, sorted by byte order, no duplicates.
const EXTENSIONS: &[(&str, KindClass)] = &[
    ("3mf", KindClass::Model),
    ("7z", KindClass::Archive),
    ("aac", KindClass::Audio),
    ("aif", KindClass::Audio),
    ("aiff", KindClass::Audio),
    ("app", KindClass::Application),
    ("bin", KindClass::Archive),
    ("blend", KindClass::Model),
    ("bmp", KindClass::Image),
    ("bz2", KindClass::Archive),
    ("c", KindClass::Code),
    ("cdr", KindClass::DiskImage),
    ("cpgz", KindClass::Archive),
    ("cpp", KindClass::Code),
    ("cr2", KindClass::Image),
    ("css", KindClass::Code),
    ("csv", KindClass::Document),
    ("dfont", KindClass::Font),
    ("dmg", KindClass::DiskImage),
    ("doc", KindClass::Document),
    ("docx", KindClass::Document),
    ("dsk", KindClass::DiskImage),
    ("epub", KindClass::Document),
    ("exe", KindClass::Application),
    ("fbx", KindClass::Model),
    ("flac", KindClass::Audio),
    ("gif", KindClass::Image),
    ("glb", KindClass::Model),
    ("gltf", KindClass::Model),
    ("gz", KindClass::Archive),
    ("h", KindClass::Code),
    ("heic", KindClass::Image),
    ("hqx", KindClass::Archive),
    ("html", KindClass::Code),
    ("icns", KindClass::Image),
    ("iges", KindClass::Model),
    ("igs", KindClass::Model),
    ("img", KindClass::DiskImage),
    ("iso", KindClass::DiskImage),
    ("java", KindClass::Code),
    ("jpeg", KindClass::Image),
    ("jpg", KindClass::Image),
    ("js", KindClass::Code),
    ("json", KindClass::Code),
    ("key", KindClass::Document),
    ("m", KindClass::Code),
    ("m4a", KindClass::Audio),
    ("m4v", KindClass::Video),
    ("md", KindClass::Document),
    ("mid", KindClass::Audio),
    ("mkv", KindClass::Video),
    ("mov", KindClass::Video),
    ("mp3", KindClass::Audio),
    ("mp4", KindClass::Video),
    ("mpeg", KindClass::Video),
    ("mpg", KindClass::Video),
    ("nef", KindClass::Image),
    ("numbers", KindClass::Document),
    ("obj", KindClass::Model),
    ("odt", KindClass::Document),
    ("off", KindClass::Model),
    ("ogg", KindClass::Audio),
    ("otf", KindClass::Font),
    ("pages", KindClass::Document),
    ("pct", KindClass::Image),
    ("pdf", KindClass::Document),
    ("pict", KindClass::Image),
    ("plist", KindClass::Code),
    ("ply", KindClass::Model),
    ("png", KindClass::Image),
    ("ppt", KindClass::Document),
    ("pptx", KindClass::Document),
    ("psd", KindClass::Image),
    ("py", KindClass::Code),
    ("rar", KindClass::Archive),
    ("raw", KindClass::Image),
    ("rs", KindClass::Code),
    ("rtf", KindClass::Document),
    ("sh", KindClass::Code),
    ("sit", KindClass::Archive),
    ("sitx", KindClass::Archive),
    ("sparsebundle", KindClass::DiskImage),
    ("sparseimage", KindClass::DiskImage),
    ("step", KindClass::Model),
    ("stl", KindClass::Model),
    ("stp", KindClass::Model),
    ("suit", KindClass::Font),
    ("svg", KindClass::Image),
    ("swift", KindClass::Code),
    ("tar", KindClass::Archive),
    ("tgz", KindClass::Archive),
    ("tif", KindClass::Image),
    ("tiff", KindClass::Image),
    ("toast", KindClass::DiskImage),
    ("ts", KindClass::Code),
    ("ttc", KindClass::Font),
    ("ttf", KindClass::Font),
    ("txt", KindClass::Document),
    ("usdz", KindClass::Model),
    ("wav", KindClass::Audio),
    ("webm", KindClass::Video),
    ("webp", KindClass::Image),
    ("wmv", KindClass::Video),
    ("xls", KindClass::Document),
    ("xlsx", KindClass::Document),
    ("xml", KindClass::Code),
    ("xz", KindClass::Archive),
    ("zip", KindClass::Archive),
];

/// Classic Mac type code (`FInfo.fdType`) to class, sorted by byte order, no
/// duplicates. Codes are compared exactly: case and trailing spaces count.
///
/// `BINA` (generic binary) is left out: it says nothing without its creator.
const TYPE_CODES: &[([u8; 4], KindClass)] = &[
    (*b"8BPS", KindClass::Image),
    (*b"AIFC", KindClass::Audio),
    (*b"AIFF", KindClass::Audio),
    (*b"APPL", KindClass::Application),
    (*b"BMPf", KindClass::Image),
    (*b"FFIL", KindClass::Font),
    (*b"GIFf", KindClass::Image),
    (*b"Gzip", KindClass::Archive),
    (*b"INIT", KindClass::Application),
    (*b"JPEG", KindClass::Image),
    (*b"LWFN", KindClass::Font),
    (*b"MPEG", KindClass::Video),
    (*b"MPG ", KindClass::Video),
    (*b"MPG3", KindClass::Audio),
    (*b"MiDi", KindClass::Audio),
    (*b"MooV", KindClass::Video),
    (*b"Mp3 ", KindClass::Audio),
    (*b"PDF ", KindClass::Document),
    (*b"PICT", KindClass::Image),
    (*b"PNGf", KindClass::Image),
    (*b"PNTG", KindClass::Image),
    (*b"SIT!", KindClass::Archive),
    (*b"SIT5", KindClass::Archive),
    (*b"SITD", KindClass::Archive),
    (*b"TEXT", KindClass::Document),
    (*b"TIFF", KindClass::Image),
    (*b"WAVE", KindClass::Audio),
    (*b"ZIP ", KindClass::Archive),
    (*b"appe", KindClass::Application),
    (*b"cdev", KindClass::Application),
    (*b"dImg", KindClass::DiskImage),
    (*b"devi", KindClass::DiskImage),
    (*b"dimg", KindClass::DiskImage),
    (*b"rohd", KindClass::DiskImage),
    (*b"sfil", KindClass::Audio),
    (*b"sfnt", KindClass::Font),
    (*b"tfil", KindClass::Font),
    (*b"ttro", KindClass::Document),
    (*b"utxt", KindClass::Document),
];

/// The class of a file: by extension; when that gives `Other` and a type
/// code is known, by type code. Never allocates.
///
/// The extension is the bytes after the last `.`, provided that dot is not
/// the first character of the name and the extension is non-empty and at
/// most 16 bytes. It is compared case-insensitively (ASCII). The type code
/// is compared exactly. Anything else is `Other`.
///
/// This never answers `Folder`: for an entry that may be a folder, use
/// [`classify_entry`].
pub fn classify(name: &str, finder_type: Option<[u8; 4]>) -> KindClass {
    let mut buf = [0u8; MAX_EXTENSION];
    let class = extension_lower(name, &mut buf).map_or(KindClass::Other, by_extension);
    if class != KindClass::Other {
        return class;
    }
    finder_type.map_or(KindClass::Other, by_type_code)
}

/// The class of any entry: [`classify`] for everything but a folder.
///
/// Folders are `Folder`, except a folder named `*.app`, which is
/// `Application`. No other extension changes a folder's class: a
/// `*.sparsebundle` is a folder on disk and stays `Folder`, although the same
/// name on a file is a `DiskImage`. A folder's type code is ignored. Never
/// allocates.
pub fn classify_entry(name: &str, is_folder: bool, finder_type: Option<[u8; 4]>) -> KindClass {
    if !is_folder {
        return classify(name, finder_type);
    }
    let mut buf = [0u8; MAX_EXTENSION];
    if extension_lower(name, &mut buf) == Some(b"app".as_slice()) {
        KindClass::Application
    } else {
        KindClass::Folder
    }
}

/// The class of a lower-cased extension; `Other` when it is not listed.
fn by_extension(ext: &[u8]) -> KindClass {
    match EXTENSIONS.binary_search_by(|(e, _)| e.as_bytes().cmp(ext)) {
        Ok(i) => EXTENSIONS.get(i).map_or(KindClass::Other, |&(_, c)| c),
        Err(_) => KindClass::Other,
    }
}

/// The class of a type code; `Other` when it is not listed.
fn by_type_code(code: [u8; 4]) -> KindClass {
    match TYPE_CODES.binary_search_by(|(c, _)| c.cmp(&code)) {
        Ok(i) => TYPE_CODES.get(i).map_or(KindClass::Other, |&(_, c)| c),
        Err(_) => KindClass::Other,
    }
}

/// The ASCII-lowercased extension of `name`, written into `buf`, or `None`
/// when the name has no usable extension.
fn extension_lower<'a>(name: &str, buf: &'a mut [u8; MAX_EXTENSION]) -> Option<&'a [u8]> {
    let dot = name.rfind('.')?;
    if dot == 0 {
        return None;
    }
    let ext = name.get(dot.checked_add(1)?..)?.as_bytes();
    if ext.is_empty() || ext.len() > MAX_EXTENSION {
        return None;
    }
    let out = buf.get_mut(..ext.len())?;
    for (slot, b) in out.iter_mut().zip(ext) {
        *slot = b.to_ascii_lowercase();
    }
    Some(out)
}

/// A set of classes, one bit per class. Empty means "any".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct KindSet(u16);

impl KindSet {
    pub const EMPTY: Self = KindSet(0);

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn contains(self, class: KindClass) -> bool {
        self.0 & (1u16 << class.bits()) != 0
    }

    #[must_use]
    pub const fn with(self, class: KindClass) -> Self {
        KindSet(self.0 | (1u16 << class.bits()))
    }

    #[must_use]
    pub const fn without(self, class: KindClass) -> Self {
        KindSet(self.0 & !(1u16 << class.bits()))
    }

    /// True when the set is empty or holds `class`.
    pub const fn admits(self, class: KindClass) -> bool {
        self.is_empty() || self.contains(class)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use KindClass as K;

    /// A file's class from its name alone.
    fn file(name: &str) -> KindClass {
        classify(name, None)
    }

    #[test]
    fn tables_are_sorted_and_unique() {
        for pair in EXTENSIONS.windows(2) {
            assert!(pair[0].0 < pair[1].0, "{} !< {}", pair[0].0, pair[1].0);
        }
        for (ext, _) in EXTENSIONS {
            assert!(!ext.is_empty() && ext.len() <= MAX_EXTENSION);
            assert!(ext.bytes().all(|b| !b.is_ascii_uppercase()), "{ext}");
        }
        for pair in TYPE_CODES.windows(2) {
            assert!(pair[0].0 < pair[1].0, "{:?} !< {:?}", pair[0].0, pair[1].0);
        }
        // Neither table ever answers Other or Folder.
        let classes = EXTENSIONS.iter().map(|e| e.1);
        for class in classes.chain(TYPE_CODES.iter().map(|t| t.1)) {
            assert!(class != K::Other && class != K::Folder);
        }
    }

    #[test]
    fn one_name_per_class() {
        assert_eq!(file("README"), K::Other);
        assert_eq!(classify_entry("Documents", true, None), K::Folder);
        assert_eq!(file("report.pdf"), K::Document);
        assert_eq!(file("photo.jpg"), K::Image);
        assert_eq!(file("song.mp3"), K::Audio);
        assert_eq!(file("movie.mov"), K::Video);
        assert_eq!(file("backup.zip"), K::Archive);
        assert_eq!(file("setup.exe"), K::Application);
        assert_eq!(file("main.rs"), K::Code);
        assert_eq!(file("install.dmg"), K::DiskImage);
        assert_eq!(file("face.ttf"), K::Font);
        assert_eq!(file("teapot.stl"), K::Model);
    }

    #[test]
    fn every_listed_extension_classifies_as_listed() {
        for &(ext, class) in EXTENSIONS {
            assert_eq!(file(&format!("name.{ext}")), class, "{ext}");
            assert_eq!(
                classify_entry(&format!("name.{ext}"), false, None),
                class,
                "{ext}"
            );
        }
    }

    #[test]
    fn model_extensions() {
        for ext in [
            "stl", "obj", "ply", "off", "gltf", "glb", "3mf", "usdz", "fbx", "step", "stp", "iges",
            "igs", "blend",
        ] {
            assert_eq!(file(&format!("part.{ext}")), K::Model, "{ext}");
            assert_eq!(
                file(&format!("PART.{}", ext.to_ascii_uppercase())),
                K::Model,
                "{ext}"
            );
        }
    }

    /// One line per type code of the design's table (r10-everyday.md §2).
    const EXPECTED_CODES: &[(&[u8; 4], KindClass)] = &[
        (b"TEXT", K::Document),
        (b"ttro", K::Document),
        (b"utxt", K::Document),
        (b"PDF ", K::Document),
        (b"APPL", K::Application),
        (b"appe", K::Application),
        (b"cdev", K::Application),
        (b"INIT", K::Application),
        (b"PICT", K::Image),
        (b"JPEG", K::Image),
        (b"GIFf", K::Image),
        (b"PNGf", K::Image),
        (b"TIFF", K::Image),
        (b"8BPS", K::Image),
        (b"PNTG", K::Image),
        (b"BMPf", K::Image),
        (b"MooV", K::Video),
        (b"MPEG", K::Video),
        (b"MPG ", K::Video),
        (b"AIFF", K::Audio),
        (b"AIFC", K::Audio),
        (b"WAVE", K::Audio),
        (b"sfil", K::Audio),
        (b"MPG3", K::Audio),
        (b"Mp3 ", K::Audio),
        (b"MiDi", K::Audio),
        (b"ZIP ", K::Archive),
        (b"SIT!", K::Archive),
        (b"SITD", K::Archive),
        (b"SIT5", K::Archive),
        (b"Gzip", K::Archive),
        (b"FFIL", K::Font),
        (b"tfil", K::Font),
        (b"sfnt", K::Font),
        (b"LWFN", K::Font),
        (b"dimg", K::DiskImage),
        (b"dImg", K::DiskImage),
        (b"rohd", K::DiskImage),
        (b"devi", K::DiskImage),
    ];

    #[test]
    fn every_type_code() {
        for &(code, class) in EXPECTED_CODES {
            let shown = String::from_utf8_lossy(code);
            // A classic name: no extension.
            assert_eq!(classify("Untitled", Some(*code)), class, "{shown:?}");
            assert_eq!(
                classify_entry("Untitled", false, Some(*code)),
                class,
                "{shown:?}"
            );
        }
        // The test list and the table hold the same codes.
        assert_eq!(EXPECTED_CODES.len(), TYPE_CODES.len());
        for &(code, class) in TYPE_CODES {
            assert!(
                EXPECTED_CODES.contains(&(&code, class)),
                "{:?} untested",
                String::from_utf8_lossy(&code)
            );
        }
    }

    #[test]
    fn type_codes_are_exact() {
        assert_eq!(classify("x", Some(*b"BINA")), K::Other);
        assert_eq!(classify("x", Some(*b"text")), K::Other);
        assert_eq!(classify("x", Some(*b"PDF\0")), K::Other);
        assert_eq!(classify("x", Some(*b"MPG\0")), K::Other);
        assert_eq!(classify("x", Some([0; 4])), K::Other);
        assert_eq!(classify("x", Some([0xFF; 4])), K::Other);
        assert_eq!(classify("x", None), K::Other);
    }

    #[test]
    fn classic_readme_is_a_document() {
        assert_eq!(classify("ReadMe", Some(*b"TEXT")), K::Document);
        assert_eq!(classify("ReadMe", None), K::Other);
    }

    #[test]
    fn extension_wins_over_type_code() {
        assert_eq!(classify("photo.jpg", Some(*b"TEXT")), K::Image);
        assert_eq!(classify("notes.txt", Some(*b"APPL")), K::Document);
        // An unknown extension falls through to the code.
        assert_eq!(classify("notes.bak", Some(*b"TEXT")), K::Document);
        assert_eq!(classify("v1.0 Installer", Some(*b"APPL")), K::Application);
    }

    #[test]
    fn folders_ignore_type_codes() {
        assert_eq!(classify_entry("Stuff", true, Some(*b"TEXT")), K::Folder);
        assert_eq!(
            classify_entry("Tool.app", true, Some(*b"TEXT")),
            K::Application
        );
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(file("PHOTO.JPG"), K::Image);
        assert_eq!(file("Photo.JpEg"), K::Image);
        assert_eq!(file("DISK.SparseImage"), K::DiskImage);
    }

    #[test]
    fn missing_or_unusable_extension() {
        assert_eq!(file("Makefile"), K::Other);
        assert_eq!(file(".profile"), K::Other);
        assert_eq!(file(".png"), K::Other);
        assert_eq!(file("file."), K::Other);
        assert_eq!(file(""), K::Other);
        assert_eq!(file("."), K::Other);
        assert_eq!(file("a.unknownext"), K::Other);
        // 17 bytes: over the limit.
        assert_eq!(file("a.abcdefghijklmnopq"), K::Other);
        // Exactly 16 bytes is looked up (and is simply not found).
        assert_eq!(file("a.abcdefghijklmnop"), K::Other);
        assert_eq!(file("..png"), K::Image);
        // An unusable extension still lets the type code decide.
        assert_eq!(classify("a.abcdefghijklmnopq", Some(*b"PICT")), K::Image);
        assert_eq!(classify(".png", Some(*b"MooV")), K::Video);
    }

    #[test]
    fn multiple_dots() {
        assert_eq!(file("archive.tar.gz"), K::Archive);
        assert_eq!(file("notes.txt.bak"), K::Other);
        assert_eq!(file("v1.2.pdf"), K::Document);
    }

    #[test]
    fn folders() {
        let folder = |n: &str| classify_entry(n, true, None);
        assert_eq!(folder("Safari.app"), K::Application);
        assert_eq!(folder("SAFARI.APP"), K::Application);
        assert_eq!(folder("photos.png"), K::Folder);
        assert_eq!(folder("disk.sparsebundle"), K::Folder);
        assert_eq!(file("disk.sparsebundle"), K::DiskImage);
        assert_eq!(folder(".app"), K::Folder);
        assert_eq!(folder("app"), K::Folder);
        assert_eq!(file("Safari.app"), K::Application);
    }

    #[test]
    fn non_ascii_names() {
        assert_eq!(file("日本語.pdf"), K::Document);
        assert_eq!(file("Écoute.MP3"), K::Audio);
        assert_eq!(file("file.日本"), K::Other);
        assert_eq!(file("file.pdf日"), K::Other);
        assert_eq!(file("日本語"), K::Other);
        assert_eq!(classify_entry("Ünï.app", true, None), K::Application);
        assert_eq!(classify("日本語", Some(*b"TEXT")), K::Document);
    }

    #[test]
    fn bits_round_trip() {
        for (i, class) in KindClass::ALL.into_iter().enumerate() {
            assert_eq!(usize::from(class.bits()), i);
            assert_eq!(KindClass::from_bits(class.bits()), class);
        }
        // The old classes keep their numbers.
        assert_eq!(K::Other.bits(), 0);
        assert_eq!(K::Image.bits(), 3);
        assert_eq!(K::Font.bits(), 10);
        assert_eq!(K::Model.bits(), 11);
        assert_eq!(KindClass::from_bits(12), K::Other);
        assert_eq!(KindClass::from_bits(15), K::Other);
        assert_eq!(KindClass::from_bits(u16::MAX), K::Other);
        assert!(KindClass::ALL.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn labels() {
        assert_eq!(K::Other.label(), "Other");
        assert_eq!(K::Image.label(), "Picture");
        assert_eq!(K::Video.label(), "Movie");
        assert_eq!(K::Code.label(), "Code and data");
        assert_eq!(K::Model.label(), "3D model");
        assert_eq!(K::DiskImage.label(), "Disk image");
        assert_eq!(K::Font.label(), "Font");
        let mut labels: Vec<_> = KindClass::ALL.iter().map(|c| c.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), KindClass::ALL.len());
    }

    #[test]
    fn kind_set() {
        let empty = KindSet::EMPTY;
        assert!(empty.is_empty());
        assert_eq!(empty, KindSet::default());
        for class in KindClass::ALL {
            assert!(!empty.contains(class));
            assert!(empty.admits(class));
        }
        let set = empty.with(K::Image).with(K::Font);
        assert!(!set.is_empty());
        assert!(set.contains(K::Image));
        assert!(set.contains(K::Font));
        assert!(!set.contains(K::Audio));
        assert!(set.admits(K::Image));
        assert!(!set.admits(K::Audio));
        assert_eq!(set.with(K::Image), set);
        let set = set.without(K::Image);
        assert!(!set.contains(K::Image));
        assert!(set.contains(K::Font));
        assert_eq!(set.without(K::Audio), set);
        assert!(set.without(K::Font).is_empty());
        let all = KindClass::ALL.iter().fold(empty, |s, &c| s.with(c));
        assert!(KindClass::ALL.iter().all(|&c| all.contains(c)));
        assert!(empty.with(K::Model).contains(K::Model));
    }
}
