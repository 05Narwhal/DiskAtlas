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

//! The facts about a file or folder, as rows of text: the details pane's, Quick Look's Details
//! tab's, and the Properties window's. No view code lives here; the views draw what this builds.
//!
//! The section loaders of the Properties window (`general_item`, `mac_item`, `on_disk_item`, and
//! so on) are pure: the caller reads the filesystem on the background executor and passes the
//! results in; they come back as [`Fact`] rows.

use chrono::{DateTime, FixedOffset, Local, TimeZone};
use disk_atlas_lib::fs::{
    EntryFlags, Extent, FinderCodes, JournalState, Metadata, NodeId, NodeKind, Owner, Stream,
    Timestamp, VolumeIdentity, VolumeInfo, XattrInfo,
};
use disk_atlas_lib::mac::xattr::MacAttributes;
use disk_atlas_lib::partition::Scheme;
use disk_atlas_lib::sizes::{ClassBytes, Totals};
use gpui::SharedString;

use crate::components::storage_bar::{legend, piece_name};
use crate::format;
use crate::state::checksum::Checksum;
use crate::state::quick_look::FileDetails;
use crate::state::sources::{DiskId, SourceId};

/// One row of any facts list: the details pane, Quick Look's Details tab, Properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    pub label: SharedString,
    pub value: SharedString,
    /// What "Copy" puts on the clipboard when it differs from `value` (the exact byte count, not "18.2 GB").
    pub copy: Option<SharedString>,
    /// "Where" and "Disk" rows: clicking goes there in the main window.
    pub goes_to: Option<FactLink>,
    /// The value is still being worked out: drawn in `ink3`.
    pub running: bool,
}

/// Where a "Where", "Disk" or "Largest" row leads in the main window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactLink {
    /// Show this folder or item in the file list.
    Folder { source: SourceId, node: NodeId },
    /// Show this disk's overview.
    Disk(DiskId),
}

impl Fact {
    /// A finished fact whose copied text is its value.
    pub fn new(label: impl Into<SharedString>, value: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            copy: None,
            goes_to: None,
            running: false,
        }
    }

    /// A line with no label, in the value column: "and 4 more", "Some sizes are a lower bound."
    pub fn note(text: impl Into<SharedString>) -> Self {
        Self::new("", text)
    }

    /// `self` with the text "Copy" puts on the clipboard.
    #[must_use]
    pub fn copying(mut self, text: impl Into<SharedString>) -> Self {
        self.copy = Some(text.into());
        self
    }

    /// `self` with the place a click on the row goes to.
    #[must_use]
    pub fn linking(mut self, link: FactLink) -> Self {
        self.goes_to = Some(link);
        self
    }

    /// `self` marked as still being worked out.
    #[must_use]
    pub fn running(mut self) -> Self {
        self.running = true;
        self
    }
}

/// What `FileSystem::metadata` adds to what the listing already knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extra {
    pub created: Option<Timestamp>,
    pub accessed: Option<Timestamp>,
    pub link_target: Option<String>,
    pub resource_size: Option<u64>,
    pub finder_codes: Option<FinderCodes>,
    pub owner: Option<Owner>,
    pub attributes: Option<u32>,
    /// The filesystem stores the content compressed.
    pub compressed: bool,
}

/// The line under a name: the kind and, when there is one, the size, and ", compressed" when the
/// filesystem stores the content compressed. "PNG image, 1.8 MB".
pub fn subtitle(
    name: &str,
    kind: NodeKind,
    codes: Option<FinderCodes>,
    size: Option<u64>,
    browsable: bool,
    compressed: bool,
) -> String {
    let kind = kind_text(name, kind, codes);
    let mut line = match size.filter(|_| !browsable) {
        Some(bytes) => format!("{kind}, {}", format::size(bytes)),
        None => kind.to_string(),
    };
    if compressed {
        line.push_str(", compressed");
    }
    line
}

/// What a classic Mac OS type code says about a file, in plain words. Only the codes everyone
/// met; anything else is the caller's fallback.
fn type_code_kind(code: [u8; 4]) -> Option<&'static str> {
    Some(match &code {
        b"TEXT" => "Plain text",
        b"APPL" => "Application",
        b"PICT" => "PICT image",
        b"JPEG" => "JPEG image",
        b"GIFf" => "GIF image",
        b"PNGf" => "PNG image",
        b"TIFF" => "TIFF image",
        b"BMPf" => "BMP image",
        b"MooV" => "Movie",
        b"MPEG" | b"MPG " => "Movie",
        b"AIFF" | b"AIFC" => "Audio",
        b"WAVE" => "Audio",
        b"sfil" => "Sound",
        b"PDF " => "PDF document",
        b"RTF " => "Rich text",
        b"ZIP " => "ZIP archive",
        b"SIT!" | b"SITD" => "StuffIt archive",
        b"rsrc" => "Resource file",
        b"clpp" | b"clpt" | b"clps" | b"clpu" => "Clipping",
        b"FFIL" | b"tfil" => "Font",
        _ => return None,
    })
}

/// The kind in plain words: the extension's, or, when that gives only "Document", the type
/// code's.
pub fn kind_text(name: &str, kind: NodeKind, codes: Option<FinderCodes>) -> &'static str {
    let by_name = format::kind(name, kind);
    if kind == NodeKind::File
        && by_name == "Document"
        && let Some(by_code) = codes.and_then(|codes| type_code_kind(codes.file_type))
    {
        return by_code;
    }
    by_name
}

/// "1 transaction", "12 transactions"
fn transactions(count: u32) -> String {
    if count == 1 {
        "1 transaction".to_string()
    } else {
        format!("{} transactions", format::grouped(u64::from(count)))
    }
}

/// The Journal row's text: what mounting did with the volume's journal. `None` for an outcome
/// this version does not know how to word.
pub fn journal_text(state: &JournalState) -> Option<String> {
    match state {
        JournalState::Clean => Some("Clean".to_string()),
        JournalState::Replayed {
            transactions: count,
            stopped_early: false,
        } => Some(format!("Replayed in memory, {}", transactions(*count))),
        JournalState::Replayed {
            transactions: count,
            stopped_early: true,
        } => Some(format!(
            "Replayed in memory up to a damaged entry, {}",
            transactions(*count)
        )),
        JournalState::NotReplayed { reason } => {
            Some(format!("Not replayed, shown as on disk ({reason})"))
        }
        _ => None,
    }
}

/// "1 extended attribute", "3 extended attributes"
fn attributes_text(count: u32) -> String {
    if count == 1 {
        "1 extended attribute".to_string()
    } else {
        format!("{} extended attributes", format::grouped(u64::from(count)))
    }
}

/// "3 items selected"
pub fn selection_text(count: usize) -> String {
    format!("{} selected", format::items(count))
}

/// The rows of facts, each only when its value is known, in display order.
pub fn facts(
    modified: Option<Timestamp>,
    extra: Option<&Extra>,
) -> Vec<(&'static str, SharedString)> {
    let date = |time: Timestamp| SharedString::from(format::date_time(time));
    let mut rows = Vec::new();
    if let Some(created) = extra.and_then(|extra| extra.created) {
        rows.push(("Created", date(created)));
    }
    if let Some(modified) = modified {
        rows.push(("Modified", date(modified)));
    }
    if let Some(accessed) = extra.and_then(|extra| extra.accessed) {
        rows.push(("Opened", date(accessed)));
    }
    if let Some(target) = extra.and_then(|extra| extra.link_target.as_deref()) {
        rows.push(("Link to", SharedString::from(target.to_owned())));
    }
    if let Some(bytes) = extra
        .and_then(|extra| extra.resource_size)
        .filter(|&bytes| bytes > 0)
    {
        rows.push(("Resource fork", SharedString::from(format::size(bytes))));
    }
    if let Some(codes) = extra.and_then(|extra| extra.finder_codes.as_ref()) {
        rows.push((
            "Type, creator",
            SharedString::from(format::finder_codes(codes)),
        ));
    }
    if let Some(owner) = extra.and_then(|extra| extra.owner.as_ref()) {
        rows.push(("Owner", SharedString::from(format::owner(owner))));
    }
    if let Some(count) = extra
        .and_then(|extra| extra.attributes)
        .filter(|&count| count > 0)
    {
        rows.push(("Attributes", SharedString::from(attributes_text(count))));
    }
    rows
}

/// The most runs of a file's extents the Details tab lists.
pub const LISTED_EXTENTS: usize = 8;

/// "12 bytes", "4.2 MB (4,404,019 bytes)"
pub(crate) fn size_with_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format::size(bytes)
    } else {
        format!("{} ({} bytes)", format::size(bytes), format::grouped(bytes))
    }
}

/// "12 Jan 2020, 09:41:07" in the computer's time zone, or as stored when `local_times` says the
/// volume keeps no zone. "Unknown" for a time that cannot be shown.
pub(crate) fn date_seconds(time: Timestamp, local_times: bool) -> String {
    let Some(utc) = DateTime::from_timestamp(time.unix_seconds(), 0) else {
        return "Unknown".to_owned();
    };
    let offset = if local_times {
        0
    } else {
        Local
            .offset_from_utc_datetime(&utc.naive_utc())
            .local_minus_utc()
    };
    date_seconds_at_offset(time, offset)
}

/// [`date_seconds`] for an explicit UTC offset in seconds.
fn date_seconds_at_offset(time: Timestamp, utc_offset_seconds: i32) -> String {
    DateTime::from_timestamp(time.unix_seconds(), 0)
        .zip(FixedOffset::east_opt(utc_offset_seconds))
        .map_or_else(
            || "Unknown".to_owned(),
            |(utc, offset)| {
                utc.with_timezone(&offset)
                    .format("%-d %b %Y, %H:%M:%S")
                    .to_string()
            },
        )
}

/// The nine permission letters of a BSD mode: `rw-r--r--`.
fn permission_letters(mode: u16) -> String {
    let mut letters = String::with_capacity(9);
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 0o7;
        letters.push(if bits & 0o4 != 0 { 'r' } else { '-' });
        letters.push(if bits & 0o2 != 0 { 'w' } else { '-' });
        letters.push(if bits & 0o1 != 0 { 'x' } else { '-' });
    }
    letters
}

/// The permission bits as the Properties window shows them: `rw-r--r-- (644)`.
pub fn permission_text(mode: u16) -> String {
    format!("{} ({:03o})", permission_letters(mode), mode & 0o777)
}

/// The BSD mode in octal with the permission letters: `100644 (-rw-r--r--)`.
fn mode_text(mode: u16) -> String {
    let kind = match mode & 0o170_000 {
        0o040_000 => 'd',
        0o120_000 => 'l',
        0o100_000 => '-',
        0o020_000 => 'c',
        0o060_000 => 'b',
        0o010_000 => 'p',
        0o140_000 => 's',
        _ => '?',
    };
    format!("{mode:06o} ({kind}{})", permission_letters(mode))
}

/// The classic Finder label names by label number: 0 is none, then Mac OS X's order.
const LABELS: [&str; 8] = [
    "none", "gray", "green", "purple", "blue", "yellow", "red", "orange",
];

/// The Finder flags word, decoded: `0x4100: invisible, has been inited`.
fn finder_flags_text(flags: u16) -> String {
    const NAMED: [(u16, &str); 10] = [
        (0x0001, "on desktop"),
        (0x0040, "shared"),
        (0x0080, "no INITs"),
        (0x0100, "has been inited"),
        (0x0400, "custom icon"),
        (0x0800, "stationery"),
        (0x1000, "name locked"),
        (0x2000, "has bundle"),
        (0x4000, "invisible"),
        (0x8000, "alias"),
    ];
    let mut parts: Vec<String> = NAMED
        .iter()
        .filter(|(bit, _)| flags & bit != 0)
        .map(|(_, name)| (*name).to_owned())
        .collect();
    let label = usize::from((flags >> 1) & 0x7);
    if label != 0 {
        parts.push(format!(
            "label {}",
            LABELS.get(label).copied().unwrap_or("?")
        ));
    }
    if parts.is_empty() {
        format!("{flags:#06x}: none set")
    } else {
        format!("{flags:#06x}: {}", parts.join(", "))
    }
}

/// The Finder flags of the 32 bytes of Finder information: a big-endian word at byte 8 for a
/// file (`fdFlags`) and a folder (`frFlags`) alike.
fn finder_flags(info: &[u8; 32]) -> u16 {
    u16::from_be_bytes([info[8], info[9]])
}

/// "3 pieces"
pub fn pieces_count_text(count: usize) -> String {
    match count {
        0 => "no pieces".to_owned(),
        1 => "1 piece".to_owned(),
        n => format!("{} pieces", format::grouped(n as u64)),
    }
}

/// "On disk: 3 pieces"
pub fn pieces_text(count: usize) -> String {
    format!("On disk: {}", pieces_count_text(count))
}

/// One row of the Details tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetailRow {
    /// A bold line that starts a group.
    Heading(SharedString),
    /// A label and its value.
    Fact(&'static str, SharedString),
    /// The column titles of a table.
    Columns(&'static str, &'static str),
    /// One row of a table.
    Pair(SharedString, SharedString),
    /// A sentence in the secondary colour.
    Note(SharedString),
}

/// The rows of the extended attributes table.
fn xattr_rows(xattrs: &Result<Vec<XattrInfo>, SharedString>, rows: &mut Vec<DetailRow>) {
    match xattrs {
        Err(text) => {
            rows.push(DetailRow::Heading("Extended attributes".into()));
            rows.push(DetailRow::Note(text.clone()));
        }
        Ok(list) if list.is_empty() => {
            rows.push(DetailRow::Heading("Extended attributes".into()));
            rows.push(DetailRow::Note("None.".into()));
        }
        Ok(list) => {
            rows.push(DetailRow::Heading(
                format!(
                    "Extended attributes ({})",
                    format::grouped(list.len() as u64)
                )
                .into(),
            ));
            rows.push(DetailRow::Columns("Name", "Size"));
            rows.extend(list.iter().map(|attribute| {
                DetailRow::Pair(
                    attribute.name.clone().into(),
                    format::size(attribute.size).into(),
                )
            }));
        }
    }
}

/// The rows of the extents table: the first [`LISTED_EXTENTS`] runs.
fn extent_rows(
    stream: Stream,
    extents: &Result<Vec<Extent>, SharedString>,
    rows: &mut Vec<DetailRow>,
) {
    let fork = if stream == Stream::Resource {
        " (resource fork)"
    } else {
        ""
    };
    match extents {
        Err(text) => {
            rows.push(DetailRow::Heading(format!("On disk{fork}").into()));
            rows.push(DetailRow::Note(text.clone()));
        }
        Ok(runs) => {
            rows.push(DetailRow::Heading(
                format!("{}{fork}", pieces_text(runs.len())).into(),
            ));
            if runs.is_empty() {
                return;
            }
            rows.push(DetailRow::Columns("Byte offset", "Length"));
            rows.extend(runs.iter().take(LISTED_EXTENTS).map(|run| {
                DetailRow::Pair(
                    format!("{} (0x{:X})", format::grouped(run.offset), run.offset).into(),
                    size_with_bytes(run.len).into(),
                )
            }));
            if runs.len() > LISTED_EXTENTS {
                let more = (runs.len() - LISTED_EXTENTS) as u64;
                rows.push(DetailRow::Note(
                    format!("and {} more", format::grouped(more)).into(),
                ));
            }
        }
    }
}

/// Everything the volume records about the file, in technical words: the rows of Quick Look's
/// Details tab.
pub fn tab_rows(name: &str, details: &FileDetails) -> Vec<DetailRow> {
    let metadata = &details.metadata;
    let date = |time: Timestamp| SharedString::from(date_seconds(time, details.local_times));
    let mut rows = vec![
        DetailRow::Fact("Name", SharedString::from(name.to_owned())),
        DetailRow::Fact(
            "Kind",
            kind_text(name, metadata.kind, metadata.finder_codes).into(),
        ),
        DetailRow::Fact("Node id", format::grouped(metadata.node.0).into()),
    ];
    if let Some(size) = metadata.size {
        rows.push(DetailRow::Fact("Data fork", size_with_bytes(size).into()));
    }
    if let Some(size) = metadata.resource_size {
        rows.push(DetailRow::Fact(
            "Resource fork",
            size_with_bytes(size).into(),
        ));
    }
    for (label, time) in [
        ("Created", metadata.created),
        ("Modified", metadata.modified),
        ("Accessed", metadata.accessed),
    ] {
        if let Some(time) = time {
            rows.push(DetailRow::Fact(label, date(time)));
        }
    }
    if let Some(target) = &metadata.link_target {
        rows.push(DetailRow::Fact("Link to", target.clone().into()));
    }
    if let Some(owner) = &metadata.owner {
        rows.push(DetailRow::Fact("Mode", mode_text(owner.mode).into()));
        rows.push(DetailRow::Fact(
            "Owner",
            format!("user {}, group {}", owner.user, owner.group).into(),
        ));
    }
    if let Some(codes) = &metadata.finder_codes {
        rows.push(DetailRow::Fact(
            "Type, creator",
            format::finder_codes(codes).into(),
        ));
    }
    if let Some(info) = &metadata.finder_info {
        rows.push(DetailRow::Fact(
            "Finder flags",
            finder_flags_text(finder_flags(info)).into(),
        ));
    }
    if metadata.flags.contains(EntryFlags::COMPRESSED) {
        rows.push(DetailRow::Fact(
            "Compressed",
            "Yes, the volume stores the content compressed".into(),
        ));
    }
    if metadata.flags.contains(EntryFlags::HIDDEN) {
        rows.push(DetailRow::Fact("Hidden", "Yes".into()));
    }
    xattr_rows(&details.xattrs, &mut rows);
    extent_rows(details.stream, &details.extents, &mut rows);
    rows
}

// ---------------------------------------------------------------------------------------------
// The Properties window's section loaders (design `r10-everyday.md` §7, R18.2a). Pure functions:
// the caller reads the filesystem and passes what it found; the rows come back.
// ---------------------------------------------------------------------------------------------

/// How one section of the Properties window stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SectionState {
    /// Being read; the window draws the head and a quiet line.
    Loading,
    /// Finished.
    Ready(Vec<Fact>),
    /// Nothing to show: the section is not drawn.
    Absent,
    /// It could not be read; the window shows the head and this reason.
    Failed(SharedString),
    /// The Checksum section: a button, a running calculation or a digest.
    Checksum(Checksum),
}

/// The sections of the Properties window, in the order the window draws them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SectionId {
    General,
    Inside,
    Mac,
    OnDisk,
    Permissions,
    Xattrs,
    Checksum,
    Volume,
    Disk,
}

impl SectionId {
    /// The head of the section.
    pub fn title(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Inside => "What is inside",
            Self::Mac => "Mac information",
            Self::OnDisk => "On the disk",
            Self::Permissions => "Permissions",
            Self::Xattrs => "Extended attributes",
            Self::Checksum => "Checksum",
            Self::Volume => "Volume",
            Self::Disk => "Disk",
        }
    }
}

/// What the loaders know about an item before any disk read: copied from the listing row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemFacts {
    pub name: SharedString,
    pub kind: NodeKind,
    pub size: Option<u64>,
    pub modified: Option<Timestamp>,
    pub browsable: bool,
}

/// The file's pieces on the volume as stretches of a strip of the volume's length.
#[derive(Debug, Clone, PartialEq)]
pub struct PieceStrip {
    /// Start and end of each stretch as fractions of the volume's length, 0 to 1, ascending;
    /// pieces that touch or overlap are one stretch.
    pub spans: Vec<(f32, f32)>,
}

impl PieceStrip {
    /// The strip of `runs` on a volume of `volume_len` bytes. `None` for a volume of no length.
    /// Runs that lie wholly past the end of the volume are left out and the rest are clipped to it.
    fn of(runs: &[Extent], volume_len: u64) -> Option<Self> {
        if volume_len == 0 {
            return None;
        }
        let mut ranges: Vec<(u64, u64)> = runs
            .iter()
            .map(|run| {
                (
                    run.offset,
                    run.offset.saturating_add(run.len).min(volume_len),
                )
            })
            .filter(|(start, end)| start < end)
            .collect();
        ranges.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for (start, end) in ranges {
            match merged.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }
        let whole = volume_len as f64;
        let spans = merged
            .into_iter()
            .map(|(start, end)| ((start as f64 / whole) as f32, (end as f64 / whole) as f32))
            .collect();
        Some(Self { spans })
    }
}

/// What the host says about a file that a BSD mode does not: Windows attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostFlags {
    pub read_only: bool,
    pub hidden: bool,
    pub system: bool,
}

/// One of the largest items of a folder, for "What is inside".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LargestItem {
    pub name: SharedString,
    pub bytes: u64,
    /// Where a click on the row goes in the main window.
    pub link: Option<FactLink>,
}

/// The most "largest" rows of "What is inside".
pub const LISTED_LARGEST: usize = 5;

/// Where a volume lies on its disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionFacts {
    pub number: Option<u32>,
    /// First byte on the disk.
    pub start: u64,
    pub len: u64,
    /// The disk's name and the id its row links to.
    pub disk: Option<(SharedString, DiskId)>,
}

/// One partition row of a disk: name, format, start, length, and `Ok` when the partition can be
/// read or the reason it cannot.
pub type PartitionRow = (
    SharedString,
    Option<SharedString>,
    u64,
    u64,
    Result<(), SharedString>,
);

/// What the "Disk" section shows. (`state::menus::DiskFacts` is the context menu's.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskDescription {
    pub model: SharedString,
    pub connection: Option<SharedString>,
    pub size: u64,
    pub sector_size: Option<u32>,
    /// The partition map in words: see [`scheme_name`].
    pub scheme: Option<&'static str>,
    pub device_path: SharedString,
    pub partitions: Vec<PartitionRow>,
}

/// The partition map's name for the "Partition map" row.
pub fn scheme_name(scheme: Scheme) -> &'static str {
    match scheme {
        Scheme::Gpt => "GUID partition table",
        Scheme::Mbr => "Master Boot Record",
        Scheme::Apm => "Apple partition map",
    }
}

/// "Yes" or "No".
fn yes_no(value: bool) -> &'static str {
    if value { "Yes" } else { "No" }
}

/// `text` with its first letter in upper case.
fn capitalised(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// A size row: "18.2 GB (19,540,221,184 bytes)", copying the bare digit count.
fn size_fact(label: &'static str, bytes: u64) -> Fact {
    Fact::new(label, size_with_bytes(bytes)).copying(bytes.to_string())
}

/// "2,140 files, 96 folders"; "1 file", "1 folder".
fn contains_text(totals: Totals) -> String {
    let files = if totals.files == 1 {
        "1 file".to_owned()
    } else {
        format!("{} files", format::grouped(totals.files))
    };
    let folders = if totals.folders == 1 {
        "1 folder".to_owned()
    } else {
        format!("{} folders", format::grouped(totals.folders))
    };
    format!("{files}, {folders}")
}

/// `Ready(rows)`, or `Absent` when there are none.
fn ready_or_absent(rows: Vec<Fact>) -> SectionState {
    if rows.is_empty() {
        SectionState::Absent
    } else {
        SectionState::Ready(rows)
    }
}

/// General, for one item: kind, size, on disk, contents, where, dates. Each row only when known.
///
/// `on_disk` is the data stream's extents (`None` when they were not asked for), `totals` the
/// folder's totals, `where_` the name of the folder it is in and where a click goes. `host` is
/// true on the host's own filesystem, which has no "On disk" row.
pub fn general_item(
    item: &ItemFacts,
    md: &Metadata,
    on_disk: Option<&Result<Vec<Extent>, SharedString>>,
    info: &VolumeInfo,
    totals: Option<Totals>,
    where_: Option<(SharedString, FactLink)>,
    host: bool,
) -> SectionState {
    let mut rows = vec![Fact::new(
        "Kind",
        kind_text(&item.name, item.kind, md.finder_codes),
    )];
    let folder = item.kind == NodeKind::Directory;
    let size = if folder {
        item.size.or(totals.map(|totals| totals.bytes))
    } else {
        item.size.or(md.size)
    };
    if let Some(bytes) = size {
        rows.push(size_fact("Size", bytes));
    }
    if item.kind == NodeKind::File
        && !host
        && let Some(Ok(runs)) = on_disk
    {
        let sum = runs
            .iter()
            .fold(0u64, |sum, run| sum.saturating_add(run.len));
        rows.push(Fact::new("On disk", format::size(sum)).copying(sum.to_string()));
    }
    if folder && let Some(totals) = totals {
        rows.push(Fact::new("Contains", contains_text(totals)));
    }
    if let Some((name, link)) = where_ {
        rows.push(Fact::new("Where", name).linking(link));
    }
    let date = |time: Timestamp| date_seconds(time, info.local_times);
    for (label, time) in [
        ("Created", md.created),
        ("Modified", md.modified.or(item.modified)),
        ("Opened", md.accessed),
    ] {
        if let Some(time) = time {
            rows.push(Fact::new(label, date(time)));
        }
    }
    SectionState::Ready(rows)
}

/// The Finder flags named in words, in the order of the Properties design.
fn finder_flag_words(md: &Metadata) -> Vec<&'static str> {
    let word = md.finder_info.as_ref().map_or(0, finder_flags);
    let folder = md.kind == NodeKind::Directory;
    let mut words = Vec::new();
    if word & 0x4000 != 0 {
        words.push("Invisible");
    }
    if word & 0x8000 != 0 || md.flags.contains(EntryFlags::ALIAS) {
        words.push("Alias");
    }
    if word & 0x0400 != 0 {
        words.push("Custom icon");
    }
    if word & 0x0800 != 0 {
        words.push("Stationery");
    }
    if word & 0x1000 != 0 {
        words.push("Name locked");
    }
    // On a classic file the bundle bit means "has a BNDL resource", not a package.
    if folder && (word & 0x2000 != 0 || md.flags.contains(EntryFlags::PACKAGE)) {
        words.push("Package");
    }
    words
}

/// The Finder label number, 0 to 7: the entry's, or else the colour bits of the Finder flags.
fn finder_label(md: &Metadata) -> usize {
    match md.flags.label() {
        0 => md
            .finder_info
            .as_ref()
            .map_or(0, |info| usize::from((finder_flags(info) >> 1) & 0x7)),
        label => usize::from(label),
    }
}

/// Mac information: type and creator, label, flags in words, resource fork. `Absent` when none of
/// them exists. `sidecar_fork_from` is the name of the `._` file the resource fork came from.
pub fn mac_item(md: &Metadata, sidecar_fork_from: Option<&str>) -> SectionState {
    let mut rows = Vec::new();
    if let Some(codes) = &md.finder_codes {
        rows.push(Fact::new(
            "Type and creator",
            format::finder_codes(codes).replacen(", ", " \u{b7} ", 1),
        ));
    }
    let label = finder_label(md);
    if label != 0
        && let Some(name) = LABELS.get(label)
    {
        rows.push(Fact::new("Label", capitalised(name)));
    }
    let words = finder_flag_words(md);
    if !words.is_empty() {
        rows.push(Fact::new("Flags in words", words.join(", ")));
    }
    if let Some(bytes) = md.resource_size.filter(|&bytes| bytes > 0) {
        let text = match sidecar_fork_from {
            Some(name) => format!("{} (from ._{name})", format::size(bytes)),
            None => format::size(bytes),
        };
        rows.push(Fact::new("Resource fork", text).copying(bytes.to_string()));
    }
    ready_or_absent(rows)
}

/// On the disk: the pieces and where they lie, the node id, compression; with the strip that
/// shows the pieces on a volume of `volume_len` bytes (`None` or 0: no strip).
pub fn on_disk_item(
    extents: &Result<Vec<Extent>, SharedString>,
    md: &Metadata,
    volume_len: Option<u64>,
) -> (SectionState, Option<PieceStrip>) {
    let runs = match extents {
        Ok(runs) => runs,
        Err(text) => return (SectionState::Failed(text.clone()), None),
    };
    let mut rows = vec![Fact::new("Pieces", pieces_count_text(runs.len()))];
    for (number, run) in (1..).zip(runs.iter().take(LISTED_EXTENTS)) {
        rows.push(Fact::new(
            format!("Piece {number}"),
            format!(
                "{} (0x{:X}), {}",
                format::grouped(run.offset),
                run.offset,
                size_with_bytes(run.len)
            ),
        ));
    }
    if runs.len() > LISTED_EXTENTS {
        let more = (runs.len() - LISTED_EXTENTS) as u64;
        rows.push(Fact::note(format!("and {} more", format::grouped(more))));
    }
    rows.push(Fact::new("Node id", format::grouped(md.node.0)));
    if md.flags.contains(EntryFlags::COMPRESSED) {
        rows.push(Fact::new("Compressed", "Yes"));
    }
    let strip = volume_len.and_then(|len| PieceStrip::of(runs, len));
    (SectionState::Ready(rows), strip)
}

/// Permissions: owner and group as the volume wrote them, the mode, and the host's Windows flags.
/// `Absent` when nothing is recorded.
pub fn permissions_item(md: &Metadata, host_flags: Option<HostFlags>) -> SectionState {
    let mut rows = Vec::new();
    if let Some(owner) = &md.owner {
        rows.push(Fact::new(
            "Owner and group",
            format!("user {}, group {}", owner.user, owner.group),
        ));
        if owner.mode != 0 {
            rows.push(Fact::new("Mode", permission_text(owner.mode)));
        }
        rows.push(Fact::new(
            "Names",
            "Names belong to the Mac that wrote the disk.",
        ));
    }
    if let Some(flags) = host_flags {
        rows.push(Fact::new("Read-only", yes_no(flags.read_only)));
        rows.push(Fact::new("Hidden", yes_no(flags.hidden)));
        rows.push(Fact::new("System", yes_no(flags.system)));
    }
    ready_or_absent(rows)
}

/// Extended attributes: a row per attribute with the size of its value. `Absent` when there are
/// none, `Failed` with the reason when they could not be read.
pub fn xattrs_item(xattrs: &Result<Vec<XattrInfo>, SharedString>) -> SectionState {
    match xattrs {
        Err(text) => SectionState::Failed(text.clone()),
        Ok(list) => ready_or_absent(
            list.iter()
                .map(|attribute| {
                    Fact::new(attribute.name.clone(), format::size(attribute.size))
                        .copying(attribute.size.to_string())
                })
                .collect(),
        ),
    }
}

/// The Finder's tag colours in the order of the stored number, 1 to 7.
fn tag_colour(colour: u8) -> Option<&'static str> {
    [
        None,
        Some("Gray"),
        Some("Green"),
        Some("Purple"),
        Some("Blue"),
        Some("Yellow"),
        Some("Red"),
        Some("Orange"),
    ]
    .get(usize::from(colour))
    .copied()
    .flatten()
}

/// What the Mac attributes say, in words: where a file came from, when and by what it was
/// downloaded, its tags and its Finder comment. Empty when it has none of them.
pub fn mac_attribute_facts(attributes: &MacAttributes) -> Vec<Fact> {
    let mut rows = Vec::new();
    if let Some(origin) = attributes.where_from.first() {
        let mut fact = Fact::new("Where from", origin.clone());
        if attributes.where_from.len() > 1 {
            fact = fact.copying(attributes.where_from.join(
                "
",
            ));
        }
        rows.push(fact);
    }
    if let Some(quarantine) = &attributes.quarantine {
        let when = quarantine
            .unix_seconds
            .and_then(|seconds| i64::try_from(seconds).ok())
            .map(|seconds| date_seconds(Timestamp::from_unix(seconds, 0), false));
        let text = match (when, quarantine.agent.is_empty()) {
            (Some(when), false) => format!("{when}, by {}", quarantine.agent),
            (Some(when), true) => when,
            (None, false) => format!("by {}", quarantine.agent),
            (None, true) => "Downloaded".to_owned(),
        };
        rows.push(Fact::new("Downloaded", text));
    }
    if !attributes.tags.is_empty() {
        let text = attributes
            .tags
            .iter()
            .map(|tag| match tag_colour(tag.colour) {
                Some(colour) => format!("{} ({colour})", tag.name),
                None => tag.name.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        rows.push(Fact::new("Tags", text));
    }
    if let Some(comment) = &attributes.comment {
        rows.push(Fact::new("Comment", comment.clone()));
    }
    rows
}

/// The Extended attributes section: the Mac attributes in words first, then every attribute with
/// the size of its value.
pub fn xattrs_with_mac(
    mac: &MacAttributes,
    xattrs: &Result<Vec<XattrInfo>, SharedString>,
) -> SectionState {
    let mut rows = mac_attribute_facts(mac);
    match xattrs_item(xattrs) {
        SectionState::Ready(list) => rows.extend(list),
        SectionState::Failed(text) if rows.is_empty() => return SectionState::Failed(text),
        _ => {}
    }
    ready_or_absent(rows)
}

/// What is inside a folder, a package, a volume or several items: the storage bar's legend as
/// rows, then the five largest items. `lower_bound` adds a note. `Absent` when there is nothing.
pub fn inside_section(
    classes: ClassBytes,
    largest: &[LargestItem],
    lower_bound: bool,
) -> SectionState {
    if classes.total() == 0 && largest.is_empty() {
        return SectionState::Absent;
    }
    // Every kind is listed: a bar this wide folds nothing into Other.
    let mut rows: Vec<Fact> = legend(&classes, classes.total(), f32::MAX)
        .into_iter()
        .map(|(piece, bytes)| {
            Fact::new(piece_name(piece), format::size(bytes)).copying(bytes.to_string())
        })
        .collect();
    for item in largest.iter().take(LISTED_LARGEST) {
        let fact =
            Fact::new(item.name.clone(), format::size(item.bytes)).copying(item.bytes.to_string());
        rows.push(match &item.link {
            Some(link) => fact.linking(link.clone()),
            None => fact,
        });
    }
    if lower_bound {
        rows.push(Fact::note("Some sizes are a lower bound."));
    }
    SectionState::Ready(rows)
}

/// A volume's sections: General (format, capacity, used, free, files and folders), Volume (name,
/// identity, journal) and, for a volume that lies in a partition, On the disk.
///
/// `_classes` is not read here: the window draws the storage bar from it and lists it through
/// [`inside_section`].
pub fn volume_sections(
    info: &VolumeInfo,
    identity: Option<&VolumeIdentity>,
    totals: Option<Totals>,
    _classes: Option<ClassBytes>,
    partition: Option<PartitionFacts>,
) -> Vec<(SectionId, SectionState)> {
    let mut general = Vec::new();
    if let Some(name) = info.format {
        general.push(Fact::new("Format", name));
    }
    if let Some(total) = info.total_bytes {
        general.push(size_fact("Capacity", total));
        if let Some(free) = info.free_bytes {
            general.push(size_fact("Used", total.saturating_sub(free)));
        }
    }
    if let Some(free) = info.free_bytes {
        general.push(size_fact("Free", free));
    }
    if let Some(totals) = totals {
        general.push(Fact::new("Files and folders", contains_text(totals)));
    }

    let mut volume = vec![Fact::new("Name", info.name.clone())];
    if let Some(identity) = identity {
        let hex: String = identity
            .id
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        volume.push(Fact::new("Identity", hex));
    }
    if let Some(text) = info.journal.as_ref().and_then(journal_text) {
        volume.push(Fact::new("Journal", text));
    }

    let mut sections = vec![
        (SectionId::General, ready_or_absent(general)),
        (SectionId::Volume, SectionState::Ready(volume)),
    ];
    if let Some(partition) = partition {
        let mut rows = Vec::new();
        if let Some(number) = partition.number {
            rows.push(Fact::new("Partition number", number.to_string()));
        }
        rows.push(
            Fact::new(
                "Start",
                format!(
                    "{} (0x{:X})",
                    format::grouped(partition.start),
                    partition.start
                ),
            )
            .copying(partition.start.to_string()),
        );
        rows.push(size_fact("Length", partition.len));
        if let Some((name, disk)) = partition.disk {
            rows.push(Fact::new("Disk", name).linking(FactLink::Disk(disk)));
        }
        sections.push((SectionId::OnDisk, SectionState::Ready(rows)));
    }
    sections
}

/// A disk's section: its model and size, then one row per partition.
pub fn disk_sections(disk: &DiskDescription) -> Vec<(SectionId, SectionState)> {
    let mut rows = vec![Fact::new("Model", disk.model.clone())];
    if let Some(connection) = &disk.connection {
        rows.push(Fact::new("Connection", connection.clone()));
    }
    rows.push(size_fact("Size", disk.size));
    if let Some(sector) = disk.sector_size {
        rows.push(Fact::new(
            "Sector size",
            format!("{} bytes", format::grouped(u64::from(sector))),
        ));
    }
    if let Some(scheme) = disk.scheme {
        rows.push(Fact::new("Partition map", scheme));
    }
    rows.push(Fact::new("Device path", disk.device_path.clone()));
    rows.push(Fact::new("Read-only", "Always"));
    for (name, fs_name, start, len, readable) in &disk.partitions {
        let status = match readable {
            Ok(()) => "readable".to_owned(),
            Err(reason) => format!("cannot be read: {reason}"),
        };
        rows.push(Fact::new(
            name.clone(),
            format!(
                "{} \u{b7} starts at {} \u{b7} {} \u{b7} {status}",
                fs_name
                    .as_ref()
                    .map_or("Unknown format", |name| name.as_ref()),
                format::grouped(*start),
                format::size(*len),
            ),
        ));
    }
    vec![(SectionId::Disk, SectionState::Ready(rows))]
}

/// General, for a selection of several items: how many, how big, and the folder they share.
pub fn several_items(
    count: usize,
    total: Option<u64>,
    shared_folder: Option<SharedString>,
) -> SectionState {
    let mut rows = vec![Fact::new("Items", format::items(count))];
    if let Some(bytes) = total {
        rows.push(size_fact("Total size", bytes));
    }
    if let Some(folder) = shared_folder {
        rows.push(Fact::new("Where", folder));
    }
    SectionState::Ready(rows)
}

/// Facts as `label<TAB>value` lines (the `copy` text when a row has one).
fn rows_text(rows: &[Fact]) -> String {
    rows.iter()
        .map(|fact| {
            let value = fact.copy.as_ref().unwrap_or(&fact.value);
            if fact.label.is_empty() {
                value.to_string()
            } else {
                format!("{}\t{value}", fact.label)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The window as text, for "Copy everything": the title, then each section that is not `Absent`
/// with its rows as `label<TAB>value` (the `copy` text when a row has one).
pub fn copy_everything(title: &str, sections: &[(SectionId, SectionState)]) -> String {
    let mut blocks = vec![title.to_owned()];
    for (id, state) in sections {
        let body = match state {
            SectionState::Absent => continue,
            SectionState::Loading => "Loading...".to_owned(),
            SectionState::Failed(text) => text.to_string(),
            SectionState::Ready(rows) => rows_text(rows),
            // Only a calculated digest is text; "not calculated" is left out.
            SectionState::Checksum(checksum) => match checksum.digest() {
                Some(digest) => rows_text(&[Fact::new(checksum.label(), digest.clone())]),
                None => continue,
            },
        };
        blocks.push(format!("{}\n{body}", id.title()));
    }
    blocks.join("\n\n")
}

#[cfg(test)]
mod tests {
    #[test]
    fn mac_attributes_read_in_words() {
        use disk_atlas_lib::mac::xattr::{Quarantine, Tag};
        let attributes = MacAttributes {
            quarantine: Some(Quarantine {
                flags: 0x83,
                unix_seconds: Some(86_400),
                agent: "Safari".into(),
                uuid: None,
            }),
            where_from: vec!["https://a.example/f.zip".into()],
            tags: vec![
                Tag {
                    name: "Work".into(),
                    colour: 6,
                },
                Tag {
                    name: "Plain".into(),
                    colour: 0,
                },
            ],
            comment: None,
        };
        let rows = mac_attribute_facts(&attributes);
        let text: Vec<(&str, &str)> = rows
            .iter()
            .map(|f| (f.label.as_ref(), f.value.as_ref()))
            .collect();
        assert_eq!(text[0], ("Where from", "https://a.example/f.zip"));
        assert_eq!(text[1].0, "Downloaded");
        assert!(text[1].1.ends_with(", by Safari"));
        assert_eq!(text[2], ("Tags", "Work (Red), Plain"));
        assert!(mac_attribute_facts(&MacAttributes::default()).is_empty());
    }

    use super::*;

    #[test]
    fn a_new_fact_copies_its_value_and_is_not_running() {
        let fact = Fact::new("Size", "18 bytes");
        assert_eq!(fact.label, "Size");
        assert_eq!(fact.value, "18 bytes");
        assert_eq!(fact.copy, None);
        assert!(!fact.running);
    }

    #[test]
    fn subtitle_for_a_file_with_a_size() {
        assert_eq!(
            subtitle("a.png", NodeKind::File, None, Some(1_887_437), false, false),
            "PNG image, 1.8 MB"
        );
        assert_eq!(
            subtitle("README", NodeKind::File, None, Some(0), false, false),
            "Document, 0 bytes"
        );
    }

    #[test]
    fn subtitle_without_a_size_is_the_kind_alone() {
        assert_eq!(
            subtitle("a.png", NodeKind::File, None, None, false, false),
            "PNG image"
        );
        assert_eq!(
            subtitle("Docs", NodeKind::Directory, None, None, true, false),
            "Folder"
        );
        // A folder never shows a size, even if the filesystem reports one.
        assert_eq!(
            subtitle("Docs", NodeKind::Directory, None, Some(96), true, false),
            "Folder"
        );
    }

    #[test]
    fn subtitle_for_links_and_special_files() {
        assert_eq!(
            subtitle("l", NodeKind::Symlink, None, None, false, false),
            "Link"
        );
        assert_eq!(
            subtitle("l", NodeKind::Symlink, None, None, true, false),
            "Link"
        );
        assert_eq!(
            subtitle("dev", NodeKind::Other, None, None, false, false),
            "Special file"
        );
    }

    #[test]
    fn subtitle_says_when_the_content_is_compressed() {
        assert_eq!(
            subtitle(
                "README",
                NodeKind::File,
                None,
                Some(33_233_920),
                false,
                true
            ),
            "Document, 31.7 MB, compressed"
        );
        assert_eq!(
            subtitle("a.png", NodeKind::File, None, None, false, true),
            "PNG image, compressed"
        );
    }

    #[test]
    fn journal_texts() {
        let text = |state| journal_text(&state);
        assert_eq!(text(JournalState::Clean).as_deref(), Some("Clean"));
        let replayed = |transactions, stopped_early| JournalState::Replayed {
            transactions,
            stopped_early,
        };
        assert_eq!(
            text(replayed(1, false)).as_deref(),
            Some("Replayed in memory, 1 transaction")
        );
        assert_eq!(
            text(replayed(2, false)).as_deref(),
            Some("Replayed in memory, 2 transactions")
        );
        assert_eq!(
            text(replayed(1_204, false)).as_deref(),
            Some("Replayed in memory, 1,204 transactions")
        );
        assert_eq!(
            text(replayed(0, false)).as_deref(),
            Some("Replayed in memory, 0 transactions")
        );
        assert_eq!(
            text(replayed(3, true)).as_deref(),
            Some("Replayed in memory up to a damaged entry, 3 transactions")
        );
        assert_eq!(
            text(replayed(1, true)).as_deref(),
            Some("Replayed in memory up to a damaged entry, 1 transaction")
        );
        assert_eq!(
            text(JournalState::NotReplayed {
                reason: "journal header: bad magic".into()
            })
            .as_deref(),
            Some("Not replayed, shown as on disk (journal header: bad magic)")
        );
    }

    #[test]
    fn attribute_texts() {
        assert_eq!(attributes_text(1), "1 extended attribute");
        assert_eq!(attributes_text(3), "3 extended attributes");
        assert_eq!(attributes_text(1_000), "1,000 extended attributes");
    }

    #[test]
    fn selection_text_groups_digits() {
        assert_eq!(selection_text(2), "2 items selected");
        assert_eq!(selection_text(1_204), "1,204 items selected");
    }

    fn extra(created: bool, accessed: bool, link: Option<&str>) -> Extra {
        Extra {
            created: created.then(|| Timestamp::from_unix(1_000, 0)),
            accessed: accessed.then(|| Timestamp::from_unix(2_000, 0)),
            link_target: link.map(str::to_owned),
            resource_size: None,
            finder_codes: None,
            owner: None,
            attributes: None,
            compressed: false,
        }
    }

    fn labels(rows: &[(&'static str, SharedString)]) -> Vec<&'static str> {
        rows.iter().map(|(label, _)| *label).collect()
    }

    #[test]
    fn facts_with_only_what_the_listing_knows() {
        let modified = Timestamp::from_unix(5_000, 0);
        let rows = facts(Some(modified), None);
        assert_eq!(labels(&rows), ["Modified"]);
        assert_eq!(rows[0].1, format::date_time(modified));
        assert!(facts(None, None).is_empty());
    }

    #[test]
    fn facts_are_in_order_and_skip_unknown_values() {
        let modified = Timestamp::from_unix(5_000, 0);
        let all = extra(true, true, Some("../target"));
        let rows = facts(Some(modified), Some(&all));
        assert_eq!(labels(&rows), ["Created", "Modified", "Opened", "Link to"]);
        assert_eq!(rows[3].1, "../target");

        let some = extra(false, true, None);
        assert_eq!(
            labels(&facts(Some(modified), Some(&some))),
            ["Modified", "Opened"]
        );
        assert_eq!(labels(&facts(None, Some(&some))), ["Opened"]);
    }

    #[test]
    fn facts_with_every_row_in_order() {
        let modified = Timestamp::from_unix(5_000, 0);
        let all = Extra {
            resource_size: Some(1_536),
            finder_codes: Some(FinderCodes {
                file_type: *b"PNGf",
                creator: *b"prvw",
            }),
            owner: Some(Owner {
                user: 501,
                group: 20,
                mode: 0o100_644,
            }),
            attributes: Some(1),
            ..extra(true, true, Some("../target"))
        };
        let rows = facts(Some(modified), Some(&all));
        assert_eq!(
            labels(&rows),
            [
                "Created",
                "Modified",
                "Opened",
                "Link to",
                "Resource fork",
                "Type, creator",
                "Owner",
                "Attributes"
            ]
        );
        assert_eq!(rows[4].1, format::size(1_536));
        assert_eq!(rows[5].1, "PNGf, prvw");
        assert_eq!(rows[6].1, "User 501, read and write");
        assert_eq!(rows[7].1, "1 extended attribute");
    }

    #[test]
    fn empty_forks_and_no_attributes_have_no_row() {
        let none = Extra {
            resource_size: Some(0),
            attributes: Some(0),
            ..extra(false, false, None)
        };
        assert!(facts(None, Some(&none)).is_empty());
        let unknown = Extra {
            resource_size: None,
            attributes: None,
            ..none
        };
        assert!(facts(None, Some(&unknown)).is_empty());
        let several = Extra {
            attributes: Some(3),
            ..unknown
        };
        let rows = facts(None, Some(&several));
        assert_eq!(labels(&rows), ["Attributes"]);
        assert_eq!(rows[0].1, "3 extended attributes");
    }

    #[test]
    fn type_codes_name_the_kind_when_the_extension_does_not() {
        let text = Some(FinderCodes {
            file_type: *b"TEXT",
            creator: *b"ttxt",
        });
        assert_eq!(kind_text("ReadMe", NodeKind::File, text), "Plain text");
        // The extension wins when it says something.
        assert_eq!(kind_text("a.png", NodeKind::File, text), "PNG image");
        // No codes, unknown codes and folders keep the plain answers.
        assert_eq!(kind_text("ReadMe", NodeKind::File, None), "Document");
        let odd = Some(FinderCodes {
            file_type: *b"zzzz",
            creator: *b"zzzz",
        });
        assert_eq!(kind_text("ReadMe", NodeKind::File, odd), "Document");
        assert_eq!(kind_text("Docs", NodeKind::Directory, text), "Folder");
        assert_eq!(type_code_kind(*b"ZIP "), Some("ZIP archive"));
        assert_eq!(type_code_kind(*b"SIT!"), Some("StuffIt archive"));
    }

    #[test]
    fn the_subtitle_uses_the_type_code() {
        let codes = Some(FinderCodes {
            file_type: *b"APPL",
            creator: *b"MSWD",
        });
        assert_eq!(
            subtitle("Word", NodeKind::File, codes, Some(2_048), false, false),
            "Application, 2 KB"
        );
    }

    #[test]
    fn mode_is_octal_with_letters() {
        assert_eq!(mode_text(0o100_644), "100644 (-rw-r--r--)");
        assert_eq!(mode_text(0o040_755), "040755 (drwxr-xr-x)");
        assert_eq!(mode_text(0o120_777), "120777 (lrwxrwxrwx)");
        assert_eq!(mode_text(0), "000000 (?---------)");
    }

    #[test]
    fn finder_flags_are_decoded() {
        assert_eq!(finder_flags_text(0), "0x0000: none set");
        assert_eq!(
            finder_flags_text(0x4100),
            "0x4100: has been inited, invisible"
        );
        assert_eq!(finder_flags_text(0x8400), "0x8400: custom icon, alias");
        // Bits 1 to 3 are the colour label, not separate flags.
        assert_eq!(finder_flags_text(0x000C), "0x000c: label red");
        assert_eq!(finder_flags_text(0x000E), "0x000e: label orange");
        let mut info = [0u8; 32];
        info[8] = 0x40;
        assert_eq!(finder_flags(&info), 0x4000);
    }

    #[test]
    fn dates_carry_seconds() {
        let time = Timestamp::from_unix(1_578_821_467, 0);
        assert_eq!(date_seconds_at_offset(time, 0), "12 Jan 2020, 09:31:07");
        assert_eq!(date_seconds_at_offset(time, 3_600), "12 Jan 2020, 10:31:07");
        // A volume without a zone prints the clock reading as it is.
        assert_eq!(date_seconds(time, true), "12 Jan 2020, 09:31:07");
        assert_eq!(date_seconds_at_offset(time, 999_999_999), "Unknown");
    }

    #[test]
    fn sizes_say_the_exact_bytes_from_a_kilobyte_up() {
        assert_eq!(size_with_bytes(18), "18 bytes");
        assert_eq!(size_with_bytes(4_404_019), "4.2 MB (4,404,019 bytes)");
    }

    #[test]
    fn pieces_are_counted_in_words() {
        assert_eq!(pieces_text(0), "On disk: no pieces");
        assert_eq!(pieces_text(1), "On disk: 1 piece");
        assert_eq!(pieces_text(3), "On disk: 3 pieces");
        assert_eq!(pieces_text(1_204), "On disk: 1,204 pieces");
    }

    fn run(offset: u64, len: u64) -> Extent {
        Extent { offset, len }
    }

    #[test]
    fn extents_list_the_first_eight_runs() {
        let runs: Vec<Extent> = (0..11).map(|n| run(n * 8_192, 4_096)).collect();
        let mut rows = Vec::new();
        extent_rows(Stream::Data, &Ok(runs), &mut rows);
        assert_eq!(rows[0], DetailRow::Heading("On disk: 11 pieces".into()));
        assert_eq!(rows[1], DetailRow::Columns("Byte offset", "Length"));
        // Heading, columns, eight runs, the note.
        assert_eq!(rows.len(), 2 + LISTED_EXTENTS + 1);
        assert_eq!(
            rows[2],
            DetailRow::Pair("0 (0x0)".into(), "4 KB (4,096 bytes)".into())
        );
        assert_eq!(
            rows[3],
            DetailRow::Pair("8,192 (0x2000)".into(), "4 KB (4,096 bytes)".into())
        );
        assert_eq!(rows[10], DetailRow::Note("and 3 more".into()));
    }

    #[test]
    fn extents_of_nothing_a_resource_fork_and_a_failure() {
        let mut rows = Vec::new();
        extent_rows(Stream::Data, &Ok(Vec::new()), &mut rows);
        assert_eq!(rows, [DetailRow::Heading("On disk: no pieces".into())]);

        let mut rows = Vec::new();
        extent_rows(Stream::Resource, &Ok(vec![run(512, 512)]), &mut rows);
        assert_eq!(
            rows[0],
            DetailRow::Heading("On disk: 1 piece (resource fork)".into())
        );

        let mut rows = Vec::new();
        extent_rows(
            Stream::Data,
            &Err("This volume does not say.".into()),
            &mut rows,
        );
        assert_eq!(
            rows,
            [
                DetailRow::Heading("On disk".into()),
                DetailRow::Note("This volume does not say.".into())
            ]
        );
    }

    #[test]
    fn attributes_are_a_name_and_size_table() {
        let attribute = |name: &str, size| XattrInfo {
            name: name.to_owned(),
            size,
        };
        let mut rows = Vec::new();
        xattr_rows(
            &Ok(vec![
                attribute("com.apple.quarantine", 57),
                attribute("x", 2_048),
            ]),
            &mut rows,
        );
        assert_eq!(
            rows,
            [
                DetailRow::Heading("Extended attributes (2)".into()),
                DetailRow::Columns("Name", "Size"),
                DetailRow::Pair("com.apple.quarantine".into(), "57 bytes".into()),
                DetailRow::Pair("x".into(), "2 KB".into()),
            ]
        );
        let mut none = Vec::new();
        xattr_rows(&Ok(Vec::new()), &mut none);
        assert_eq!(none[1], DetailRow::Note("None.".into()));
    }

    /// A `Metadata` to fill in: the type is `non_exhaustive`, so the host's answer for a folder
    /// is the way to get one.
    fn metadata() -> disk_atlas_lib::fs::Metadata {
        use disk_atlas_lib::fs::FileSystem;
        use disk_atlas_lib::fs::native::NativeFs;
        let fs = NativeFs::new(std::env::temp_dir(), "Volume").expect("folder opens");
        let mut metadata = fs.metadata(fs.root()).expect("root has metadata");
        metadata.kind = NodeKind::File;
        metadata.size = Some(1_536);
        metadata.resource_size = Some(0);
        metadata.created = None;
        metadata.modified = Some(Timestamp::from_unix(1_578_821_467, 0));
        metadata.accessed = None;
        metadata.flags = EntryFlags::EMPTY;
        metadata.link_target = None;
        metadata.finder_codes = None;
        metadata.owner = None;
        metadata.finder_info = None;
        metadata
    }

    fn details(metadata: disk_atlas_lib::fs::Metadata) -> FileDetails {
        FileDetails {
            metadata,
            stream: Stream::Data,
            xattrs: Ok(Vec::new()),
            extents: Ok(vec![run(4_096, 1_536)]),
            local_times: true,
        }
    }

    fn fact<'a>(rows: &'a [DetailRow], label: &str) -> Option<&'a SharedString> {
        rows.iter().find_map(|row| match row {
            DetailRow::Fact(name, value) if *name == label => Some(value),
            _ => None,
        })
    }

    #[test]
    fn the_details_tab_has_the_technical_rows() {
        let mut m = metadata();
        m.finder_codes = Some(FinderCodes {
            file_type: *b"TEXT",
            creator: *b"ttxt",
        });
        m.owner = Some(Owner {
            user: 501,
            group: 20,
            mode: 0o100_644,
        });
        let mut info = [0u8; 32];
        info[8] = 0x41;
        m.finder_info = Some(info);
        m.flags = EntryFlags::COMPRESSED;
        let node = m.node;
        let rows = tab_rows("ReadMe", &details(m));
        assert_eq!(fact(&rows, "Name").unwrap(), "ReadMe");
        assert_eq!(fact(&rows, "Kind").unwrap(), "Plain text");
        assert_eq!(
            fact(&rows, "Node id").unwrap().as_ref(),
            format::grouped(node.0)
        );
        assert_eq!(fact(&rows, "Data fork").unwrap(), "2 KB (1,536 bytes)");
        assert_eq!(fact(&rows, "Resource fork").unwrap(), "0 bytes");
        assert_eq!(fact(&rows, "Modified").unwrap(), "12 Jan 2020, 09:31:07");
        assert_eq!(fact(&rows, "Mode").unwrap(), "100644 (-rw-r--r--)");
        assert_eq!(fact(&rows, "Owner").unwrap(), "user 501, group 20");
        assert_eq!(fact(&rows, "Type, creator").unwrap(), "TEXT, ttxt");
        assert_eq!(
            fact(&rows, "Finder flags").unwrap(),
            "0x4100: has been inited, invisible"
        );
        assert!(fact(&rows, "Compressed").is_some());
        assert!(fact(&rows, "Hidden").is_none());
        assert!(fact(&rows, "Created").is_none());
        assert!(rows.contains(&DetailRow::Heading("On disk: 1 piece".into())));
    }

    // ----- the Properties window's section loaders -----

    /// The rows of a ready section as (label, value) pairs.
    fn pairs(state: &SectionState) -> Vec<(&str, &str)> {
        match state {
            SectionState::Ready(rows) => rows
                .iter()
                .map(|fact| (fact.label.as_ref(), fact.value.as_ref()))
                .collect(),
            other => panic!("expected a ready section, got {other:?}"),
        }
    }

    fn copy_of(fact: &Fact) -> Option<&str> {
        fact.copy.as_ref().map(|text| text.as_ref())
    }

    fn ready(state: &SectionState) -> &[Fact] {
        match state {
            SectionState::Ready(rows) => rows,
            other => panic!("expected a ready section, got {other:?}"),
        }
    }

    fn volume_info() -> VolumeInfo {
        let mut info = VolumeInfo::new("Macintosh HD");
        info.local_times = true;
        info
    }

    fn item(name: &str, kind: NodeKind) -> ItemFacts {
        ItemFacts {
            name: name.to_owned().into(),
            kind,
            size: None,
            modified: None,
            browsable: kind == NodeKind::Directory,
        }
    }

    #[test]
    fn permission_text_is_letters_and_octal() {
        assert_eq!(permission_text(0o100_644), "rw-r--r-- (644)");
        assert_eq!(permission_text(0o040_755), "rwxr-xr-x (755)");
        assert_eq!(permission_text(0o120_777), "rwxrwxrwx (777)");
        assert_eq!(permission_text(0o100_004), "------r-- (004)");
    }

    #[test]
    fn general_for_a_file_on_a_disk() {
        let mut md = metadata();
        md.created = Some(Timestamp::from_unix(1_000, 0));
        let mut file = item("ReadMe", NodeKind::File);
        file.size = Some(1_536);
        let extents = Ok(vec![run(4_096, 4_096), run(65_536, 4_096)]);
        let link = FactLink::Folder {
            source: SourceId::for_test(1),
            node: NodeId(2),
        };
        let state = general_item(
            &file,
            &md,
            Some(&extents),
            &volume_info(),
            None,
            Some(("Documents".into(), link.clone())),
            false,
        );
        assert_eq!(
            pairs(&state),
            [
                ("Kind", "Document"),
                ("Size", "2 KB (1,536 bytes)"),
                ("On disk", "8 KB"),
                ("Where", "Documents"),
                ("Created", "1 Jan 1970, 00:16:40"),
                ("Modified", "12 Jan 2020, 09:31:07"),
            ]
        );
        let rows = ready(&state);
        // The size copies as bare digits, "On disk" as its byte count, "Where" leads somewhere.
        assert_eq!(copy_of(&rows[1]), Some("1536"));
        assert_eq!(copy_of(&rows[2]), Some("8192"));
        assert_eq!(rows[3].goes_to, Some(link));
        assert!(rows.iter().all(|fact| !fact.running));
    }

    #[test]
    fn a_host_item_has_no_on_disk_row() {
        let md = metadata();
        let mut file = item("a.png", NodeKind::File);
        file.size = Some(100);
        let extents = Ok(vec![run(0, 4_096)]);
        let state = general_item(&file, &md, Some(&extents), &volume_info(), None, None, true);
        assert_eq!(
            pairs(&state),
            [
                ("Kind", "PNG image"),
                ("Size", "100 bytes"),
                ("Modified", "12 Jan 2020, 09:31:07"),
            ]
        );
        // A failed extents read leaves the row out; it is the On the disk section's to report.
        let failed: Result<Vec<Extent>, SharedString> = Err("no".into());
        let state = general_item(&file, &md, Some(&failed), &volume_info(), None, None, false);
        assert!(!pairs(&state).iter().any(|(label, _)| *label == "On disk"));
    }

    #[test]
    fn general_for_a_folder_says_what_it_contains() {
        let md = metadata();
        let folder = item("Docs", NodeKind::Directory);
        let totals = Totals {
            bytes: 19_540_221_184,
            files: 2_140,
            folders: 96,
        };
        let state = general_item(
            &folder,
            &md,
            None,
            &volume_info(),
            Some(totals),
            None,
            false,
        );
        assert_eq!(
            pairs(&state),
            [
                ("Kind", "Folder"),
                ("Size", "18.2 GB (19,540,221,184 bytes)"),
                ("Contains", "2,140 files, 96 folders"),
                ("Modified", "12 Jan 2020, 09:31:07"),
            ]
        );
        // The exact digits, with no grouping.
        assert_eq!(copy_of(&ready(&state)[1]), Some("19540221184"));

        let one = Totals {
            bytes: 5,
            files: 1,
            folders: 1,
        };
        let state = general_item(&folder, &md, None, &volume_info(), Some(one), None, false);
        assert_eq!(pairs(&state)[2], ("Contains", "1 file, 1 folder"));

        // Without totals: no size and no contents.
        let state = general_item(&folder, &md, None, &volume_info(), None, None, false);
        assert_eq!(
            pairs(&state),
            [("Kind", "Folder"), ("Modified", "12 Jan 2020, 09:31:07")]
        );
    }

    #[test]
    fn mac_information_of_a_classic_file() {
        let mut md = metadata();
        md.finder_codes = Some(FinderCodes {
            file_type: *b"TEXT",
            creator: *b"ttxt",
        });
        md.resource_size = Some(3_000);
        md.flags = EntryFlags::EMPTY.with_label(4);
        let mut info = [0u8; 32];
        info[8] = 0x44;
        md.finder_info = Some(info);
        let state = mac_item(&md, None);
        assert_eq!(
            pairs(&state),
            [
                ("Type and creator", "TEXT \u{b7} ttxt"),
                ("Label", "Blue"),
                ("Flags in words", "Invisible, Custom icon"),
                ("Resource fork", "3 KB"),
            ]
        );
        assert_eq!(copy_of(&ready(&state)[3]), Some("3000"));
        let state = mac_item(&md, Some("ReadMe"));
        assert_eq!(pairs(&state)[3], ("Resource fork", "3 KB (from ._ReadMe)"));
    }

    #[test]
    fn the_label_falls_back_to_the_finder_flags_word() {
        let mut md = metadata();
        // 0x000C: label 6, red.
        let mut info = [0u8; 32];
        info[9] = 0x0C;
        md.finder_info = Some(info);
        assert_eq!(pairs(&mac_item(&md, None)), [("Label", "Red")]);
    }

    #[test]
    fn the_package_flag_counts_for_a_folder_only() {
        let mut info = [0u8; 32];
        info[8] = 0x20;
        let mut md = metadata();
        md.finder_info = Some(info);
        assert_eq!(mac_item(&md, None), SectionState::Absent);
        md.kind = NodeKind::Directory;
        assert_eq!(pairs(&mac_item(&md, None)), [("Flags in words", "Package")]);
        // The listing's own flags say the same.
        let mut md = metadata();
        md.flags = EntryFlags::ALIAS;
        assert_eq!(pairs(&mac_item(&md, None)), [("Flags in words", "Alias")]);
    }

    #[test]
    fn a_plain_host_file_has_no_mac_information() {
        assert_eq!(mac_item(&metadata(), None), SectionState::Absent);
        // A resource fork of 0 bytes is no row.
        let mut md = metadata();
        md.resource_size = Some(0);
        assert_eq!(mac_item(&md, Some("x")), SectionState::Absent);
    }

    #[test]
    fn permissions_of_a_mac_disk_and_of_windows() {
        let mut md = metadata();
        md.owner = Some(Owner {
            user: 501,
            group: 20,
            mode: 0o100_644,
        });
        assert_eq!(
            pairs(&permissions_item(&md, None)),
            [
                ("Owner and group", "user 501, group 20"),
                ("Mode", "rw-r--r-- (644)"),
                ("Names", "Names belong to the Mac that wrote the disk."),
            ]
        );
        md.owner = None;
        let flags = HostFlags {
            read_only: true,
            hidden: false,
            system: false,
        };
        assert_eq!(
            pairs(&permissions_item(&md, Some(flags))),
            [("Read-only", "Yes"), ("Hidden", "No"), ("System", "No")]
        );
        assert_eq!(permissions_item(&md, None), SectionState::Absent);
    }

    #[test]
    fn extended_attributes_are_a_name_and_a_size() {
        let attribute = |name: &str, size| XattrInfo {
            name: name.to_owned(),
            size,
        };
        let state = xattrs_item(&Ok(vec![
            attribute("com.apple.quarantine", 57),
            attribute("x", 2_048),
        ]));
        assert_eq!(
            pairs(&state),
            [("com.apple.quarantine", "57 bytes"), ("x", "2 KB")]
        );
        assert_eq!(xattrs_item(&Ok(Vec::new())), SectionState::Absent);
        assert_eq!(
            xattrs_item(&Err("attributes node 12".into())),
            SectionState::Failed("attributes node 12".into())
        );
    }

    #[test]
    fn on_the_disk_with_three_pieces() {
        let mut md = metadata();
        md.flags = EntryFlags::COMPRESSED;
        let node = format::grouped(md.node.0);
        let extents = Ok(vec![run(4_096, 1_536), run(8_192, 4_096), run(65_536, 512)]);
        let (state, strip) = on_disk_item(&extents, &md, Some(1_000_000));
        assert_eq!(
            pairs(&state),
            [
                ("Pieces", "3 pieces"),
                ("Piece 1", "4,096 (0x1000), 2 KB (1,536 bytes)"),
                ("Piece 2", "8,192 (0x2000), 4 KB (4,096 bytes)"),
                ("Piece 3", "65,536 (0x10000), 512 bytes"),
                ("Node id", node.as_str()),
                ("Compressed", "Yes"),
            ]
        );
        assert_eq!(strip.expect("a strip").spans.len(), 3);
    }

    #[test]
    fn on_the_disk_lists_eight_pieces_and_counts_the_rest() {
        let md = metadata();
        let runs: Vec<Extent> = (0..12).map(|n| run(n * 8_192, 4_096)).collect();
        let (state, _) = on_disk_item(&Ok(runs), &md, None);
        let rows = ready(&state);
        // Pieces, eight listed, the note, Node id.
        assert_eq!(rows.len(), 1 + LISTED_EXTENTS + 1 + 1);
        assert_eq!(rows[0].value, "12 pieces");
        assert_eq!(rows[8].label, "Piece 8");
        assert_eq!(rows[9], Fact::note("and 4 more"));
        assert_eq!(rows[10].label, "Node id");
        assert!(!rows.iter().any(|fact| fact.label == "Compressed"));
    }

    #[test]
    fn on_the_disk_without_extents_is_the_reason_or_no_pieces() {
        let md = metadata();
        let (state, strip) = on_disk_item(&Err("This volume does not say.".into()), &md, Some(10));
        assert_eq!(
            state,
            SectionState::Failed("This volume does not say.".into())
        );
        assert!(strip.is_none());
        let (state, strip) = on_disk_item(&Ok(Vec::new()), &md, Some(10));
        assert_eq!(pairs(&state)[0], ("Pieces", "no pieces"));
        assert_eq!(strip, Some(PieceStrip { spans: Vec::new() }));
    }

    #[test]
    fn the_piece_strip_merges_touching_pieces() {
        let md = metadata();
        // 0-100 and 100-200 touch; 150-250 overlaps; 500-600 stands apart. Out of order.
        let extents = Ok(vec![
            run(500, 100),
            run(100, 100),
            run(0, 100),
            run(150, 100),
        ]);
        let (_, strip) = on_disk_item(&extents, &md, Some(1_000));
        assert_eq!(strip.unwrap().spans, [(0.0, 0.25), (0.5, 0.6)]);
    }

    #[test]
    fn the_piece_strip_is_clipped_to_the_volume() {
        let md = metadata();
        let extents = Ok(vec![run(900, 400), run(5_000, 10)]);
        let (_, strip) = on_disk_item(&extents, &md, Some(1_000));
        assert_eq!(strip.unwrap().spans, [(0.9, 1.0)]);
        // No length, no strip.
        assert!(on_disk_item(&extents, &md, Some(0)).1.is_none());
        assert!(on_disk_item(&extents, &md, None).1.is_none());
        // A run that would overflow does not panic.
        let wild = Ok(vec![run(u64::MAX - 1, u64::MAX)]);
        let (_, strip) = on_disk_item(&wild, &md, Some(u64::MAX));
        assert_eq!(strip.unwrap().spans.len(), 1);
    }

    fn classes(of: &[(disk_atlas_lib::search::kind::KindClass, u64)], system: u64) -> ClassBytes {
        let mut classes = ClassBytes::default();
        for (class, bytes) in of {
            classes.by_class[usize::from(class.bits())] = *bytes;
        }
        classes.system = system;
        classes
    }

    #[test]
    fn what_is_inside_is_the_legend_and_the_largest_items() {
        use disk_atlas_lib::search::kind::KindClass;
        let link = FactLink::Folder {
            source: SourceId::for_test(1),
            node: NodeId(7),
        };
        let largest: Vec<LargestItem> = (0..6)
            .map(|n| LargestItem {
                name: format!("big{n}").into(),
                bytes: 1 << (30 - n),
                link: (n == 0).then(|| link.clone()),
            })
            .collect();
        let all = classes(
            &[
                (KindClass::Document, 1_024),
                (KindClass::Image, 3 << 20),
                (KindClass::Folder, 10),
            ],
            500,
        );
        let state = inside_section(all, &largest, true);
        assert_eq!(
            pairs(&state),
            [
                ("Pictures", "3.0 MB"),
                ("Documents", "1 KB"),
                ("Other", "10 bytes"),
                ("System", "500 bytes"),
                ("big0", "1.0 GB"),
                ("big1", "512.0 MB"),
                ("big2", "256.0 MB"),
                ("big3", "128.0 MB"),
                ("big4", "64.0 MB"),
                ("", "Some sizes are a lower bound."),
            ]
        );
        assert_eq!(ready(&state)[4].goes_to, Some(link));
        assert_eq!(ready(&state)[5].goes_to, None);
        // No note without the flag.
        let state = inside_section(all, &largest[..1], false);
        assert_eq!(pairs(&state).len(), 4 + 1);
    }

    #[test]
    fn what_is_inside_is_absent_when_nothing_is() {
        assert_eq!(
            inside_section(ClassBytes::default(), &[], true),
            SectionState::Absent
        );
        // Largest items alone are enough to draw the section.
        let one = [LargestItem {
            name: "a".into(),
            bytes: 5,
            link: None,
        }];
        assert_eq!(
            pairs(&inside_section(ClassBytes::default(), &one, false)),
            [("a", "5 bytes")]
        );
    }

    #[test]
    fn a_volume_has_general_volume_and_disk_position_sections() {
        let mut info = VolumeInfo::new("Macintosh HD");
        info.format = Some("HFS+");
        info.total_bytes = Some(1_073_741_824);
        info.free_bytes = Some(268_435_456);
        info.journal = Some(JournalState::Clean);
        let identity = VolumeIdentity {
            id: [0xAB; 16],
            change: [0; 16],
        };
        let totals = Totals {
            bytes: 1,
            files: 2_140,
            folders: 96,
        };
        let sections = volume_sections(&info, Some(&identity), Some(totals), None, None);
        assert_eq!(
            sections.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            [SectionId::General, SectionId::Volume]
        );
        assert_eq!(
            pairs(&sections[0].1),
            [
                ("Format", "HFS+"),
                ("Capacity", "1.0 GB (1,073,741,824 bytes)"),
                ("Used", "768.0 MB (805,306,368 bytes)"),
                ("Free", "256.0 MB (268,435,456 bytes)"),
                ("Files and folders", "2,140 files, 96 folders"),
            ]
        );
        let hex = "ab".repeat(16);
        assert_eq!(
            pairs(&sections[1].1),
            [
                ("Name", "Macintosh HD"),
                ("Identity", hex.as_str()),
                ("Journal", "Clean"),
            ]
        );

        let partition = PartitionFacts {
            number: Some(2),
            start: 1_048_576,
            len: 536_870_912,
            disk: Some(("Samsung SSD".into(), DiskId::for_test(3))),
        };
        let sections = volume_sections(&info, None, None, None, Some(partition));
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[2].0, SectionId::OnDisk);
        assert_eq!(
            pairs(&sections[2].1),
            [
                ("Partition number", "2"),
                ("Start", "1,048,576 (0x100000)"),
                ("Length", "512.0 MB (536,870,912 bytes)"),
                ("Disk", "Samsung SSD"),
            ]
        );
        assert_eq!(
            ready(&sections[2].1)[3].goes_to,
            Some(FactLink::Disk(DiskId::for_test(3)))
        );
        // Without an identity that row is left out.
        assert_eq!(
            pairs(&sections[1].1),
            [("Name", "Macintosh HD"), ("Journal", "Clean")]
        );
    }

    #[test]
    fn a_volume_that_knows_nothing_has_only_its_name() {
        let sections = volume_sections(&VolumeInfo::new("USB"), None, None, None, None);
        assert_eq!(sections[0], (SectionId::General, SectionState::Absent));
        assert_eq!(pairs(&sections[1].1), [("Name", "USB")]);
    }

    #[test]
    fn a_disk_lists_its_partitions_and_why_one_is_unreadable() {
        let disk = DiskDescription {
            model: "Samsung SSD 970".into(),
            connection: Some("USB".into()),
            size: 500_107_862_016,
            sector_size: Some(512),
            scheme: Some(scheme_name(Scheme::Gpt)),
            device_path: "\\\\.\\PhysicalDrive2".into(),
            partitions: vec![
                (
                    "Macintosh HD".into(),
                    Some("HFS+".into()),
                    1_048_576,
                    500_107_862_016,
                    Ok(()),
                ),
                (
                    "Vault".into(),
                    None,
                    2_000_000,
                    4_096,
                    Err("encrypted volume".into()),
                ),
            ],
        };
        let sections = disk_sections(&disk);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].0, SectionId::Disk);
        assert_eq!(
            pairs(&sections[0].1),
            [
                ("Model", "Samsung SSD 970"),
                ("Connection", "USB"),
                ("Size", "465.8 GB (500,107,862,016 bytes)"),
                ("Sector size", "512 bytes"),
                ("Partition map", "GUID partition table"),
                ("Device path", "\\\\.\\PhysicalDrive2"),
                ("Read-only", "Always"),
                (
                    "Macintosh HD",
                    "HFS+ \u{b7} starts at 1,048,576 \u{b7} 465.8 GB \u{b7} readable"
                ),
                (
                    "Vault",
                    "Unknown format \u{b7} starts at 2,000,000 \u{b7} 4 KB \u{b7} cannot be read: encrypted volume"
                ),
            ]
        );
        assert_eq!(scheme_name(Scheme::Mbr), "Master Boot Record");
        assert_eq!(scheme_name(Scheme::Apm), "Apple partition map");
    }

    #[test]
    fn several_items_say_how_many_and_how_big() {
        assert_eq!(
            pairs(&several_items(14, Some(2_048), Some("Photos".into()))),
            [
                ("Items", "14 items"),
                ("Total size", "2 KB (2,048 bytes)"),
                ("Where", "Photos"),
            ]
        );
        assert_eq!(pairs(&several_items(1, None, None)), [("Items", "1 item")]);
    }

    #[test]
    fn copy_everything_is_the_window_as_text() {
        let sections = [
            (
                SectionId::General,
                SectionState::Ready(vec![
                    Fact::new("Kind", "Folder"),
                    Fact::new("Size", "2 KB (2,048 bytes)").copying("2048"),
                ]),
            ),
            (SectionId::Mac, SectionState::Absent),
            (SectionId::Inside, SectionState::Loading),
            (
                SectionId::Xattrs,
                SectionState::Failed("The extended attributes could not be read.".into()),
            ),
            (
                SectionId::OnDisk,
                SectionState::Ready(vec![
                    Fact::new("Pieces", "2 pieces"),
                    Fact::note("and 1 more"),
                ]),
            ),
        ];
        assert_eq!(
            copy_everything("Docs", &sections),
            "Docs\n\n\
             General\nKind\tFolder\nSize\t2048\n\n\
             What is inside\nLoading...\n\n\
             Extended attributes\nThe extended attributes could not be read.\n\n\
             On the disk\nPieces\t2 pieces\nand 1 more"
        );
        assert_eq!(copy_everything("Only", &[]), "Only");
    }

    #[test]
    fn section_titles() {
        assert_eq!(SectionId::Inside.title(), "What is inside");
        assert_eq!(SectionId::Mac.title(), "Mac information");
        assert_eq!(SectionId::OnDisk.title(), "On the disk");
        assert_eq!(SectionId::Xattrs.title(), "Extended attributes");
    }

    #[test]
    fn fact_builders_set_one_thing_each() {
        let link = FactLink::Disk(DiskId::for_test(1));
        let fact = Fact::new("Disk", "Samsung")
            .copying("x")
            .linking(link.clone())
            .running();
        assert_eq!(copy_of(&fact), Some("x"));
        assert_eq!(fact.goes_to, Some(link));
        assert!(fact.running);
        assert_eq!(Fact::new("a", "b").goes_to, None);
    }

    #[test]
    fn pieces_count_text_has_no_prefix() {
        assert_eq!(pieces_count_text(0), "no pieces");
        assert_eq!(pieces_count_text(1), "1 piece");
        assert_eq!(pieces_count_text(1_204), "1,204 pieces");
    }
}
