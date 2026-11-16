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

//! Conformance checker: opens a disk image, sparse bundle or device
//! read-only, prints what the engine finds, and optionally compares the
//! files it lists with a manifest written by `tools/conformance/manifest.sh`
//! on a Mac.
//!
//! ```text
//! cargo run --release -p disk-atlas-lib --example atlas_check -- \
//!     <path> [--manifest FILE] [--max-entries N] [--read-data] [--index]
//! ```
//!
//! `<path>` is an image file, a sparse bundle folder, or a device path
//! (`\\.\PhysicalDriveN` on Windows, `/dev/...` elsewhere). See
//! `tools/conformance/README.md`.

// A command line tool: printing is its job, errors go to stderr.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use disk_atlas_lib::fs::hfsplus::{self, unicode};
use disk_atlas_lib::fs::probe::{self, Content, Mount, VolumeState};
use disk_atlas_lib::fs::{
    DirBatch, DirProgress, FileSystem, JournalState, NodeId, NodeKind, Stream,
};
use disk_atlas_lib::image;
use disk_atlas_lib::io::block::{BlockSource, Slice};
use disk_atlas_lib::io::cache::{CachedSource, ChunkCache};
use disk_atlas_lib::io::device;
use disk_atlas_lib::partition::{self, TypeHint};
use disk_atlas_lib::search;

/// The most errors and differences printed per list.
const LIST_CAP: usize = 200;
/// Folder nesting beyond which a walk gives up on a branch.
const MAX_DEPTH: usize = 256;
/// Bytes read per call when reading file data.
const READ_STEP: usize = 1 << 20;
/// Cache budget for the source stack.
const CACHE_BYTES: u64 = 256 << 20;

struct Args {
    path: PathBuf,
    manifest: Option<PathBuf>,
    max_entries: usize,
    read_data: bool,
    index: bool,
}

fn usage() -> &'static str {
    "usage: atlas_check <path> [--manifest FILE] [--max-entries N] [--read-data] [--index]\n\
     \n\
     <path>           an image file, a sparse bundle folder, or a device\n\
     \x20                (\\\\.\\PhysicalDriveN on Windows, /dev/... elsewhere)\n\
     --manifest FILE  compare with a manifest from tools/conformance/manifest.sh\n\
     --max-entries N  stop each volume's walk after N entries\n\
     --read-data      read every file's data (timing, checksums for the manifest)\n\
     --index          build each volume's search index and print its size and time"
}

fn parse_args() -> Result<Args, String> {
    let mut path = None;
    let mut manifest = None;
    let mut max_entries = usize::MAX;
    let mut read_data = false;
    let mut index = false;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--manifest") => {
                manifest = Some(PathBuf::from(
                    args.next().ok_or("--manifest needs a file name")?,
                ));
            }
            Some("--max-entries") => {
                let n = args.next().ok_or("--max-entries needs a number")?;
                max_entries = n
                    .to_str()
                    .and_then(|n| n.parse().ok())
                    .ok_or("--max-entries needs a number")?;
            }
            Some("--read-data") => read_data = true,
            Some("--index") => index = true,
            Some("-h" | "--help") => return Err(String::new()),
            Some(flag) if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            _ => {
                if path.replace(PathBuf::from(arg)).is_some() {
                    return Err("only one path may be given".to_string());
                }
            }
        }
    }
    Ok(Args {
        path: path.ok_or("no path given")?,
        manifest,
        max_entries,
        read_data,
        index,
    })
}

// ---- POSIX cksum ------------------------------------------------------------

/// The table for the POSIX `cksum` CRC (polynomial 0x04C11DB7, most
/// significant bit first).
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// A running `cksum`: the CRC and the length, as the command prints them.
#[derive(Clone, Copy, Default)]
struct Cksum {
    crc: u32,
    len: u64,
}

impl Cksum {
    fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.crc = (self.crc << 8) ^ CRC_TABLE[usize::from((self.crc >> 24) as u8 ^ b)];
        }
        self.len = self.len.wrapping_add(bytes.len() as u64);
    }

    /// The CRC after appending the length, least significant byte first
    /// and without leading zero bytes, and complementing.
    fn finish(self) -> (u32, u64) {
        let mut crc = self.crc;
        let mut n = self.len;
        while n != 0 {
            crc = (crc << 8) ^ CRC_TABLE[usize::from((crc >> 24) as u8 ^ (n & 0xFF) as u8)];
            n >>= 8;
        }
        (!crc, self.len)
    }
}

// ---- the manifest -----------------------------------------------------------

/// One manifest line.
#[derive(Debug, Clone, PartialEq)]
struct ManifestEntry {
    kind: char,
    path: String,
    size: Option<u64>,
    rsrc: Option<u64>,
    mode: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    mtime: Option<i64>,
    crc: Option<u32>,
    cklen: Option<u64>,
    target: Option<String>,
}

struct Manifest {
    volume: String,
    entries: Vec<ManifestEntry>,
    bad_lines: Vec<(usize, String)>,
}

/// Decodes `%XX` escapes (the script writes `%25`, `%09`, `%0A`, `%0D`).
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        let hex = |at: usize| {
            bytes
                .get(at)
                .and_then(|c| char::from(*c).to_digit(16))
                .map(|d| d as u8)
        };
        if b == b'%'
            && let (Some(hi), Some(lo)) = (hex(i + 1), hex(i + 2))
        {
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A numeric column, `-` meaning absent. `Err` for anything else.
fn number<T: std::str::FromStr>(text: &str) -> Result<Option<T>, String> {
    if text == "-" {
        return Ok(None);
    }
    text.parse()
        .map(Some)
        .map_err(|_| format!("not a number: {text:?}"))
}

fn parse_manifest_line(line: &str) -> Result<ManifestEntry, String> {
    let cols: Vec<&str> = line.split('\t').collect();
    let [
        kind,
        path,
        size,
        rsrc,
        mode,
        uid,
        gid,
        mtime,
        crc,
        cklen,
        target,
    ] = cols[..]
    else {
        return Err(format!("{} columns, expected 11", cols.len()));
    };
    let kind = match kind {
        "d" | "f" | "l" => kind.chars().next().unwrap_or('f'),
        other => return Err(format!("unknown kind {other:?}")),
    };
    Ok(ManifestEntry {
        kind,
        path: percent_decode(path),
        size: number(size)?,
        rsrc: number(rsrc)?,
        mode: match mode {
            "-" => None,
            m => Some(u32::from_str_radix(m, 8).map_err(|_| format!("bad mode {m:?}"))?),
        },
        uid: number(uid)?,
        gid: number(gid)?,
        mtime: number(mtime)?,
        crc: number(crc)?,
        cklen: number(cklen)?,
        target: (target != "-").then(|| percent_decode(target)),
    })
}

fn read_manifest(path: &Path) -> Result<Manifest, String> {
    let text = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let text = String::from_utf8_lossy(&text);
    let mut lines = text.lines();
    let first = lines.next().unwrap_or_default();
    let volume = first
        .strip_prefix("# disk-atlas manifest v1\t")
        .ok_or("the first line is not a `# disk-atlas manifest v1` header")?;
    let mut manifest = Manifest {
        volume: percent_decode(volume),
        entries: Vec::new(),
        bad_lines: Vec::new(),
    };
    for (i, line) in lines.enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match parse_manifest_line(line) {
            Ok(entry) => manifest.entries.push(entry),
            Err(e) => manifest.bad_lines.push((i + 2, e)),
        }
    }
    Ok(manifest)
}

// ---- walking a volume -------------------------------------------------------

/// What the engine reports for one path.
struct EngineEntry {
    path: String,
    kind: char,
    size: Option<u64>,
    rsrc: u64,
    mode: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    mtime: Option<i64>,
    cksum: Option<(u32, u64)>,
    target: Option<String>,
}

#[derive(Default)]
struct Walk {
    entries: Vec<EngineEntry>,
    errors: Vec<(String, String)>,
    error_count: usize,
    folders: u64,
    files: u64,
    links: u64,
    others: u64,
    data_bytes: u64,
    rsrc_bytes: u64,
    bound_hit: bool,
    depth_hit: bool,
    elapsed: Duration,
    read_time: Duration,
    read_bytes: u64,
}

impl Walk {
    fn error(&mut self, path: &str, what: &str, error: impl std::fmt::Display) {
        self.error_count += 1;
        if self.errors.len() < LIST_CAP {
            self.errors
                .push((path.to_string(), format!("{what}: {error}")));
        }
    }
}

/// Reads all of `source`, returning its checksum.
fn checksum(source: &dyn BlockSource) -> Result<(u32, u64), String> {
    let mut sum = Cksum::default();
    let mut buf = vec![0u8; READ_STEP];
    let mut pos = 0u64;
    let len = source.len();
    while pos < len {
        let n = usize::try_from(len - pos).map_or(READ_STEP, |left| left.min(READ_STEP));
        let chunk = buf.get_mut(..n).ok_or("read buffer")?;
        source.read_at(pos, chunk).map_err(|e| e.to_string())?;
        sum.update(chunk);
        pos += n as u64;
    }
    Ok(sum.finish())
}

fn walk_volume(fs: &dyn FileSystem, max_entries: usize, read_data: bool) -> Walk {
    let started = Instant::now();
    let mut walk = Walk::default();
    let mut stack: Vec<(NodeId, String, usize)> = vec![(fs.root(), String::new(), 0)];
    'folders: while let Some((dir, path, depth)) = stack.pop() {
        let mut stream = match fs.read_dir(dir) {
            Ok(stream) => stream,
            Err(e) => {
                walk.error(&path, "listing", e);
                continue;
            }
        };
        let mut batch = DirBatch::new();
        loop {
            batch.clear();
            let progress = stream.next_batch(&mut batch, 256);
            for entry in batch.iter() {
                if walk.entries.len() >= max_entries {
                    walk.bound_hit = true;
                    break 'folders;
                }
                let child = if path.is_empty() {
                    entry.name.to_string()
                } else {
                    format!("{path}/{}", entry.name)
                };
                let mut item = EngineEntry {
                    path: child.clone(),
                    kind: 'o',
                    size: entry.size,
                    rsrc: entry.resource_size.unwrap_or(0),
                    mode: None,
                    uid: None,
                    gid: None,
                    mtime: entry.modified.map(|t| t.unix_seconds()),
                    cksum: None,
                    target: None,
                };
                match fs.metadata(entry.node) {
                    Ok(meta) => {
                        if let Some(owner) = meta.owner {
                            item.mode = Some(u32::from(owner.mode) & 0o7777);
                            item.uid = Some(owner.user);
                            item.gid = Some(owner.group);
                        }
                        item.mtime = meta.modified.map(|t| t.unix_seconds());
                        item.target = meta.link_target;
                    }
                    Err(e) => walk.error(&child, "metadata", e),
                }
                match entry.kind {
                    NodeKind::Directory => {
                        item.kind = 'd';
                        walk.folders += 1;
                        if depth + 1 >= MAX_DEPTH {
                            walk.depth_hit = true;
                        } else {
                            stack.push((entry.node, child.clone(), depth + 1));
                        }
                    }
                    NodeKind::Symlink => {
                        item.kind = 'l';
                        walk.links += 1;
                    }
                    NodeKind::File => {
                        item.kind = 'f';
                        walk.files += 1;
                        walk.data_bytes += entry.size.unwrap_or(0);
                        walk.rsrc_bytes += item.rsrc;
                    }
                    _ => walk.others += 1,
                }
                if read_data && matches!(entry.kind, NodeKind::File | NodeKind::Symlink) {
                    let read_started = Instant::now();
                    match fs.open(entry.node, Stream::Data) {
                        Ok(source) => match checksum(source.as_ref()) {
                            Ok(sum) => {
                                walk.read_bytes += sum.1;
                                item.cksum = Some(sum);
                            }
                            Err(e) => walk.error(&child, "reading data", e),
                        },
                        Err(e) => walk.error(&child, "opening data", e),
                    }
                    walk.read_time += read_started.elapsed();
                }
                walk.entries.push(item);
            }
            match progress {
                Ok(DirProgress::More) => {}
                Ok(DirProgress::Done) => break,
                Err(e) => {
                    walk.error(&path, "listing", e);
                    break;
                }
            }
        }
    }
    walk.elapsed = started.elapsed();
    walk
}

// ---- comparing with a manifest ----------------------------------------------

/// Names the manifest script skips at the volume root.
const SCRIPT_SKIPS: [&str; 5] = [
    ".Spotlight-V100",
    ".fseventsd",
    ".Trashes",
    ".DocumentRevisions-V100",
    ".TemporaryItems",
];
/// Root entries macOS hides and the engine lists (the journal files and the
/// hot files tree).
const MAC_HIDES: [&str; 3] = [".journal", ".journal_info_block", ".hotfiles.btree"];

/// A path in the form both sides can be compared in: every name decomposed
/// the way HFS+ stores it (macOS lists stored names; the engine composes them
/// for display).
fn key_of(path: &str) -> String {
    let mut out = String::new();
    for (i, name) in path.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        let mut units = Vec::new();
        unicode::decompose(name, &mut units);
        out.push_str(&String::from_utf16_lossy(&units));
    }
    out
}

fn first_component(key: &str) -> &str {
    key.split('/').next().unwrap_or(key)
}

/// True for the root entries the engine hides and macOS shows: the two
/// private folders for hard links, and anything under them.
fn is_private_folder(key: &str) -> bool {
    let first = first_component(key);
    first.ends_with("HFS+ Private Data") || first.starts_with(".HFS+ Private Directory Data")
}

struct Differences {
    /// In the manifest, hidden by the engine on purpose.
    expected_hidden: Vec<String>,
    only_manifest: Vec<String>,
    /// In the engine, left out of the manifest on purpose.
    expected_extra: Vec<String>,
    only_engine: Vec<String>,
    mismatches: BTreeMap<&'static str, Vec<String>>,
    matched: usize,
}

fn compare(manifest: &Manifest, walk: &Walk, read_data: bool) -> Differences {
    let mut diff = Differences {
        expected_hidden: Vec::new(),
        only_manifest: Vec::new(),
        expected_extra: Vec::new(),
        only_engine: Vec::new(),
        mismatches: BTreeMap::new(),
        matched: 0,
    };
    let engine: BTreeMap<String, &EngineEntry> =
        walk.entries.iter().map(|e| (key_of(&e.path), e)).collect();
    let mut seen = std::collections::BTreeSet::new();
    let mut note = |field: &'static str, path: &str, detail: String| {
        diff.mismatches
            .entry(field)
            .or_default()
            .push(format!("{path}: {detail}"));
    };
    let mut only_manifest = Vec::new();
    let mut expected_hidden = Vec::new();
    let mut matched = 0usize;
    for m in &manifest.entries {
        let key = key_of(&m.path);
        let Some(e) = engine.get(&key) else {
            if is_private_folder(&key) || MAC_HIDES.contains(&key.as_str()) {
                expected_hidden.push(m.path.clone());
            } else {
                only_manifest.push(m.path.clone());
            }
            continue;
        };
        seen.insert(key);
        matched += 1;
        if m.kind != e.kind {
            note(
                "kind",
                &m.path,
                format!("manifest {} engine {}", m.kind, e.kind),
            );
            continue;
        }
        if m.kind != 'd' && m.size != e.size {
            note(
                "size",
                &m.path,
                format!("manifest {:?} engine {:?}", m.size, e.size),
            );
        }
        if m.kind == 'f' && m.rsrc.unwrap_or(0) != e.rsrc {
            note(
                "resource fork size",
                &m.path,
                format!("manifest {:?} engine {}", m.rsrc, e.rsrc),
            );
        }
        if m.mode != e.mode {
            let show = |mode: Option<u32>| mode.map_or("none".to_string(), |m| format!("{m:o}"));
            note(
                "mode",
                &m.path,
                format!("manifest {} engine {}", show(m.mode), show(e.mode)),
            );
        }
        if m.uid != e.uid {
            note(
                "uid",
                &m.path,
                format!("manifest {:?} engine {:?}", m.uid, e.uid),
            );
        }
        if m.gid != e.gid {
            note(
                "gid",
                &m.path,
                format!("manifest {:?} engine {:?}", m.gid, e.gid),
            );
        }
        if m.mtime != e.mtime {
            note(
                "mtime",
                &m.path,
                format!("manifest {:?} engine {:?}", m.mtime, e.mtime),
            );
        }
        if m.kind == 'l' && m.target != e.target {
            note(
                "link target",
                &m.path,
                format!("manifest {:?} engine {:?}", m.target, e.target),
            );
        }
        if read_data
            && m.kind == 'f'
            && let (Some(crc), Some(len)) = (m.crc, m.cklen)
            && e.cksum.is_some_and(|sum| sum != (crc, len))
        {
            note(
                "data checksum",
                &m.path,
                format!("manifest {crc} {len} engine {:?}", e.cksum),
            );
        }
    }
    let mut only_engine = Vec::new();
    let mut expected_extra = Vec::new();
    for (key, e) in &engine {
        if seen.contains(key) {
            continue;
        }
        let first = first_component(key);
        if SCRIPT_SKIPS.contains(&first) || MAC_HIDES.contains(&key.as_str()) {
            expected_extra.push(e.path.clone());
        } else {
            only_engine.push(e.path.clone());
        }
    }
    diff.only_manifest = only_manifest;
    diff.expected_hidden = expected_hidden;
    diff.only_engine = only_engine;
    diff.expected_extra = expected_extra;
    diff.matched = matched;
    diff
}

fn print_list(title: &str, items: &[String]) {
    println!("  {title}: {}", items.len());
    for item in items.iter().take(LIST_CAP) {
        println!("    {item:?}");
    }
    if items.len() > LIST_CAP {
        println!("    ... and {} more", items.len() - LIST_CAP);
    }
}

/// Prints the comparison. True when anything differs that is not expected.
fn report_comparison(manifest: &Manifest, walk: &Walk, read_data: bool) -> bool {
    let diff = compare(manifest, walk, read_data);
    println!("  manifest comparison (volume {:?}):", manifest.volume);
    println!(
        "    manifest entries: {}, engine entries: {}, matched by path: {}",
        manifest.entries.len(),
        walk.entries.len(),
        diff.matched
    );
    if !manifest.bad_lines.is_empty() {
        println!(
            "    unreadable manifest lines: {}",
            manifest.bad_lines.len()
        );
        for (line, why) in manifest.bad_lines.iter().take(LIST_CAP) {
            println!("      line {line}: {why}");
        }
    }
    if walk.bound_hit {
        println!("    the walk stopped at --max-entries: entries beyond it are not compared");
    }
    if !read_data {
        println!("    data checksums not compared (run with --read-data)");
    }
    print_list(
        "only in the manifest, expected (the engine hides the private hard link folders and the journal files)",
        &diff.expected_hidden,
    );
    print_list(
        "only in the engine, expected (skipped by manifest.sh, or hidden by macOS)",
        &diff.expected_extra,
    );
    print_list("only in the manifest", &diff.only_manifest);
    print_list("only in Disk Atlas", &diff.only_engine);
    let mut differs = !diff.only_manifest.is_empty() || !diff.only_engine.is_empty();
    if diff.mismatches.is_empty() {
        println!("  mismatches: none");
    }
    for (field, items) in &diff.mismatches {
        differs = true;
        print_list(&format!("mismatches in {field}"), items);
    }
    differs || !manifest.bad_lines.is_empty()
}

// ---- the stack ----------------------------------------------------------------

fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} bytes")
    } else {
        format!("{value:.1} {} ({bytes} bytes)", UNITS[unit])
    }
}

fn is_device_path(path: &Path) -> bool {
    let text = path.to_string_lossy();
    text.starts_with("\\\\.\\") || text.starts_with("//./") || text.starts_with("/dev/")
}

/// A volume found on the disk.
struct Volume {
    label: String,
    source: Arc<dyn BlockSource>,
    content: Content,
}

fn journal_text(state: &JournalState) -> String {
    match state {
        JournalState::Clean => "clean (no journal, or nothing to replay)".to_string(),
        JournalState::Replayed {
            transactions,
            stopped_early,
        } => format!(
            "replayed {transactions} transactions in memory (stopped early: {stopped_early})"
        ),
        JournalState::NotReplayed { reason } => format!("not replayed: {reason}"),
        _ => format!("{state:?}"),
    }
}

fn run(args: &Args) -> Result<bool, String> {
    let manifest = args.manifest.as_deref().map(read_manifest).transpose()?;

    println!("atlas_check {}", args.path.display());
    let started = Instant::now();
    let (format_name, raw): (String, Arc<dyn BlockSource>) = if is_device_path(&args.path) {
        let source = device::open(&args.path).map_err(|e| format!("opening the device: {e}"))?;
        ("device".to_string(), Arc::new(source))
    } else {
        let opened =
            image::open(&args.path).map_err(|e| format!("opening {}: {e}", args.path.display()))?;
        (opened.format.name().to_string(), opened.source)
    };
    let source: Arc<dyn BlockSource> = Arc::new(
        CachedSource::new(raw, Arc::new(ChunkCache::new(CACHE_BYTES)))
            .map_err(|e| format!("reading the disk: {e}"))?,
    );
    println!("image format: {format_name}");
    println!("disk size: {}", size_text(source.len()));
    println!("open: {:.1} ms", started.elapsed().as_secs_f64() * 1000.0);

    let mut volumes = Vec::new();
    match partition::read(source.as_ref()) {
        Ok(Some(map)) => {
            println!(
                "partition map: {:?}, block size {}, {} partitions",
                map.scheme,
                map.block_size,
                map.partitions.len()
            );
            for damage in &map.damage {
                println!("  map damage worked around: {damage}");
            }
            for p in &map.partitions {
                let slice = Slice::new(Arc::clone(&source), p.start, p.len);
                let content = match (&slice, p.hint) {
                    (_, TypeHint::NotData) => None,
                    (Ok(slice), _) => Some(probe::probe(slice).unwrap_or(Content::Unknown)),
                    (Err(_), _) => Some(Content::Unknown),
                };
                println!(
                    "  partition {}: offset {} size {} type {:?} name {:?}{}{}",
                    p.number,
                    p.start,
                    size_text(p.len),
                    p.kind,
                    p.name.as_deref().unwrap_or(""),
                    if p.truncated {
                        " (cut short by the end of the disk)"
                    } else {
                        ""
                    },
                    content.map_or(" content: not data".to_string(), |c| format!(
                        " content: {}",
                        c.format_name()
                    )),
                );
                if let (Ok(slice), Some(content)) = (slice, content) {
                    volumes.push(Volume {
                        label: format!(
                            "partition {} {:?}",
                            p.number,
                            p.name.as_deref().unwrap_or("")
                        ),
                        source: Arc::new(slice),
                        content,
                    });
                }
            }
        }
        Ok(None) => {
            let content = probe::probe(source.as_ref()).unwrap_or(Content::Unknown);
            println!(
                "partition map: none; whole disk content: {}",
                content.format_name()
            );
            volumes.push(Volume {
                label: "whole disk".to_string(),
                source: Arc::clone(&source),
                content,
            });
        }
        Err(e) => println!("partition map: unreadable: {e}"),
    }

    let readable: Vec<&Volume> = volumes
        .iter()
        .filter(|v| {
            matches!(
                v.content,
                Content::HfsPlus | Content::Hfsx | Content::HfsWrapped | Content::Apfs
            )
        })
        .collect();
    let single = readable.len() == 1;
    let mut manifest_used = manifest.is_none();
    let mut problems = false;
    for volume in &readable {
        println!();
        println!("== {} ({})", volume.label, volume.content.format_name());
        let mount_started = Instant::now();
        if volume.content == Content::Apfs {
            let mount = match probe::mount(Arc::clone(&volume.source), Content::Apfs) {
                Ok(Some(Mount::Container(volumes))) => volumes,
                Ok(_) => {
                    println!("mount: FAILED: not mounted as a container");
                    problems = true;
                    continue;
                }
                Err(e) => {
                    println!("mount: FAILED: {e}");
                    problems = true;
                    continue;
                }
            };
            println!(
                "container: ok in {:.1} ms, {} volumes",
                mount_started.elapsed().as_secs_f64() * 1000.0,
                mount.len()
            );
            for (i, cv) in mount.iter().enumerate() {
                println!();
                println!("-- volume {i}: {:?}, role {:?}", cv.name, cv.role);
                match &cv.state {
                    VolumeState::Ready(fs) => {
                        problems |= check_snapshots(fs.as_ref());
                        problems |= check_fs(
                            fs.as_ref(),
                            args,
                            manifest.as_ref(),
                            single && mount.len() == 1,
                            &mut manifest_used,
                        );
                    }
                    VolumeState::Locked(_) => println!("state: locked (encrypted)"),
                    VolumeState::Failed(e) => {
                        println!("state: FAILED: {e}");
                        problems = true;
                    }
                    _ => println!("state: unknown"),
                }
            }
            continue;
        }
        let fs = match hfsplus::mount(Arc::clone(&volume.source)) {
            Ok(fs) => fs,
            Err(e) => {
                println!("mount: FAILED: {e}");
                problems = true;
                continue;
            }
        };
        println!(
            "mount: ok in {:.1} ms",
            mount_started.elapsed().as_secs_f64() * 1000.0
        );
        println!("journal: {}", journal_text(fs.journal()));
        problems |= check_fs(&fs, args, manifest.as_ref(), single, &mut manifest_used);
    }
    for volume in volumes
        .iter()
        .filter(|v| !readable.iter().any(|h| std::ptr::eq(*h, *v)))
    {
        println!();
        println!(
            "== {}: {} (no reader yet, not walked)",
            volume.label,
            volume.content.format_name()
        );
    }
    if readable.is_empty() {
        println!();
        println!("no HFS+ or APFS volume found");
    }
    if !manifest_used {
        println!();
        println!("the manifest's volume was not found among the volumes");
        problems = true;
    }
    println!();
    println!("total time: {:.1} s", started.elapsed().as_secs_f64());
    Ok(problems)
}

/// Lists a volume's snapshots and opens each one, timing both. Returns
/// whether anything failed.
fn check_snapshots(fs: &dyn FileSystem) -> bool {
    let started = Instant::now();
    let snapshots = match fs.snapshots() {
        Ok(list) => list,
        Err(e) => {
            println!("snapshots: FAILED: {e}");
            return true;
        }
    };
    println!(
        "snapshots: {} listed in {:.1} ms",
        snapshots.len(),
        started.elapsed().as_secs_f64() * 1000.0
    );
    let mut problems = false;
    for (i, snap) in snapshots.iter().enumerate() {
        let opened = Instant::now();
        let result = fs.open_snapshot(snap.id).and_then(|s| {
            let mut stream = s.read_dir(s.root())?;
            let mut batch = DirBatch::new();
            stream.next_batch(&mut batch, 256)?;
            Ok(batch.iter().count())
        });
        let ms = opened.elapsed().as_secs_f64() * 1000.0;
        let created = snap.created.map(|t| t.unix_seconds());
        match result {
            // Print the first few in full; a Time Machine disk has hundreds.
            Ok(n) if i < 20 => println!(
                "  {:?} (created {created:?}): opened, {n} root entries, {ms:.1} ms",
                snap.name
            ),
            Ok(_) => {}
            Err(e) => {
                println!("  {:?}: FAILED: {e} ({ms:.1} ms)", snap.name);
                problems = true;
            }
        }
    }
    if !snapshots.is_empty() {
        println!(
            "snapshots opened: {:.1} ms in all",
            started.elapsed().as_secs_f64() * 1000.0
        );
    }
    problems
}

/// Walks one mounted volume, times its scan, optionally builds its index
/// and compares it with the manifest. Returns whether anything failed.
fn check_fs(
    fs: &dyn FileSystem,
    args: &Args,
    manifest: Option<&Manifest>,
    single: bool,
    manifest_used: &mut bool,
) -> bool {
    let mut problems = false;
    let info = fs.info();
    println!("volume name: {:?}", info.name);
    if let Some(format) = &info.format {
        println!("format: {format}");
    }
    if let (Some(total), Some(free)) = (info.total_bytes, info.free_bytes) {
        println!("capacity: {}, free: {}", size_text(total), size_text(free));
    }

    let scan_started = Instant::now();
    match fs.scan(&mut |_| ControlFlow::Continue(())) {
        Ok(summary) => {
            let secs = scan_started.elapsed().as_secs_f64();
            println!(
                "scan: {} entries, {} skipped, {:.3} s ({:.0} entries/s)",
                summary.entries,
                summary.skipped,
                secs,
                summary.entries as f64 / secs.max(1e-9)
            );
            problems |= summary.skipped > 0;
        }
        Err(e) => {
            println!("scan: FAILED: {e}");
            problems = true;
        }
    }

    {
        let walk = walk_volume(fs, args.max_entries, args.read_data);
        println!(
            "walk: {} entries ({} folders, {} files, {} links, {} other); data {}; resource forks {}",
            walk.entries.len(),
            walk.folders,
            walk.files,
            walk.links,
            walk.others,
            size_text(walk.data_bytes),
            size_text(walk.rsrc_bytes),
        );
        let walk_only = walk.elapsed.saturating_sub(walk.read_time).as_secs_f64();
        println!(
            "walk time: {:.3} s ({:.0} entries/s)",
            walk_only,
            walk.entries.len() as f64 / walk_only.max(1e-9)
        );
        if args.read_data {
            let secs = walk.read_time.as_secs_f64();
            println!(
                "data read: {} in {:.3} s ({:.1} MB/s)",
                size_text(walk.read_bytes),
                secs,
                walk.read_bytes as f64 / 1e6 / secs.max(1e-9)
            );
        }
        if walk.bound_hit {
            println!("the walk stopped at --max-entries {}", args.max_entries);
        }
        if walk.depth_hit {
            println!("folders nested deeper than {MAX_DEPTH} were not entered");
        }
        println!("errors: {}", walk.error_count);
        for (path, error) in &walk.errors {
            println!("  {path:?}: {error}");
        }
        if walk.error_count > walk.errors.len() {
            println!("  ... and {} more", walk.error_count - walk.errors.len());
        }
        problems |= walk.error_count > 0;

        if args.index {
            let started = Instant::now();
            let cancel = AtomicBool::new(false);
            match search::build(fs, &cancel, &mut |_| {}) {
                Ok(Some(index)) => {
                    let secs = started.elapsed().as_secs_f64();
                    let summary = index.summary();
                    let entries = u64::from(index.len());
                    println!(
                        "index: {} entries, {} skipped, built in {:.3} s ({:.0} entries/s), {:.1} bytes/entry, {}",
                        summary.entries,
                        summary.skipped,
                        secs,
                        entries as f64 / secs.max(1e-9),
                        index.heap_bytes() as f64 / (entries.max(1)) as f64,
                        size_text(index.heap_bytes() as u64),
                    );
                    problems |= summary.skipped > 0;
                }
                Ok(None) => println!("index: cancelled"),
                Err(error) => {
                    println!("index: failed: {error}");
                    problems = true;
                }
            }
        }

        if let Some(manifest) = manifest {
            let wanted = key_of(&manifest.volume);
            if single || key_of(&info.name) == wanted {
                *manifest_used = true;
                problems |= report_comparison(manifest, &walk, args.read_data);
            } else {
                println!(
                    "  manifest is for volume {:?}, not compared with this one",
                    manifest.volume
                );
            }
        }
    }
    problems
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            if !message.is_empty() {
                eprintln!("atlas_check: {message}");
            }
            eprintln!("{}", usage());
            return ExitCode::from(1);
        }
    };
    match run(&args) {
        Ok(false) => ExitCode::SUCCESS,
        Ok(true) => {
            println!("RESULT: problems found (see above)");
            ExitCode::from(2)
        }
        Err(message) => {
            eprintln!("atlas_check: {message}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cksum_of(bytes: &[u8]) -> (u32, u64) {
        let mut sum = Cksum::default();
        sum.update(bytes);
        sum.finish()
    }

    #[test]
    fn cksum_matches_the_posix_command() {
        // printf 'abc' | cksum
        assert_eq!(cksum_of(b"abc"), (1_219_131_554, 3));
        // An empty input prints 4294967295 0.
        assert_eq!(cksum_of(b""), (4_294_967_295, 0));
        // Fed in pieces, the result is the same.
        let mut sum = Cksum::default();
        sum.update(b"a");
        sum.update(b"bc");
        assert_eq!(sum.finish(), (1_219_131_554, 3));
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%09b%0Ac%0Dd%25e"), "a\tb\nc\rd%e");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
    }

    #[test]
    fn manifest_lines() {
        let line = "f\tDocs/a%09b.txt\t12\t0\t644\t501\t20\t1700000000\t1219131554\t3\t-";
        let e = parse_manifest_line(line).unwrap();
        assert_eq!(e.path, "Docs/a\tb.txt");
        assert_eq!(
            (e.kind, e.size, e.mode, e.crc, e.cklen),
            ('f', Some(12), Some(0o644), Some(1_219_131_554), Some(3))
        );
        let dir = parse_manifest_line("d\tDocs\t-\t-\t755\t501\t20\t1700000000\t-\t-\t-").unwrap();
        assert_eq!((dir.size, dir.rsrc, dir.crc), (None, None, None));
        let link = parse_manifest_line("l\tx\t3\t0\t755\t0\t0\t5\t-\t-\ta%0Ab").unwrap();
        assert_eq!(link.target.as_deref(), Some("a\nb"));
        assert!(parse_manifest_line("f\tonly\tthree").is_err());
        assert!(parse_manifest_line("x\ta\t-\t-\t-\t-\t-\t-\t-\t-\t-").is_err());
    }

    #[test]
    fn keys_compare_decomposed_names() {
        // "é" composed and decomposed give the same key.
        assert_eq!(key_of("caf\u{e9}/x"), key_of("cafe\u{301}/x"));
        assert_ne!(key_of("a"), key_of("b"));
    }

    #[test]
    fn private_folders_are_recognised() {
        assert!(is_private_folder("\0\0\0\0HFS+ Private Data/iNode5"));
        assert!(is_private_folder(".HFS+ Private Directory Data\r"));
        assert!(!is_private_folder("Docs/HFS+ Private Data"));
    }
}
