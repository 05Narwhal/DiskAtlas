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

//! Synthetic indexes and ignored timing tests for the search budgets.
//!
//! [`synthetic`] builds an [`Index`] through [`build`] from a deterministic
//! generator, so the timings below cover the same code a real volume goes
//! through. The ignored tests print the figures the roadmap records against
//! the budgets in `.claude/docs/performance.md`; run them in release:
//!
//! ```text
//! cargo test -p disk-atlas-lib --release --lib search_bench_1m -- --ignored --nocapture
//! cargo test -p disk-atlas-lib --release --lib search_bench_5m -- --ignored --nocapture
//! ```
//!
//! # The generator
//!
//! - **Words.** A fixed list of 2,000 unique pseudo-words built from
//!   consonant-vowel syllables with an optional final consonant, 20 of them
//!   ending in an accented letter so the non-ASCII fold path is exercised.
//!   Words containing a planted term are rejected, so a planted term occurs
//!   only where it is planted. A name picks its words with a skew towards
//!   the front of the list.
//! - **Planted words**, per word slot: `photo` 1.0 %, `final` 0.8 %,
//!   `report` 0.8 %, `img` 3.5 % (the "common" term, about 8 % of names).
//!   Entry number 7 of every 20,000 holds `zeppelin` (the "rare" term) and
//!   entry number 3 of every 400 starts `photo final`, so every case hits
//!   even at a few thousand entries.
//! - **Names** have 1 to 4 words joined by a space, `_`, `-` or camelCase,
//!   in lower, Title or UPPER case; 15 % end in a number. Files get an
//!   extension from the lists in the design (`.claude/docs/design/m4-search.md`
//!   section 4.2) except 7 % that have none; 2 % of folders end in `.app`.
//! - **Tree.** Each folder holds 1 to 200 entries (skewed towards the low
//!   end, mean about 50) and takes its share of the entries left to place.
//!   Depth goes to 12; one folder in ten below the top chains straight down
//!   to reach it. The first folder under the root holds a tenth of the
//!   volume (node 2, the "scope" of the scoped query).
//! - **Arrival order.** Half of the folders are reported after everything
//!   in them, so about half of all entries arrive before their parent.
//! - **Values.** File sizes are log-uniform up to 16 GiB, 3 % of files have a
//!   resource fork, 1 in 8 entries has no modification date, the rest span
//!   1990 to 2026, 3 % are hidden.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::hint::black_box;
use std::ops::ControlFlow;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::fs::{
    DirStream, EntryFlags, FileSystem, Metadata, NodeId, NodeKind, ScanEntry, ScanSummary,
    Timestamp, VolumeIdentity, VolumeInfo,
};
use crate::search::build::build;
use crate::search::index::Index;
use crate::search::persist;
use crate::search::query::{Hits, Query};

/// The node of the folder that holds a tenth of the volume.
const SCOPE_NODE: u64 = 2;
/// Folders at this depth hold only files, so no entry is deeper than 12
/// (the root's children are depth 1).
const MAX_DEPTH: u32 = 11;
/// Words in the list.
const WORDS: usize = 2_000;
/// One entry in this many holds `zeppelin`, at the offset below.
const RARE_EVERY: u64 = 20_000;
const RARE_AT: u64 = 7;
/// One entry in this many starts `photo final`, at the offset below.
const PAIR_EVERY: u64 = 400;
const PAIR_AT: u64 = 3;

const DOCUMENT: &[&str] = &[
    "doc", "docx", "odt", "pages", "pdf", "rtf", "txt", "md", "xls", "xlsx", "numbers", "csv",
    "ppt", "pptx", "key", "epub",
];
const IMAGE: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "tif", "tiff", "webp", "heic", "psd", "pict", "pct",
    "icns", "svg", "raw", "cr2", "nef",
];
const AUDIO: &[&str] = &[
    "mp3", "m4a", "aac", "wav", "aif", "aiff", "flac", "ogg", "mid",
];
const VIDEO: &[&str] = &[
    "mov", "mp4", "m4v", "avi", "mkv", "mpg", "mpeg", "wmv", "webm",
];
const ARCHIVE: &[&str] = &[
    "zip", "sit", "sitx", "hqx", "bin", "gz", "tgz", "bz2", "xz", "7z", "rar", "tar", "cpgz",
];
const CODE: &[&str] = &[
    "c", "h", "cpp", "rs", "py", "js", "ts", "html", "css", "json", "xml", "plist", "sh", "swift",
    "m", "java",
];
const DISK_IMAGE: &[&str] = &[
    "dmg",
    "iso",
    "img",
    "cdr",
    "sparseimage",
    "sparsebundle",
    "toast",
    "dsk",
];
const FONT: &[&str] = &["ttf", "otf", "ttc", "dfont", "suit"];
const APPLICATION: &[&str] = &["exe"];
const OTHER: &[&str] = &["dat", "log", "tmp", "bak", "cache"];

/// Cumulative percent and the extensions of a class of files.
const EXTENSIONS: [(u64, &[&str]); 10] = [
    (28, DOCUMENT),
    (52, IMAGE),
    (60, AUDIO),
    (66, VIDEO),
    (72, ARCHIVE),
    (88, CODE),
    (90, DISK_IMAGE),
    (92, FONT),
    (94, APPLICATION),
    (100, OTHER),
];

const ONSETS: [&str; 20] = [
    "b", "d", "f", "g", "h", "k", "l", "m", "n", "p", "r", "s", "t", "v", "w", "z", "br", "st",
    "tr", "gl",
];
const VOWELS: [&str; 10] = ["a", "e", "i", "o", "u", "a", "e", "o", "ai", "ou"];
const FINALS: [&str; 8] = ["", "", "", "n", "r", "l", "s", "t"];
/// Terms the queries look for; the syllable words never contain them.
const PLANTED: [&str; 5] = ["photo", "final", "report", "zeppelin", "img"];

const NO_SIZE: u64 = u64::MAX;
const NO_TIME: i64 = i64::MIN;
const FOLDER: u8 = 1;
const HIDDEN: u8 = 2;

/// SplitMix64: small, fast, and the same everywhere.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (`n` of 0 gives 0).
    fn below(&mut self, n: u64) -> u64 {
        ((u128::from(self.next()) * u128::from(n)) >> 64) as u64
    }
}

/// The 2,000 words, the same for every volume.
fn word_list() -> Vec<String> {
    let mut rng = Rng(0xD15C_A71A5);
    let mut seen = HashSet::new();
    let mut words = Vec::with_capacity(WORDS);
    while words.len() < WORDS {
        let mut word = String::new();
        let syllables = match rng.below(10) {
            0..=1 => 1,
            2..=6 => 2,
            _ => 3,
        };
        for _ in 0..syllables {
            word.push_str(ONSETS[rng.below(ONSETS.len() as u64) as usize]);
            word.push_str(VOWELS[rng.below(VOWELS.len() as u64) as usize]);
            word.push_str(FINALS[rng.below(FINALS.len() as u64) as usize]);
        }
        if word.len() < 3 || PLANTED.iter().any(|p| word.contains(p)) {
            continue;
        }
        if words.len() % 100 == 0 {
            word.push('\u{e9}');
        }
        if seen.insert(word.clone()) {
            words.push(word);
        }
    }
    words
}

/// One entry in arrival order; its name is `names[previous.end..end]`.
#[derive(Clone, Copy)]
struct Rec {
    end: u32,
    node: u32,
    parent: u32,
    flags: u8,
    size: u64,
    rsize: u64,
    modified: i64,
}

/// A folder reported after everything in it: its record and name wait on
/// its frame until the frame closes.
struct Pending {
    rec: Rec,
    name: String,
}

/// A folder being filled, on the generator's explicit stack.
struct Frame {
    node: u32,
    depth: u32,
    children_left: u64,
    folders_left: u64,
    /// Entries still to hand to the folders this one has not yet created.
    rest: u64,
    /// The share of the first sub-folder, when it is fixed.
    fixed_first: Option<u64>,
    late: Option<Pending>,
}

/// A `FileSystem` double that reports a generated volume from `scan`.
struct SynthFs {
    names: String,
    recs: Vec<Rec>,
    /// What `identity()` reports; `None` for the benchmarks.
    identity: Option<VolumeIdentity>,
}

impl SynthFs {
    /// Entries that arrive before their parent folder.
    fn early_children(&self) -> usize {
        let mut position = vec![u32::MAX; self.recs.len() + 2];
        for (i, r) in self.recs.iter().enumerate() {
            position[r.node as usize] = i as u32;
        }
        self.recs
            .iter()
            .enumerate()
            .filter(|(i, r)| r.parent > 1 && position[r.parent as usize] > *i as u32)
            .count()
    }
}

struct Generator {
    rng: Rng,
    words: Vec<String>,
    names: String,
    recs: Vec<Rec>,
    scratch: String,
    created: u64,
}

impl Generator {
    fn frame(
        &mut self,
        node: u32,
        depth: u32,
        budget: u64,
        fixed_first: Option<u64>,
        late: Option<Pending>,
    ) -> Frame {
        let cap = 1 + self.rng.below(200);
        let fan = 1 + self.rng.below(cap);
        let children = fan.min(budget);
        let rest = budget - children;
        let mut folders = 0;
        if rest > 0 && depth < MAX_DEPTH {
            folders = rest.div_ceil(64);
            if depth >= 1 && self.rng.below(10) == 0 {
                folders = 1;
            }
            folders = folders.min(children - children / 4).min(rest).max(1);
        }
        Frame {
            node,
            depth,
            children_left: children,
            folders_left: folders,
            rest,
            fixed_first: fixed_first.filter(|_| folders > 0),
            late,
        }
    }

    fn pick_word(&mut self) -> &str {
        let slot = self.rng.below(1000);
        match slot {
            0..=9 => "photo",
            10..=17 => "final",
            18..=25 => "report",
            26..=60 => "img",
            _ => {
                let len = self.words.len() as u64;
                let i = self.rng.below(len) * self.rng.below(len) / len;
                &self.words[i as usize]
            }
        }
    }

    /// Writes the name of entry number `ordinal` (in creation order) to
    /// `self.scratch`.
    fn make_name(&mut self, folder: bool, ordinal: u64) {
        let mut out = std::mem::take(&mut self.scratch);
        out.clear();
        let count = 1 + self.rng.below(4) as usize;
        let sep = self.rng.below(4);
        let style = if sep == 3 { 1 } else { self.rng.below(20) };
        for slot in 0..count {
            let word: String = if slot == 0 && ordinal % RARE_EVERY == RARE_AT {
                "zeppelin".to_owned()
            } else if ordinal % PAIR_EVERY == PAIR_AT && slot < 2 {
                ["photo", "final"][slot].to_owned()
            } else {
                self.pick_word().to_owned()
            };
            if slot > 0 {
                match sep {
                    0 => out.push(' '),
                    1 => out.push('_'),
                    2 => out.push('-'),
                    _ => {}
                }
            }
            match style {
                0..=8 => out.push_str(&word),
                16..=19 => out.push_str(&word.to_ascii_uppercase()),
                _ => {
                    let (head, tail) = word.split_at(1);
                    out.push_str(&head.to_ascii_uppercase());
                    out.push_str(tail);
                }
            }
        }
        if self.rng.below(100) < 15 {
            let digits = 1 + self.rng.below(9999);
            let _ = write!(out, "{}{digits}", if sep == 1 { "_" } else { " " });
        }
        if folder {
            if self.rng.below(50) == 0 {
                out.push_str(".app");
            }
        } else if self.rng.below(100) >= 7 {
            let pick = self.rng.below(100);
            let list = EXTENSIONS
                .iter()
                .find(|(upto, _)| pick < *upto)
                .map_or(OTHER, |(_, list)| *list);
            let ext = list[self.rng.below(list.len() as u64) as usize];
            out.push('.');
            if self.rng.below(10) == 0 {
                out.push_str(&ext.to_ascii_uppercase());
            } else {
                out.push_str(ext);
            }
        }
        self.scratch = out;
    }

    fn record(&mut self, folder: bool, ordinal: u64, parent: u32) -> Rec {
        let special = ordinal % RARE_EVERY == RARE_AT || ordinal % PAIR_EVERY == PAIR_AT;
        let mut flags = if folder { FOLDER } else { 0 };
        if !special && self.rng.below(100) < 3 {
            flags |= HIDDEN;
        }
        let (size, rsize) = if folder {
            (NO_SIZE, NO_SIZE)
        } else {
            let bits = self.rng.below(34);
            let size = (1u64 << bits) + self.rng.below(1u64 << bits);
            let rsize = if self.rng.below(100) < 3 {
                self.rng.below(1 << 20)
            } else {
                NO_SIZE
            };
            (size, rsize)
        };
        let modified = if self.rng.below(8) == 0 {
            NO_TIME
        } else {
            631_152_000 + self.rng.below(1_140_000_000) as i64
        };
        Rec {
            end: 0,
            node: u32::try_from(ordinal + 2).unwrap_or(u32::MAX),
            parent,
            flags,
            size,
            rsize,
            modified,
        }
    }

    fn emit(&mut self, mut rec: Rec, name: &str) {
        self.names.push_str(name);
        rec.end = self.names.len() as u32;
        self.recs.push(rec);
    }
}

/// Generates `n` entries (the root not counted) in arrival order.
fn generate(n: usize, seed: u64) -> SynthFs {
    let mut g = Generator {
        rng: Rng(seed ^ 0x5EA2_C4B3_17E9_0A11),
        words: word_list(),
        names: String::with_capacity(n.saturating_mul(24)),
        recs: Vec::with_capacity(n),
        scratch: String::new(),
        created: 0,
    };
    let total = n as u64;
    let mut fixed = (total >= 20).then(|| total / 10 - 1);
    let mut stack: Vec<Frame> = Vec::new();
    loop {
        let Some(top) = stack.last_mut() else {
            if g.created >= total {
                break;
            }
            // The root, or what is left to place when the depth limit
            // swallowed part of a share.
            let frame = g.frame(1, 0, total - g.created, fixed.take(), None);
            stack.push(frame);
            continue;
        };
        if top.children_left == 0 {
            if let Some(p) = stack.pop().and_then(|f| f.late) {
                g.emit(p.rec, &p.name);
            }
            continue;
        }
        let forced = top.fixed_first.is_some();
        let folder =
            forced || (top.folders_left > 0 && g.rng.below(top.children_left) < top.folders_left);
        top.children_left -= 1;
        let (parent, depth) = (top.node, top.depth);
        let ordinal = g.created;
        g.created += 1;
        g.make_name(folder, ordinal);
        let rec = g.record(folder, ordinal, parent);
        let name = std::mem::take(&mut g.scratch);
        if !folder {
            g.emit(rec, &name);
            g.scratch = name;
            continue;
        }
        let share = {
            let top = stack.last_mut().expect("a frame is open");
            let keep = top.folders_left - 1;
            let share = if let Some(fixed) = top.fixed_first.take() {
                fixed.min(top.rest - keep)
            } else if keep == 0 {
                top.rest
            } else {
                let even = top.rest / top.folders_left;
                (even * (50 + g.rng.below(150)) / 100).clamp(1, top.rest - keep)
            };
            top.rest -= share;
            top.folders_left = keep;
            share
        };
        let node = rec.node;
        let late = (g.rng.below(2) == 0).then(|| Pending {
            rec,
            name: name.clone(),
        });
        if late.is_none() {
            g.emit(rec, &name);
        }
        g.scratch = name;
        let frame = g.frame(node, depth + 1, share, None, late);
        stack.push(frame);
    }
    SynthFs {
        names: g.names,
        recs: g.recs,
        identity: None,
    }
}

impl FileSystem for SynthFs {
    fn info(&self) -> VolumeInfo {
        VolumeInfo::new("Synthetic".to_owned())
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

    fn identity(&self) -> Option<VolumeIdentity> {
        self.identity
    }

    fn scan(&self, visit: &mut dyn FnMut(ScanEntry<'_>) -> ControlFlow<()>) -> Result<ScanSummary> {
        let mut summary = ScanSummary::default();
        let mut start = 0usize;
        for r in &self.recs {
            let end = r.end as usize;
            let name = &self.names[start..end];
            start = end;
            summary.entries += 1;
            let folder = r.flags & FOLDER != 0;
            let entry = ScanEntry {
                node: NodeId(u64::from(r.node)),
                parent: NodeId(u64::from(r.parent)),
                name,
                kind: if folder {
                    NodeKind::Directory
                } else {
                    NodeKind::File
                },
                size: (r.size != NO_SIZE).then_some(r.size),
                resource_size: (r.rsize != NO_SIZE).then_some(r.rsize),
                modified: (r.modified != NO_TIME).then(|| Timestamp::from_unix(r.modified, 0)),
                flags: if r.flags & HIDDEN != 0 {
                    EntryFlags::HIDDEN
                } else {
                    EntryFlags::EMPTY
                },
                finder_type: None,
            };
            if visit(entry).is_break() {
                break;
            }
        }
        Ok(summary)
    }
}

/// A built index with what producing it cost.
struct Synth {
    index: Index,
    generate: Duration,
    build: Duration,
    /// Entries that arrived before their parent.
    early: usize,
}

fn synthesize(n: usize, seed: u64) -> Synth {
    let started = Instant::now();
    let fs = generate(n, seed);
    let generate = started.elapsed();
    let early = fs.early_children();
    let started = Instant::now();
    let index = build(&fs, &AtomicBool::new(false), &mut |_| {})
        .unwrap()
        .unwrap();
    Synth {
        index,
        generate,
        build: started.elapsed(),
        early,
    }
}

/// An index of `n` entries below the root (`n + 1` in all), built through
/// [`build`] from a generated volume. The same `(n, seed)` gives the same
/// index.
pub(crate) fn synthetic(n: usize, seed: u64) -> Index {
    synthesize(n, seed).index
}

fn q(text: &str) -> Query {
    Query {
        text: text.to_owned(),
        ..Query::default()
    }
}

/// The queries timed, each with its label.
fn cases(index: &Index) -> Vec<(String, Query)> {
    let folder = index.find_node(NodeId(SCOPE_NODE)).unwrap();
    let scoped = Query {
        scope: Some(folder),
        ..q("img")
    };
    vec![
        ("rare 8 letters (zeppelin)".to_owned(), q("zeppelin")),
        ("common 3 letters (img)".to_owned(), q("img")),
        ("one letter (a)".to_owned(), q("a")),
        ("two terms (photo final)".to_owned(), q("photo final")),
        (
            format!("scoped img ({} entries)", index.subtree(folder).len()),
            scoped,
        ),
    ]
}

/// The narrowing step: the text typed so far, then the same plus one letter.
fn narrowing() -> (Query, Query) {
    (q("rep"), q("repo"))
}

fn search(index: &Index, query: &Query, previous: Option<(&Query, &Hits)>) -> Hits {
    index
        .search(query, previous, &AtomicBool::new(false))
        .unwrap()
}

/// Median of `runs` timings of `f`, in milliseconds.
fn median_ms(runs: usize, mut f: impl FnMut()) -> f64 {
    let mut times: Vec<f64> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    times.sort_by(f64::total_cmp);
    times[times.len() / 2]
}

fn yes_no(ok: bool) -> &'static str {
    if ok { "yes" } else { "no" }
}

/// Builds `n` entries and prints the figures; `scale` is the label of the
/// size ("1M") and `budget_ms` the first-results budget at that size.
#[allow(clippy::print_stdout)]
fn run(label: &str, n: usize, scale: &str, budget_ms: f64) {
    let synth = synthesize(n, 42);
    let index = &synth.index;
    let entries = index.len() as usize;
    println!(
        "{label}: generated {n} entries in {:.0} ms (not counted as build time), {:.1} % arrived before their parent",
        synth.generate.as_secs_f64() * 1e3,
        synth.early as f64 * 100.0 / n as f64,
    );
    let build_s = synth.build.as_secs_f64();
    let per_million = build_s * 1e6 / entries as f64;
    let name_bytes: usize = (0..index.len()).map(|i| index.name(i).len()).sum();
    let mut depth = vec![0u8; entries];
    let mut deepest = 0u8;
    for i in 1..index.len() {
        let d = depth[index.parent(i).unwrap() as usize] + 1;
        depth[i as usize] = d;
        deepest = deepest.max(d);
    }
    let bytes_per = index.heap_bytes() as f64 / entries as f64;
    println!(
        "{label}: build {build_s:.2} s ({per_million:.2} s per million), {entries} entries, {bytes_per:.1} bytes per entry, average name {:.1} bytes, deepest {deepest}, skipped {}",
        name_bytes as f64 / entries as f64,
        index.summary().skipped,
    );
    let cores = std::thread::available_parallelism().map_or(0, |n| n.get());
    println!("{label}: available_parallelism {cores}");

    let mut medians = Vec::new();
    for (name, query) in cases(index) {
        let mut hits = 0;
        let ms = median_ms(20, || {
            hits = black_box(search(index, &query, None)).len();
        });
        println!(
            "{label}:   {name:<30} {ms:>8.2} ms median of 20, {hits} hits ({:.2} % of entries)",
            hits as f64 * 100.0 / entries as f64
        );
        medians.push(ms);
    }
    let (from, to) = narrowing();
    let before = search(index, &from, None);
    let mut after = 0;
    let ms = median_ms(20, || {
        after = black_box(search(index, &to, Some((&from, &before)))).len();
    });
    let fresh = median_ms(20, || {
        black_box(search(index, &to, None));
    });
    println!(
        "{label}:   {:<30} {ms:>8.2} ms median of 20, {} hits before, {after} after (fresh search {fresh:.2} ms)",
        format!("narrow {} -> {}", from.text, to.text),
        before.len(),
    );

    let (rare, common, one) = (medians[0], medians[1], medians[2]);
    println!(
        "{label}: rare/common under {budget_ms:.0} ms at {scale}: {} (rare {rare:.2} ms, common {common:.2} ms)",
        yes_no(rare.max(common) < budget_ms)
    );
    println!("{label}: one letter: {one:.2} ms (reported, budget applies to rare and common)");
    println!(
        "{label}: build under 5 s per million: {} ({per_million:.2} s); bytes per entry about 80: {bytes_per:.1}",
        yes_no(per_million < 5.0)
    );
}

/// Query and build figures over 1 million entries. Needs about 0.5 GB at
/// peak.
#[test]
#[ignore = "timing; run in release with --ignored --nocapture"]
fn search_bench_1m() {
    run("search_bench_1m", 1_000_000, "1M", 16.0);
}

/// Query and build figures over 5 million entries. Needs about 1 to 2 GB of
/// RAM at peak (the generated volume, the builder's temporary arrays and the
/// index are alive together).
#[test]
#[ignore = "timing; run in release with --ignored --nocapture"]
fn search_bench_5m() {
    run("search_bench_5m", 5_000_000, "5M", 50.0);
}

#[test]
fn word_list_has_unique_words() {
    let words = word_list();
    assert_eq!(words.len(), WORDS);
    let unique: HashSet<&String> = words.iter().collect();
    assert_eq!(unique.len(), WORDS);
    assert!(words.iter().any(|w| !w.is_ascii()));
    assert!(words.iter().all(|w| PLANTED.iter().all(|p| !w.contains(p))));
}

#[test]
fn synthetic_is_deterministic_and_every_case_hits() {
    const N: usize = 2_000;
    let a = synthetic(N, 1);
    let b = synthetic(N, 1);
    assert_eq!(a.len() as usize, N + 1);
    assert_eq!(a.len(), b.len());
    for i in 0..a.len() {
        assert_eq!(a.name(i), b.name(i));
        assert_eq!(a.parent(i), b.parent(i));
        assert_eq!(a.is_folder(i), b.is_folder(i));
        assert_eq!(a.is_hidden(i), b.is_hidden(i));
    }
    let other = synthetic(N, 2);
    assert!((0..a.len().min(other.len())).any(|i| a.name(i) != other.name(i)));

    // The scoped folder holds about a tenth of the volume.
    let folder = a.find_node(NodeId(SCOPE_NODE)).unwrap();
    let scope_len = a.subtree(folder).len();
    assert!((N / 20..=N / 5).contains(&scope_len), "scope {scope_len}");

    for (name, query) in cases(&a) {
        assert!(!search(&a, &query, None).is_empty(), "{name} has no hit");
    }
    let common = search(&a, &q("img"), None).len();
    assert!(
        (N * 3 / 100..=N / 5).contains(&common),
        "img hits {common} of {N}"
    );
    let (from, to) = narrowing();
    let before = search(&a, &from, None);
    let narrowed = search(&a, &to, Some((&from, &before)));
    assert!(!narrowed.is_empty());
    assert!(narrowed.len() <= before.len());
    assert_eq!(narrowed.len(), search(&a, &to, None).len());
}

#[test]
fn about_half_of_the_entries_arrive_before_their_parent() {
    let fs = generate(20_000, 3);
    assert_eq!(fs.recs.len(), 20_000);
    let early = fs.early_children();
    assert!(
        (20_000 / 5..=20_000 * 4 / 5).contains(&early),
        "{early} of 20000 arrive before their parent"
    );
}

/// Process memory in bytes, read without a dependency or `unsafe`.
#[derive(Debug, Clone, Copy)]
struct ProcessMemory {
    /// Resident now (working set on Windows, `VmRSS` on Linux).
    resident: u64,
    /// Largest resident size so far (`PeakWorkingSet64`, `VmHWM`).
    peak: u64,
    /// Committed private memory (`PrivateMemorySize64`, `RssAnon`).
    private: u64,
}

/// Asks PowerShell about this process: std only, slow (a few hundred ms),
/// fine for a measurement.
#[cfg(windows)]
fn process_memory() -> Option<ProcessMemory> {
    let script = format!(
        "$p = Get-Process -Id {}; \"$($p.WorkingSet64) $($p.PeakWorkingSet64) $($p.PrivateMemorySize64)\"",
        std::process::id()
    );
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let mut it = text.split_whitespace().map(|v| v.parse::<u64>().ok());
    Some(ProcessMemory {
        resident: it.next()??,
        peak: it.next()??,
        private: it.next()??,
    })
}

#[cfg(target_os = "linux")]
fn process_memory() -> Option<ProcessMemory> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let field = |key: &str| -> Option<u64> {
        let line = status.lines().find(|l| l.starts_with(key))?;
        let kb: u64 = line[key.len()..]
            .trim()
            .trim_end_matches("kB")
            .trim()
            .parse()
            .ok()?;
        kb.checked_mul(1024)
    };
    Some(ProcessMemory {
        resident: field("VmRSS:")?,
        peak: field("VmHWM:")?,
        private: field("RssAnon:")?,
    })
}

#[cfg(not(any(windows, target_os = "linux")))]
fn process_memory() -> Option<ProcessMemory> {
    None
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

#[allow(clippy::print_stdout)]
fn print_memory(label: &str, m: Option<ProcessMemory>) {
    match m {
        Some(m) => println!(
            "native_scan_memory_: {label:<28} resident {:>8.1} MiB, peak {:>8.1} MiB, private {:>8.1} MiB",
            mib(m.resident),
            mib(m.peak),
            mib(m.private),
        ),
        None => println!("native_scan_memory_: {label:<28} (process memory not available here)"),
    }
}

/// The trees to index: `DISK_ATLAS_BENCH_TREE` as a path list (`;` on
/// Windows, `:` elsewhere), or else the cargo registry and this repository's
/// `target` folder, whichever exist.
fn bench_trees() -> Vec<std::path::PathBuf> {
    if let Some(list) = std::env::var_os("DISK_ATLAS_BENCH_TREE") {
        return std::env::split_paths(&list)
            .filter(|p| p.is_dir())
            .collect();
    }
    let mut trees = Vec::new();
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join(".cargo")));
    if let Some(home) = cargo_home {
        trees.push(home.join("registry"));
    }
    trees.push(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    trees.retain(|p| p.is_dir());
    trees
}

/// What the `NativeFs` intern table costs after indexing a real host tree
/// (roadmap review item 7). Prints process memory before the build, after
/// it, after dropping the indexes (the filesystems, and their tables, are
/// still alive), and after dropping the filesystems too. The table's cost
/// is the private memory still held after the indexes are gone. Budget: at
/// most 100 bytes per entry and 256 MB at two million entries.
///
/// ```text
/// cargo test -p disk-atlas-lib --release native_scan_memory_ -- --ignored --nocapture
/// ```
#[test]
#[ignore = "measures a real host tree; run in release with --ignored --nocapture"]
#[allow(clippy::print_stdout)]
fn native_scan_memory_() {
    use crate::fs::native::NativeFs;

    let trees = bench_trees();
    assert!(
        !trees.is_empty(),
        "no tree to index; set DISK_ATLAS_BENCH_TREE"
    );
    let before = process_memory();
    print_memory("before the build", before);

    let started = Instant::now();
    let mut built = Vec::new();
    for tree in &trees {
        let fs = NativeFs::new(tree, "bench").unwrap();
        let index = build(&fs, &AtomicBool::new(false), &mut |_| {})
            .unwrap()
            .unwrap();
        built.push((fs, index));
    }
    let elapsed = started.elapsed();
    let after_build = process_memory();

    let mut entries = 0u64;
    let mut folders = 0u64;
    let mut skipped = 0u64;
    let mut index_bytes = 0usize;
    for (tree, (_, index)) in trees.iter().zip(&built) {
        let summary = index.summary();
        let tree_folders = (1..index.len()).filter(|&i| index.is_folder(i)).count() as u64;
        println!(
            "native_scan_memory_: {}: {} entries ({tree_folders} folders), {} skipped, index {:.1} MiB",
            tree.display(),
            summary.entries,
            summary.skipped,
            mib(index.heap_bytes() as u64),
        );
        entries += summary.entries;
        folders += tree_folders;
        skipped += summary.skipped;
        index_bytes += index.heap_bytes();
    }
    println!(
        "native_scan_memory_: {entries} entries in all ({folders} folders, {skipped} skipped), built in {:.1} s, index arrays {:.1} MiB",
        elapsed.as_secs_f64(),
        mib(index_bytes as u64),
    );
    print_memory("after the build", after_build);

    let mut filesystems = Vec::new();
    for (fs, index) in built {
        drop(index);
        filesystems.push(fs);
    }
    let after_index = process_memory();
    print_memory("after dropping the index", after_index);
    drop(filesystems);
    let after_fs = process_memory();
    print_memory("after dropping the fs too", after_fs);

    if let (Some(b), Some(a), Some(f)) = (before, after_index, after_fs) {
        let held = a.private.saturating_sub(b.private);
        let per_entry = held as f64 / entries.max(1) as f64;
        println!(
            "native_scan_memory_: held after dropping the index: {:.1} MiB, {per_entry:.1} bytes per entry, about {:.0} MB at two million",
            mib(held),
            per_entry * 2e6 / 1e6,
        );
        println!(
            "native_scan_memory_: freed by dropping the fs: {:.1} MiB",
            mib(a.private.saturating_sub(f.private)),
        );
        println!(
            "native_scan_memory_: budget (at most 100 bytes per entry, 256 MB at 2M) met: {}",
            yes_no(per_entry <= 100.0 && per_entry * 2e6 <= 256e6),
        );
    }
}

/// A saved-index file for `n` generated entries under `identity`.
fn saved_index(n: usize, seed: u64, identity: VolumeIdentity) -> Vec<u8> {
    let mut fs = generate(n, seed);
    fs.identity = Some(identity);
    let index = build(&fs, &AtomicBool::new(false), &mut |_| {})
        .unwrap()
        .unwrap();
    let mut file = Vec::new();
    persist::save(&index, &mut file).unwrap();
    file
}

/// Writes seed files for the `search_index_load` fuzz target into the
/// directory named by `DISK_ATLAS_FUZZ_SEEDS`, in a subfolder of that name.
/// Does nothing when the variable is unset. Run it with
/// `cargo test -p disk-atlas-lib --lib write_fuzz_seeds -- --ignored`.
#[test]
#[ignore = "writes files; run by the fuzz CI job"]
fn write_fuzz_seeds() {
    let Some(root) = std::env::var_os("DISK_ATLAS_FUZZ_SEEDS") else {
        return;
    };
    let dir = std::path::Path::new(&root).join("search_index_load");
    std::fs::create_dir_all(&dir).unwrap();
    let identity = VolumeIdentity {
        id: *b"disk-atlas-seed!",
        change: *b"change-marker-01",
    };
    for (name, n) in [("saved_50.idx", 50), ("saved_1.idx", 0)] {
        let file = saved_index(n, 7, identity);
        let path = dir.join(name);
        std::fs::write(&path, &file).unwrap();
        let written = std::fs::read(&path).unwrap();
        let loaded = persist::load(&mut &written[..], written.len() as u64, identity).unwrap();
        let index = loaded.unwrap_or_else(|| panic!("{name} does not load under its identity"));
        assert_eq!(index.len() as usize, n + 1, "{name}");
    }
}
