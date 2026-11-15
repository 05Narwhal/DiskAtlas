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

//! The rules of the Quick Look bytes view, with no GPUI in them: which rows the list holds
//! ([`ByteWindow`]), which pages of the stream are in memory ([`PageCache`]), what is selected
//! ([`Selection`]), and how rows, values and the clipboard text are written. The view
//! (`views/bytes.rs`) draws from these and runs the reads on the background executor.
//! Design: `.claude/docs/design/m5-quicklook-copy.md` §4.1.
//!
//! Offsets are `u64` all the way down: a stream may be far larger than the address space, and
//! every sum that could pass `u64::MAX` saturates or is checked.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use disk_atlas_lib::fs::Timestamp;
use disk_atlas_lib::io::block::BlockSource;

use crate::format;
use crate::preview::structure::{Field, Structure};

/// Bytes on one row of the grid.
pub const BYTES_PER_ROW: u64 = 16;

/// Bytes the list holds at a time: 262,144 rows. A larger stream is shown through a window that
/// moves; a uniform list of the whole of a 2 TB disk would pass what `f32` pixels can address.
pub const WINDOW: u64 = 4 * 1024 * 1024;

/// The window starts on a multiple of this, and moves by it.
pub const HALF: u64 = WINDOW / 2;

/// Rows of the window.
#[cfg(test)]
pub const WINDOW_ROWS: u64 = WINDOW / BYTES_PER_ROW;

/// Scrolling to within this many rows of the "Earlier bytes" or "Later bytes" row moves the
/// window.
pub const EDGE_ROWS: usize = 64;

/// Bytes read from the stream at a time.
pub const PAGE: u64 = 64 * 1024;

/// Most pages kept in memory (2 MiB).
pub const MAX_PAGES: usize = 32;

/// Most bytes Ctrl+C copies.
pub const MAX_COPY: u64 = 1024 * 1024;

/// Most structure fields the side column explains for one selection.
pub const MAX_FIELDS_SHOWN: usize = 8;

/// Characters of a row's hex column: 16 pairs, 15 single spaces and the wider gap after the
/// eighth byte.
pub const HEX_ROW_CHARS: usize = 48;

/// A stream this long or longer shows offsets with 12 hex digits instead of 8.
pub const WIDE_OFFSETS: u64 = 4 * 1024 * 1024 * 1024;

/// Seconds from 1904-01-01 (the classic Mac OS epoch) to 1970-01-01.
const MAC_TO_UNIX: i64 = 2_082_844_800;

/// Unix-nanosecond values from 1980 to 2100 are shown as a date; anything else is likely not
/// one.
const PLAUSIBLE_NANOS: Range<i64> = 315_532_800_000_000_000..4_102_444_800_000_000_000;

// -------------------------------------------------------------------------------------------
// The window
// -------------------------------------------------------------------------------------------

/// What one row of the list is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    /// "Earlier bytes": the window can move back.
    Earlier,
    /// The 16 bytes (fewer on the last row) from this offset.
    Row(u64),
    /// "Later bytes": the window can move on.
    Later,
}

/// Which way the window moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shift {
    Earlier,
    Later,
}

/// The part of the stream the list holds: at most [`WINDOW`] bytes from `start`, which is a
/// multiple of [`HALF`]. The list has a row per 16 bytes, plus an "Earlier bytes" row first and
/// a "Later bytes" row last when there is more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteWindow {
    len: u64,
    start: u64,
}

impl ByteWindow {
    /// The window at the start of a stream of `len` bytes.
    pub fn new(len: u64) -> Self {
        Self { len, start: 0 }
    }

    /// A window holding `target` with at least a quarter window of context around it where the
    /// stream allows, so the target does not sit on the edge that moves the window. A target past
    /// the end means the last byte.
    pub fn around(len: u64, target: u64) -> Self {
        let target = target.min(len.saturating_sub(1));
        let wanted = target.saturating_sub(WINDOW / 4) / HALF * HALF;
        Self {
            len,
            start: wanted.min(Self::last_start(len)),
        }
    }

    /// The latest start a window needs: the first multiple of [`HALF`] whose window reaches the
    /// end. A window from there always holds more than [`HALF`] bytes.
    fn last_start(len: u64) -> u64 {
        if len <= WINDOW {
            0
        } else {
            // (len - WINDOW) rounded up to HALF stays below len: no overflow.
            (len - WINDOW).div_ceil(HALF) * HALF
        }
    }

    /// First byte of the window.
    pub fn start(&self) -> u64 {
        self.start
    }

    /// One past the window's last byte.
    pub fn end(&self) -> u64 {
        self.start.saturating_add(WINDOW).min(self.len)
    }

    /// True when the window does not start at the start of the stream.
    pub fn has_earlier(&self) -> bool {
        self.start > 0
    }

    /// True when the stream goes on past the window.
    pub fn has_later(&self) -> bool {
        self.end() < self.len
    }

    /// True when `offset` is one of the window's bytes.
    pub fn contains(&self, offset: u64) -> bool {
        (self.start..self.end()).contains(&offset)
    }

    /// Rows of bytes in the window (at most [`WINDOW_ROWS`]).
    pub fn data_rows(&self) -> usize {
        // At most WINDOW_ROWS, which fits any usize this app builds for.
        ((self.end() - self.start).div_ceil(BYTES_PER_ROW)) as usize
    }

    /// Rows of the list: the bytes, and the two "more" rows when there is more.
    pub fn list_len(&self) -> usize {
        self.data_rows() + usize::from(self.has_earlier()) + usize::from(self.has_later())
    }

    /// What row `index` of the list is.
    pub fn item(&self, index: usize) -> Option<Item> {
        let earlier = usize::from(self.has_earlier());
        if earlier == 1 && index == 0 {
            return Some(Item::Earlier);
        }
        let row = index - earlier;
        if row < self.data_rows() {
            return Some(Item::Row(self.start + row as u64 * BYTES_PER_ROW));
        }
        (self.has_later() && row == self.data_rows()).then_some(Item::Later)
    }

    /// The list row that shows `offset`, when the window holds it.
    pub fn index_of(&self, offset: u64) -> Option<usize> {
        if !self.contains(offset) {
            return None;
        }
        let row = ((offset - self.start) / BYTES_PER_ROW) as usize;
        Some(row + usize::from(self.has_earlier()))
    }

    /// The offset that row `index` stands for, used to keep the same bytes under the eye when
    /// the window moves: a row's own offset, the row just before the window for "Earlier bytes",
    /// and the row just after it for "Later bytes".
    fn anchor(&self, index: usize) -> Option<u64> {
        match self.item(index)? {
            Item::Row(offset) => Some(offset),
            Item::Earlier => self.start.checked_sub(BYTES_PER_ROW),
            Item::Later => Some(self.end().div_ceil(BYTES_PER_ROW) * BYTES_PER_ROW),
        }
    }

    /// The window moved by half its size, or `None` when there is nothing that way.
    pub fn shifted(&self, shift: Shift) -> Option<Self> {
        let start = match shift {
            Shift::Earlier if self.has_earlier() => self.start - HALF,
            Shift::Later if self.has_later() => self.start + HALF,
            _ => return None,
        };
        Some(Self {
            len: self.len,
            start,
        })
    }

    /// The move the visible rows `top..bottom` (list indices, `bottom` exclusive) call for:
    /// within [`EDGE_ROWS`] of "Earlier bytes" or of "Later bytes".
    pub fn edge(&self, top: usize, bottom: usize) -> Option<Shift> {
        if self.has_earlier() && top <= EDGE_ROWS {
            return Some(Shift::Earlier);
        }
        if self.has_later() {
            let later = self.list_len() - 1;
            if bottom.saturating_add(EDGE_ROWS) > later {
                return Some(Shift::Later);
            }
        }
        None
    }

    /// Moves the window and says where the list row that was at `top` now is, so the caller can
    /// move the scroll offset by the same number of rows in the same update.
    pub fn shift_keeping(&self, shift: Shift, top: usize) -> Option<(Self, usize)> {
        let next = self.shifted(shift)?;
        let anchor = self.anchor(top)?;
        let index = next.index_of(anchor)?;
        Some((next, index))
    }
}

// -------------------------------------------------------------------------------------------
// Pages
// -------------------------------------------------------------------------------------------

/// The page that holds `offset`.
pub fn page_of(offset: u64) -> u64 {
    offset / PAGE
}

/// The byte range of `page` in a stream of `len` bytes; `None` past the end.
pub fn page_range(page: u64, len: u64) -> Option<Range<u64>> {
    let start = page.checked_mul(PAGE)?;
    (start < len).then(|| start..start.saturating_add(PAGE).min(len))
}

/// Reads one page. Blocking: call it on the background executor. The error is worded for the
/// side column.
pub fn read_page(source: &dyn BlockSource, page: u64) -> Result<Arc<[u8]>, String> {
    let range =
        page_range(page, source.len()).ok_or_else(|| "past the end of the stream".to_owned())?;
    // At most PAGE bytes.
    let mut buf = vec![0u8; (range.end - range.start) as usize];
    source
        .read_at(range.start, &mut buf)
        .map_err(|error| error.to_string())?;
    Ok(buf.into())
}

/// A page in memory: its bytes, or why they could not be read.
#[derive(Debug, Clone)]
pub enum Page {
    Ready(Arc<[u8]>),
    Failed(Arc<str>),
}

#[derive(Debug)]
struct Slot {
    page: Page,
    /// When it was last used, for eviction.
    used: u64,
}

/// The pages of the stream in memory: at most [`MAX_PAGES`], the least recently used evicted.
/// Each request is made once ([`PageCache::want`]) and its answer accepted only for the
/// generation it was made under ([`PageCache::arrive`]).
#[derive(Debug, Default)]
pub struct PageCache {
    generation: u64,
    pages: HashMap<u64, Slot>,
    pending: HashSet<u64>,
    clock: u64,
    /// The page that failed last, for the side column.
    last_failed: Option<u64>,
}

impl PageCache {
    /// Forgets every page and request: a new file or fork. Returns the new generation, which
    /// every request made from now on carries.
    pub fn reset(&mut self) -> u64 {
        self.generation += 1;
        self.pages.clear();
        self.pending.clear();
        self.last_failed = None;
        self.generation
    }

    /// The generation requests are made under.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// The page, which becomes the most recently used.
    pub fn get(&mut self, page: u64) -> Option<&Page> {
        let now = self.tick();
        let slot = self.pages.get_mut(&page)?;
        slot.used = now;
        Some(&slot.page)
    }

    /// True when the caller should fetch `page`: it is neither in memory nor already asked for.
    /// The request is then counted as made.
    pub fn want(&mut self, page: u64) -> bool {
        if self.pages.contains_key(&page) || self.pending.contains(&page) {
            return false;
        }
        self.pending.insert(page);
        true
    }

    /// A read came back. Kept only when it was made under the current generation; returns
    /// whether it was. The least recently used pages go when the cache is full.
    pub fn arrive(
        &mut self,
        generation: u64,
        page: u64,
        result: Result<Arc<[u8]>, String>,
    ) -> bool {
        if generation != self.generation || !self.pending.remove(&page) {
            return false;
        }
        let page_state = match result {
            Ok(bytes) => Page::Ready(bytes),
            Err(error) => {
                self.last_failed = Some(page);
                Page::Failed(error.into())
            }
        };
        let used = self.tick();
        self.pages.insert(
            page,
            Slot {
                page: page_state,
                used,
            },
        );
        while self.pages.len() > MAX_PAGES {
            let Some(oldest) = self
                .pages
                .iter()
                .min_by_key(|(_, slot)| slot.used)
                .map(|(page, _)| *page)
            else {
                break;
            };
            self.pages.remove(&oldest);
        }
        true
    }

    /// The page that failed last and why, while it is still in memory.
    pub fn last_failure(&self) -> Option<(u64, Arc<str>)> {
        let page = self.last_failed?;
        match &self.pages.get(&page)?.page {
            Page::Failed(error) => Some((page, Arc::clone(error))),
            Page::Ready(_) => None,
        }
    }

    /// Copies the bytes at `offset` into `out` when every page they lie in is in memory.
    /// `out` is small (a value of at most 8 bytes), so it spans one or two pages.
    pub fn copy(&mut self, offset: u64, out: &mut [u8]) -> bool {
        let mut done = 0usize;
        while done < out.len() {
            let Some(at) = offset.checked_add(done as u64) else {
                return false;
            };
            let Some(Page::Ready(bytes)) = self.get(page_of(at)) else {
                return false;
            };
            // `at % PAGE` is below PAGE, a usize.
            let Some(available) = bytes.get((at % PAGE) as usize..) else {
                return false;
            };
            let n = available.len().min(out.len() - done);
            if n == 0 {
                return false;
            }
            out[done..done + n].copy_from_slice(&available[..n]);
            done += n;
        }
        true
    }

    #[cfg(test)]
    fn held(&self) -> Vec<u64> {
        let mut pages: Vec<u64> = self.pages.keys().copied().collect();
        pages.sort_unstable();
        pages
    }
}

// -------------------------------------------------------------------------------------------
// Selection
// -------------------------------------------------------------------------------------------

/// A step of the keyboard caret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Left,
    Right,
    Up,
    Down,
}

/// The selected bytes: from `anchor` (where the selection started) to `head` (where it ends
/// now), both included, in either order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: u64,
    pub head: u64,
}

impl Selection {
    /// One byte.
    pub fn at(offset: u64) -> Self {
        Self {
            anchor: offset,
            head: offset,
        }
    }

    /// The selected bytes as a half-open range.
    pub fn range(&self) -> Range<u64> {
        let (low, high) = if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        };
        low..high.saturating_add(1)
    }

    /// Bytes selected (at least 1).
    #[cfg(test)]
    pub fn len(&self) -> u64 {
        let range = self.range();
        range.end - range.start
    }

    /// The head moved one step in a stream of `len` bytes; the anchor stays when `extend`, else
    /// the selection becomes the one byte under the head. Stops at both ends.
    pub fn moved(self, step: Step, extend: bool, len: u64) -> Self {
        let last = len.saturating_sub(1);
        let head = match step {
            Step::Left => self.head.saturating_sub(1),
            Step::Right => self.head.saturating_add(1).min(last),
            Step::Up => self.head.checked_sub(BYTES_PER_ROW).unwrap_or(self.head),
            Step::Down => match self.head.checked_add(BYTES_PER_ROW) {
                Some(next) if next <= last => next,
                _ => self.head,
            },
        };
        if extend {
            Self {
                anchor: self.anchor,
                head,
            }
        } else {
            Self::at(head)
        }
    }
}

// -------------------------------------------------------------------------------------------
// Go to byte
// -------------------------------------------------------------------------------------------

/// Why "Go to byte" cannot go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoToError {
    Empty,
    NotANumber,
    TooLarge,
    /// The stream has this many bytes.
    PastEnd(u64),
}

impl GoToError {
    /// What the prompt says.
    pub fn message(&self) -> String {
        match self {
            Self::Empty => "Type an offset.".to_owned(),
            Self::NotANumber => "Type a decimal number, or hex starting with 0x.".to_owned(),
            Self::TooLarge => "That number is too large.".to_owned(),
            Self::PastEnd(0) => "This stream is empty.".to_owned(),
            Self::PastEnd(len) => format!(
                "The last byte is at {} (0x{:X}).",
                format::grouped(len - 1),
                len - 1
            ),
        }
    }
}

/// The offset typed into "Go to byte": decimal, or hex with `0x`. Spaces around it, and `_` or
/// `,` between digits (as the side column groups them), are allowed.
pub fn parse_offset(text: &str) -> Result<u64, GoToError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(GoToError::Empty);
    }
    let (digits, radix) = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => (hex, 16),
        None => (text, 10),
    };
    let mut value: u64 = 0;
    let mut any = false;
    for c in digits.chars() {
        if c == '_' || c == ',' {
            continue;
        }
        let digit = c.to_digit(radix).ok_or(GoToError::NotANumber)?;
        value = value
            .checked_mul(u64::from(radix))
            .and_then(|v| v.checked_add(u64::from(digit)))
            .ok_or(GoToError::TooLarge)?;
        any = true;
    }
    if any {
        Ok(value)
    } else {
        Err(GoToError::NotANumber)
    }
}

/// The byte "Go to byte" goes to in a stream of `len` bytes.
pub fn go_to_target(text: &str, len: u64) -> Result<u64, GoToError> {
    let offset = parse_offset(text)?;
    if offset >= len {
        return Err(GoToError::PastEnd(len));
    }
    Ok(offset)
}

/// True for a character the "Go to byte" field accepts as typed.
pub fn go_to_accepts(c: char) -> bool {
    c.is_ascii_hexdigit() || matches!(c, 'x' | 'X' | '_' | ',')
}

/// Most characters the "Go to byte" field holds: more than any `u64` needs, with separators.
pub const GO_TO_MAX_CHARS: usize = 32;

// -------------------------------------------------------------------------------------------
// Rows
// -------------------------------------------------------------------------------------------

/// How the Text column reads bytes above 0x7F.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextColumn {
    /// Printable ASCII; everything else is `.`.
    #[default]
    Ascii,
    /// The stream is treated as Mac OS Roman (an HFS volume, or a `TEXT` file).
    #[allow(dead_code)] // Built by the M6 volume formats, with the Roman table in `text_char`.
    MacRoman,
}

/// The character the Text column shows for `byte`.
pub fn text_char(byte: u8, column: TextColumn) -> char {
    match (byte, column) {
        (0x20..=0x7E, _) => char::from(byte),
        // TODO(M6): Mac OS Roman's own characters for 0x80..=0xFF, from the M6 table in
        // `disk_atlas_lib::mac`. Until it lands they show as `.`, like any other byte; the
        // preview's text provider waits on the same table.
        (_, TextColumn::MacRoman) => '.',
        (_, TextColumn::Ascii) => '.',
    }
}

/// Hex digits of the offset column for a stream of `len` bytes: 8, or 12 (more past 256 TiB)
/// when the stream is 4 GiB or longer.
pub fn offset_digits(len: u64) -> usize {
    if len < WIDE_OFFSETS {
        return 8;
    }
    let needed = (u64::BITS - len.saturating_sub(1).leading_zeros()).div_ceil(4) as usize;
    needed.max(12)
}

/// Writes `offset` as `digits` upper-case hex digits (more when it needs them).
pub fn write_offset(offset: u64, digits: usize, out: &mut String) {
    use std::fmt::Write as _;
    // Writing to a String does not fail.
    let _ = write!(out, "{offset:0digits$X}");
}

const HEX_DIGITS: &[u8; 16] = b"0123456789ABCDEF";

/// Where byte `column` (0 to 15) starts in a row's hex text.
pub fn hex_column_start(column: usize) -> usize {
    column * 3 + usize::from(column >= 8)
}

/// The byte column under character `index` of a row's hex text. A space belongs to the byte
/// before it.
pub fn hex_column_at(index: usize) -> usize {
    let column = match index {
        0..=23 => index / 3,
        // The double gap after the eighth byte belongs to it.
        24 => 7,
        _ => (index - 1) / 3,
    };
    column.min(15)
}

/// Writes the hex of up to 16 bytes, padded to [`HEX_ROW_CHARS`] so the Text column lines up on a
/// short last row. No bytes gives a blank row (a page that has not arrived).
pub fn write_hex_row(bytes: &[u8], out: &mut String) {
    let start = out.len();
    for (column, &byte) in bytes.iter().take(BYTES_PER_ROW as usize).enumerate() {
        if column > 0 {
            out.push(' ');
        }
        if column == 8 {
            out.push(' ');
        }
        out.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(HEX_DIGITS[usize::from(byte & 0xF)]));
    }
    while out.len() - start < HEX_ROW_CHARS {
        out.push(' ');
    }
}

/// Writes the Text column of up to 16 bytes and records where each byte's character starts
/// (`starts[i]`, with `starts[n]` the end), so a highlight can cover whole characters.
pub fn write_text_row(
    bytes: &[u8],
    column: TextColumn,
    out: &mut String,
    starts: &mut [usize; 17],
) {
    let base = out.len();
    let bytes = &bytes[..bytes.len().min(BYTES_PER_ROW as usize)];
    for (i, &byte) in bytes.iter().enumerate() {
        starts[i] = out.len() - base;
        out.push(text_char(byte, column));
    }
    for start in starts.iter_mut().skip(bytes.len()) {
        *start = out.len() - base;
    }
}

/// How a byte of the grid is marked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mark {
    #[default]
    None,
    /// Inside a structure field the selection touches (`hover` tone).
    Field,
    /// Selected (the selection colour). Wins over `Field`.
    Selected,
}

/// The marks of the `count` bytes of the row at `row`.
pub fn row_marks(
    row: u64,
    count: usize,
    selection: Option<&Range<u64>>,
    fields: &[Range<u64>],
) -> [Mark; 16] {
    let mut marks = [Mark::None; 16];
    for (column, mark) in marks.iter_mut().enumerate().take(count) {
        let Some(offset) = row.checked_add(column as u64) else {
            break;
        };
        if selection.is_some_and(|range| range.contains(&offset)) {
            *mark = Mark::Selected;
        } else if fields.iter().any(|range| range.contains(&offset)) {
            *mark = Mark::Field;
        }
    }
    marks
}

/// Runs of equal marks, as column ranges; unmarked runs are left out.
pub fn mark_runs(marks: &[Mark]) -> Vec<(Range<usize>, Mark)> {
    let mut runs: Vec<(Range<usize>, Mark)> = Vec::new();
    for (column, &mark) in marks.iter().enumerate() {
        if mark == Mark::None {
            continue;
        }
        match runs.last_mut() {
            Some((range, last)) if *last == mark && range.end == column => range.end += 1,
            _ => runs.push((column..column + 1, mark)),
        }
    }
    runs
}

// -------------------------------------------------------------------------------------------
// The side column
// -------------------------------------------------------------------------------------------

/// "41,968 (0xA3F0)"
pub fn number_text(n: u64) -> String {
    format!("{} (0x{n:X})", format::grouped(n))
}

/// A signed number with grouping: "-1,024".
fn signed_text(n: i64) -> String {
    if n < 0 {
        format!("-{}", format::grouped(n.unsigned_abs()))
    } else {
        format::grouped(n.unsigned_abs())
    }
}

/// The offset and length lines for a selection.
pub fn selection_facts(range: &Range<u64>) -> Vec<(&'static str, String)> {
    let len = range.end - range.start;
    let length = if len == 1 {
        "1 byte (0x1)".to_owned()
    } else {
        format!("{} bytes (0x{len:X})", format::grouped(len))
    };
    vec![("Offset", number_text(range.start)), ("Length", length)]
}

/// Four bytes as a quoted type code: printable ASCII as is, anything else `.`.
pub fn four_cc(bytes: [u8; 4]) -> String {
    let code: String = bytes
        .iter()
        .map(|&b| text_char(b, TextColumn::Ascii))
        .collect();
    format!("'{code}'")
}

/// Seconds since 1904-01-01 00:00 UTC as a date in UTC.
pub fn hfs_date(seconds: u32) -> String {
    let unix = i64::from(seconds) - MAC_TO_UNIX;
    format!(
        "{} UTC",
        format::date_time_at_offset(Timestamp::from_unix(unix, 0), 0)
    )
}

/// Nanoseconds since 1970-01-01 UTC as a date in UTC, when the value is a plausible date.
pub fn unix_nanos_date(nanos: i64) -> Option<String> {
    PLAUSIBLE_NANOS.contains(&nanos).then(|| {
        format!(
            "{} UTC",
            format::date_time_at_offset(Timestamp::from_unix_nanos(nanos), 0)
        )
    })
}

/// What a selection of 1, 2, 4 or 8 bytes reads as. Empty for any other length.
pub fn value_facts(bytes: &[u8]) -> Vec<(&'static str, String)> {
    let mut facts = Vec::new();
    match *bytes {
        [b] => {
            facts.push(("Unsigned", format::grouped(u64::from(b))));
            facts.push(("Signed", signed_text(i64::from(b as i8))));
        }
        [a, b] => {
            let (be, le) = (u16::from_be_bytes([a, b]), u16::from_le_bytes([a, b]));
            facts.push(("Unsigned BE", format::grouped(u64::from(be))));
            facts.push(("Signed BE", signed_text(i64::from(be as i16))));
            facts.push(("Unsigned LE", format::grouped(u64::from(le))));
            facts.push(("Signed LE", signed_text(i64::from(le as i16))));
        }
        [a, b, c, d] => {
            let raw = [a, b, c, d];
            let (be, le) = (u32::from_be_bytes(raw), u32::from_le_bytes(raw));
            facts.push(("Unsigned BE", format::grouped(u64::from(be))));
            facts.push(("Signed BE", signed_text(i64::from(be as i32))));
            facts.push(("Unsigned LE", format::grouped(u64::from(le))));
            facts.push(("Signed LE", signed_text(i64::from(le as i32))));
            facts.push(("Type code", four_cc(raw)));
            facts.push(("HFS date", hfs_date(be)));
        }
        [a, b, c, d, e, f, g, h] => {
            let raw = [a, b, c, d, e, f, g, h];
            let (be, le) = (u64::from_be_bytes(raw), u64::from_le_bytes(raw));
            facts.push(("Unsigned BE", format::grouped(be)));
            facts.push(("Signed BE", signed_text(be as i64)));
            facts.push(("Unsigned LE", format::grouped(le)));
            facts.push(("Signed LE", signed_text(le as i64)));
            if let Some(date) = unix_nanos_date(be as i64) {
                facts.push(("Unix time BE", date));
            }
            if let Some(date) = unix_nanos_date(le as i64) {
                facts.push(("Unix time LE", date));
            }
        }
        _ => {}
    }
    facts
}

/// True when the selection is a length [`value_facts`] reads.
pub fn has_value(len: u64) -> bool {
    matches!(len, 1 | 2 | 4 | 8)
}

/// The structure fields the selection touches, in order, at most [`MAX_FIELDS_SHOWN`].
pub fn fields_at<'a>(
    structures: &'a [Structure],
    selection: &Range<u64>,
) -> Vec<(&'a Structure, &'a Field)> {
    structures
        .iter()
        .flat_map(|structure| structure.fields.iter().map(move |field| (structure, field)))
        .filter(|(_, field)| field.range.start < selection.end && selection.start < field.range.end)
        .take(MAX_FIELDS_SHOWN)
        .collect()
}

// -------------------------------------------------------------------------------------------
// Copying
// -------------------------------------------------------------------------------------------

/// Bytes Ctrl+C copies of a selection of `len` bytes.
pub fn copy_len(len: u64) -> u64 {
    len.min(MAX_COPY)
}

/// The bytes as hex text: pairs separated by spaces, 16 to a line.
pub fn hex_text(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 3);
    for (i, &byte) in bytes.iter().enumerate() {
        if i > 0 {
            out.push(if i % 16 == 0 { '\n' } else { ' ' });
        }
        out.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(HEX_DIGITS[usize::from(byte & 0xF)]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use disk_atlas_lib::error::Result as LibResult;
    use disk_atlas_lib::io::block::check_range;

    /// A stream of any length whose bytes are computed, not stored.
    struct Generated(u64);

    fn generated_byte(offset: u64) -> u8 {
        (offset ^ (offset >> 8) ^ (offset >> 24)) as u8
    }

    impl BlockSource for Generated {
        fn len(&self) -> u64 {
            self.0
        }
        fn sector_size(&self) -> u32 {
            512
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> LibResult<()> {
            check_range(offset, buf.len(), self.0)?;
            for (i, byte) in buf.iter_mut().enumerate() {
                *byte = generated_byte(offset + i as u64);
            }
            Ok(())
        }
    }

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;

    // --- window ---

    #[test]
    fn a_small_stream_is_one_window_without_more_rows() {
        let window = ByteWindow::new(100);
        assert_eq!(window.data_rows(), 7);
        assert_eq!(window.list_len(), 7);
        assert!(!window.has_earlier() && !window.has_later());
        assert_eq!(window.item(0), Some(Item::Row(0)));
        assert_eq!(window.item(6), Some(Item::Row(96)));
        assert_eq!(window.item(7), None);
        assert_eq!(window.index_of(99), Some(6));
        assert_eq!(window.index_of(100), None);
        assert_eq!(window.edge(0, 7), None);
    }

    #[test]
    fn empty_and_one_byte_streams() {
        let empty = ByteWindow::new(0);
        assert_eq!(empty.list_len(), 0);
        assert_eq!(empty.item(0), None);
        assert_eq!(empty.index_of(0), None);
        assert_eq!(ByteWindow::around(0, 5), empty);

        let one = ByteWindow::around(1, 1_000);
        assert_eq!(one.start(), 0);
        assert_eq!(one.list_len(), 1);
        assert_eq!(one.item(0), Some(Item::Row(0)));
        assert_eq!(one.index_of(0), Some(0));
    }

    #[test]
    fn a_stream_of_exactly_one_window_has_no_more_rows() {
        let window = ByteWindow::around(WINDOW, WINDOW - 1);
        assert_eq!(window.start(), 0);
        assert_eq!(window.data_rows() as u64, WINDOW_ROWS);
        assert_eq!(window.list_len() as u64, WINDOW_ROWS);
        assert!(!window.has_later());
        // One byte more and there is a later row.
        let longer = ByteWindow::new(WINDOW + 1);
        assert!(longer.has_later());
        assert_eq!(longer.list_len() as u64, WINDOW_ROWS + 1);
        assert_eq!(longer.item(WINDOW_ROWS as usize), Some(Item::Later));
    }

    #[test]
    fn the_window_start_is_a_multiple_of_half_with_context_around_the_target() {
        let len = 100 * MIB;
        for target in [0, 1, MIB, 3 * MIB, 50 * MIB + 7, 97 * MIB, len - 1, len + 5] {
            let window = ByteWindow::around(len, target);
            assert_eq!(window.start() % HALF, 0, "target {target}");
            let clamped = target.min(len - 1);
            assert!(window.contains(clamped), "target {target}");
            // A quarter window of room both ways, unless an end of the stream is nearer.
            if window.has_earlier() {
                assert!(clamped - window.start() >= WINDOW / 4, "target {target}");
            }
            if window.has_later() {
                assert!(window.end() - clamped > WINDOW / 4, "target {target}");
            }
        }
        // Near the end the window is pulled back so it is never mostly empty.
        let last = ByteWindow::around(len, len - 1);
        assert!(!last.has_later());
        assert!(last.end() - last.start() > HALF);
    }

    #[test]
    fn shifting_moves_by_half_and_keeps_the_rows_under_the_eye() {
        let len = 64 * MIB;
        let window = ByteWindow::around(len, 20 * MIB);
        assert!(window.has_earlier() && window.has_later());
        let top = 40; // within EDGE_ROWS of "Earlier bytes"
        assert_eq!(window.edge(top, top + 30), Some(Shift::Earlier));
        let before = window.item(top);
        let (moved, index) = window.shift_keeping(Shift::Earlier, top).unwrap();
        assert_eq!(moved.start(), window.start() - HALF);
        assert_eq!(moved.item(index), before);
        // The same number of rows as half a window.
        assert_eq!((index - top) as u64, HALF / BYTES_PER_ROW);
        assert_eq!(moved.edge(index, index + 30), None);

        let bottom_top = window.list_len() - 50;
        assert_eq!(window.edge(bottom_top, bottom_top + 30), Some(Shift::Later));
        let before = window.item(bottom_top);
        let (moved, index) = window.shift_keeping(Shift::Later, bottom_top).unwrap();
        assert_eq!(moved.start(), window.start() + HALF);
        assert_eq!(moved.item(index), before);
        assert_eq!((bottom_top - index) as u64, HALF / BYTES_PER_ROW);
    }

    #[test]
    fn activating_a_more_row_keeps_its_neighbour_in_place() {
        let window = ByteWindow::around(64 * MIB, 20 * MIB);
        // "Earlier bytes" stands for the row just before the window.
        let (moved, index) = window.shift_keeping(Shift::Earlier, 0).unwrap();
        assert_eq!(moved.item(index), Some(Item::Row(window.start() - 16)));
        let later = window.list_len() - 1;
        let (moved, index) = window.shift_keeping(Shift::Later, later).unwrap();
        assert_eq!(moved.item(index), Some(Item::Row(window.end())));
    }

    #[test]
    fn shifting_stops_at_both_ends() {
        let len = 10 * MIB + 3;
        let first = ByteWindow::new(len);
        assert_eq!(first.shifted(Shift::Earlier), None);
        assert_eq!(first.edge(0, 30), None);
        // Down to the start, the earlier row disappears and the index accounts for it.
        let second = first.shifted(Shift::Later).unwrap();
        let (back, index) = second.shift_keeping(Shift::Earlier, 10).unwrap();
        assert_eq!(back, first);
        assert_eq!(back.item(index), second.item(10));

        let mut window = first;
        let mut steps = 0;
        while let Some(next) = window.shifted(Shift::Later) {
            window = next;
            steps += 1;
            assert!(steps < 100);
        }
        assert!(!window.has_later());
        assert!(window.contains(len - 1));
        assert_eq!(window, ByteWindow::around(len, len - 1));
    }

    #[test]
    fn offsets_near_two_to_the_63_do_not_overflow() {
        let len = (1u64 << 63) + 12_345;
        let window = ByteWindow::around(len, len - 1);
        assert!(window.contains(len - 1));
        assert!(!window.has_later());
        let index = window.index_of(len - 1).unwrap();
        assert!(index < window.list_len());
        assert_eq!(
            window.item(window.list_len() - 1),
            Some(Item::Row((len - 1) / 16 * 16))
        );
        assert_eq!(page_range(page_of(len - 1), len).unwrap().end, len);
        let huge = ByteWindow::around(u64::MAX, u64::MAX);
        assert!(huge.contains(u64::MAX - 1));
        assert!(huge.index_of(u64::MAX - 1).is_some());
        assert_eq!(offset_digits(len), 16);
        let mut text = String::new();
        write_offset(len - 1, offset_digits(len), &mut text);
        assert_eq!(text, "8000000000003038");
        assert_eq!(
            Selection::at(u64::MAX - 1).moved(Step::Down, false, u64::MAX),
            Selection::at(u64::MAX - 1)
        );
        assert_eq!(Selection::at(u64::MAX - 1).range().end, u64::MAX);
    }

    #[test]
    fn large_synthetic_streams_navigate_to_the_end() {
        for len in [3 * GIB, 5 * GIB + 7] {
            let source = Generated(len);
            let last = len - 1;
            let window = ByteWindow::around(len, last);
            assert!(window.contains(last));
            let index = window.index_of(last).unwrap();
            let Some(Item::Row(row)) = window.item(index) else {
                panic!("not a row");
            };
            assert_eq!(row, last / 16 * 16);
            let page = read_page(&source, page_of(last)).unwrap();
            assert_eq!(page.last().copied(), Some(generated_byte(last)));
            assert!(read_page(&source, page_of(last) + 1).is_err());

            let mut cache = PageCache::default();
            let generation = cache.reset();
            assert!(cache.want(page_of(last)));
            assert!(cache.arrive(generation, page_of(last), Ok(page)));
            let mut out = [0u8; 1];
            assert!(cache.copy(last, &mut out));
            assert_eq!(out[0], generated_byte(last));
            // Asking for a byte past the end fails rather than wrapping.
            assert!(!cache.copy(last, &mut [0u8; 2]));

            // Going to the start again and stepping to the end window by window.
            let mut walk = ByteWindow::around(len, 0);
            let mut steps = 0u64;
            while let Some(next) = walk.shifted(Shift::Later) {
                walk = next;
                steps += 1;
            }
            assert_eq!(walk, window);
            assert!(steps <= len / HALF);
            assert_eq!(offset_digits(len), if len < WIDE_OFFSETS { 8 } else { 12 });
        }
    }

    // --- pages ---

    #[test]
    fn requests_are_made_once() {
        let mut cache = PageCache::default();
        let generation = cache.reset();
        assert!(cache.want(3));
        assert!(!cache.want(3), "already asked for");
        assert!(cache.arrive(generation, 3, Ok(Arc::from(vec![1u8; 4]))));
        assert!(!cache.want(3), "in memory");
        assert!(matches!(cache.get(3), Some(Page::Ready(_))));
        // An answer nobody asked for is dropped.
        assert!(!cache.arrive(generation, 9, Ok(Arc::from(vec![]))));
    }

    #[test]
    fn answers_of_an_older_generation_are_dropped() {
        let mut cache = PageCache::default();
        let old = cache.reset();
        assert!(cache.want(0));
        let new = cache.reset();
        assert_ne!(old, new);
        assert!(!cache.arrive(old, 0, Ok(Arc::from(vec![1u8]))));
        assert!(cache.get(0).is_none());
        // The page can be asked for again under the new generation.
        assert!(cache.want(0));
        assert!(cache.arrive(new, 0, Ok(Arc::from(vec![1u8]))));
    }

    #[test]
    fn the_least_recently_used_page_goes_first() {
        let mut cache = PageCache::default();
        let generation = cache.reset();
        for page in 0..MAX_PAGES as u64 {
            assert!(cache.want(page));
            assert!(cache.arrive(generation, page, Ok(Arc::from(vec![0u8]))));
        }
        // Page 0 is used again, so page 1 is now the oldest.
        assert!(cache.get(0).is_some());
        let extra = MAX_PAGES as u64;
        assert!(cache.want(extra));
        assert!(cache.arrive(generation, extra, Ok(Arc::from(vec![0u8]))));
        let held = cache.held();
        assert_eq!(held.len(), MAX_PAGES);
        assert!(held.contains(&0) && held.contains(&extra) && !held.contains(&1));
    }

    #[test]
    fn a_failed_page_is_kept_with_its_reason() {
        let mut cache = PageCache::default();
        let generation = cache.reset();
        assert!(cache.want(2));
        assert!(cache.arrive(generation, 2, Err("read error".to_owned())));
        assert!(matches!(cache.get(2), Some(Page::Failed(_))));
        assert!(!cache.want(2), "a failed page is not asked for in a loop");
        let (page, error) = cache.last_failure().unwrap();
        assert_eq!((page, &*error), (2, "read error"));
        cache.reset();
        assert!(cache.last_failure().is_none());
    }

    #[test]
    fn values_are_copied_across_two_pages() {
        let mut cache = PageCache::default();
        let generation = cache.reset();
        let first: Vec<u8> = (0..PAGE).map(|i| i as u8).collect();
        let second: Vec<u8> = vec![0xAA; 16];
        cache.want(0);
        cache.want(1);
        cache.arrive(generation, 0, Ok(first.into()));
        let mut out = [0u8; 4];
        assert!(
            !cache.copy(PAGE - 2, &mut out),
            "the second page is missing"
        );
        cache.arrive(generation, 1, Ok(second.into()));
        assert!(cache.copy(PAGE - 2, &mut out));
        assert_eq!(out, [0xFE, 0xFF, 0xAA, 0xAA]);
    }

    #[test]
    fn page_ranges_end_at_the_stream() {
        assert_eq!(page_range(0, 10), Some(0..10));
        assert_eq!(page_range(1, PAGE + 1), Some(PAGE..PAGE + 1));
        assert_eq!(page_range(1, PAGE), None);
        assert_eq!(page_range(u64::MAX, u64::MAX), None);
        assert_eq!(
            read_page(&Generated(0), 0).unwrap_err(),
            "past the end of the stream"
        );
    }

    // --- selection ---

    #[test]
    fn the_caret_moves_and_extends_within_the_stream() {
        let len = 40;
        let at = Selection::at(17);
        assert_eq!(at.moved(Step::Left, false, len), Selection::at(16));
        assert_eq!(at.moved(Step::Up, false, len), Selection::at(1));
        assert_eq!(
            Selection::at(1).moved(Step::Up, false, len),
            Selection::at(1)
        );
        assert_eq!(at.moved(Step::Down, false, len), Selection::at(33));
        assert_eq!(
            Selection::at(33).moved(Step::Down, false, len),
            Selection::at(33)
        );
        assert_eq!(
            Selection::at(39).moved(Step::Right, false, len),
            Selection::at(39)
        );
        assert_eq!(
            Selection::at(0).moved(Step::Left, false, len),
            Selection::at(0)
        );
        let extended = at
            .moved(Step::Right, true, len)
            .moved(Step::Down, true, len);
        assert_eq!(
            extended,
            Selection {
                anchor: 17,
                head: 34
            }
        );
        assert_eq!(extended.range(), 17..35);
        assert_eq!(extended.len(), 18);
        let backwards = at.moved(Step::Up, true, len);
        assert_eq!(backwards.range(), 1..18);
    }

    // --- go to byte ---

    #[test]
    fn offsets_parse_as_decimal_or_hex() {
        assert_eq!(parse_offset("0x1F"), Ok(31));
        assert_eq!(parse_offset("0X1f"), Ok(31));
        assert_eq!(parse_offset("31"), Ok(31));
        assert_eq!(parse_offset("  41,968 "), Ok(41_968));
        assert_eq!(parse_offset("0xA3_F0"), Ok(0xA3F0));
        assert_eq!(parse_offset(""), Err(GoToError::Empty));
        assert_eq!(parse_offset("   "), Err(GoToError::Empty));
        assert_eq!(parse_offset("1F"), Err(GoToError::NotANumber));
        assert_eq!(parse_offset("0x"), Err(GoToError::NotANumber));
        assert_eq!(parse_offset("0xG"), Err(GoToError::NotANumber));
        assert_eq!(parse_offset("-1"), Err(GoToError::NotANumber));
        assert_eq!(parse_offset("hello"), Err(GoToError::NotANumber));
        assert_eq!(parse_offset("18446744073709551615"), Ok(u64::MAX));
        assert_eq!(
            parse_offset("18446744073709551616"),
            Err(GoToError::TooLarge)
        );
        assert_eq!(
            parse_offset("0x10000000000000000"),
            Err(GoToError::TooLarge)
        );
        assert_eq!(go_to_target("0x1F", 32), Ok(31));
        assert_eq!(go_to_target("32", 32), Err(GoToError::PastEnd(32)));
        assert_eq!(
            GoToError::PastEnd(32).message(),
            "The last byte is at 31 (0x1F)."
        );
        assert_eq!(GoToError::PastEnd(0).message(), "This stream is empty.");
        assert!(go_to_accepts('f') && go_to_accepts('x') && !go_to_accepts(' '));
    }

    // --- rows ---

    #[test]
    fn rows_are_written_with_a_gap_after_the_eighth_byte() {
        let bytes = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52,
        ];
        let mut hex = String::new();
        write_hex_row(&bytes, &mut hex);
        assert_eq!(hex, "89 50 4E 47 0D 0A 1A 0A  00 00 00 0D 49 48 44 52");
        assert_eq!(hex.len(), HEX_ROW_CHARS);
        let mut text = String::new();
        let mut starts = [0; 17];
        write_text_row(&bytes, TextColumn::Ascii, &mut text, &mut starts);
        assert_eq!(text, ".PNG........IHDR");
        assert_eq!(starts[16], 16);

        // A short row is padded so the Text column lines up; no bytes is a blank row.
        let mut short = String::new();
        write_hex_row(&[0xAB], &mut short);
        assert_eq!(short.len(), HEX_ROW_CHARS);
        assert!(short.starts_with("AB "));
        let mut blank = String::new();
        write_hex_row(&[], &mut blank);
        assert_eq!(blank, " ".repeat(HEX_ROW_CHARS));
    }

    #[test]
    fn hex_columns_map_both_ways() {
        let mut hex = String::new();
        write_hex_row(&[0u8; 16], &mut hex);
        for column in 0..16 {
            let start = hex_column_start(column);
            assert_eq!(&hex[start..start + 2], "00");
            assert_eq!(hex_column_at(start), column);
            assert_eq!(hex_column_at(start + 1), column);
        }
        // The spaces belong to the byte before them, the double gap included.
        assert_eq!(hex_column_at(2), 0);
        assert_eq!(hex_column_at(23), 7);
        assert_eq!(hex_column_at(24), 7);
        assert_eq!(hex_column_at(500), 15);
    }

    #[test]
    fn the_text_column_reads_ascii_and_marks_the_rest() {
        assert_eq!(text_char(b'A', TextColumn::Ascii), 'A');
        assert_eq!(text_char(b' ', TextColumn::Ascii), ' ');
        assert_eq!(text_char(b'~', TextColumn::Ascii), '~');
        assert_eq!(text_char(0x7F, TextColumn::Ascii), '.');
        assert_eq!(text_char(0x00, TextColumn::Ascii), '.');
        assert_eq!(text_char(0xA9, TextColumn::Ascii), '.');
        // Mac OS Roman agrees with ASCII below 0x80; above it waits for the M6 table.
        assert_eq!(text_char(b'z', TextColumn::MacRoman), 'z');
        assert_eq!(text_char(0x0D, TextColumn::MacRoman), '.');
        assert_eq!(text_char(0xA5, TextColumn::MacRoman), '.');
        let mut text = String::new();
        let mut starts = [0; 17];
        write_text_row(b"Hi\x80!", TextColumn::MacRoman, &mut text, &mut starts);
        assert_eq!(text, "Hi.!");
        assert_eq!(&starts[..5], &[0, 1, 2, 3, 4]);
        assert!(starts[5..].iter().all(|&s| s == 4));
    }

    #[test]
    fn offsets_widen_at_four_gibibytes() {
        assert_eq!(offset_digits(0), 8);
        assert_eq!(offset_digits(WIDE_OFFSETS - 1), 8);
        assert_eq!(offset_digits(WIDE_OFFSETS), 12);
        assert_eq!(offset_digits(1 << 48), 12);
        assert_eq!(offset_digits((1 << 48) + 1), 13);
        let mut text = String::new();
        write_offset(0xA3F0, 8, &mut text);
        assert_eq!(text, "0000A3F0");
        text.clear();
        write_offset(0xA3F0, offset_digits(5 * GIB), &mut text);
        assert_eq!(text, "00000000A3F0");
    }

    #[test]
    fn marks_prefer_the_selection_and_merge_into_runs() {
        let fields = [10..20, 30..34];
        let marks = row_marks(16, 16, Some(&(18..22)), &fields);
        use Mark::{Field as F, None as N, Selected as S};
        assert_eq!(marks, [F, F, S, S, S, S, N, N, N, N, N, N, N, N, F, F]);
        assert_eq!(mark_runs(&marks), vec![(0..2, F), (2..6, S), (14..16, F)]);
        // Bytes past a short row's end are never marked.
        let short = row_marks(0, 3, Some(&(0..100)), &[]);
        assert_eq!(&short[..4], &[S, S, S, N]);
        assert!(mark_runs(&[N; 16]).is_empty());
    }

    // --- the side column ---

    #[test]
    fn values_read_both_ways() {
        let one = value_facts(&[0xFF]);
        assert_eq!(
            one,
            vec![("Unsigned", "255".to_owned()), ("Signed", "-1".to_owned())]
        );
        let two = value_facts(&[0x01, 0x80]);
        assert_eq!(
            two,
            vec![
                ("Unsigned BE", "384".to_owned()),
                ("Signed BE", "384".to_owned()),
                ("Unsigned LE", "32,769".to_owned()),
                ("Signed LE", "-32,767".to_owned()),
            ]
        );
        let four = value_facts(&[0xFF, 0xFF, 0xFF, 0xFE]);
        assert_eq!(four[0], ("Unsigned BE", "4,294,967,294".to_owned()));
        assert_eq!(four[1], ("Signed BE", "-2".to_owned()));
        assert_eq!(four[2], ("Unsigned LE", "4,278,190,079".to_owned()));
        assert_eq!(four[3], ("Signed LE", "-16,777,217".to_owned()));
        assert_eq!(four[4], ("Type code", "'....'".to_owned()));
        let eight = value_facts(&[0x80, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            eight[0],
            ("Unsigned BE", "9,223,372,036,854,775,808".to_owned())
        );
        assert_eq!(
            eight[1],
            ("Signed BE", "-9,223,372,036,854,775,808".to_owned())
        );
        assert_eq!(eight[2], ("Unsigned LE", "128".to_owned()));
        assert_eq!(eight.len(), 4, "neither reading is a plausible date");
        assert!(value_facts(&[1, 2, 3]).is_empty());
        assert!(value_facts(&[]).is_empty());
        assert!(has_value(8) && !has_value(3));
    }

    #[test]
    fn type_codes_mark_what_is_not_printable() {
        assert_eq!(four_cc(*b"TEXT"), "'TEXT'");
        assert_eq!(four_cc([b'a', 0, 0xA9, b' ']), "'a.. '");
    }

    #[test]
    fn hfs_dates_count_from_1904() {
        assert_eq!(hfs_date(0), "1 Jan 1904, 00:00 UTC");
        assert_eq!(hfs_date(u32::MAX), "6 Feb 2040, 06:28 UTC");
        // 2020-01-12 09:41:00 UTC.
        assert_eq!(
            hfs_date(1_578_822_060 + 2_082_844_800),
            "12 Jan 2020, 09:41 UTC"
        );
        let four = value_facts(&0xD9_E4_7C_00u32.to_be_bytes());
        assert_eq!(four[5].0, "HFS date");
    }

    #[test]
    fn unix_nanoseconds_are_dated_only_when_plausible() {
        let nanos = 1_578_822_060_000_000_000i64;
        assert_eq!(
            unix_nanos_date(nanos).as_deref(),
            Some("12 Jan 2020, 09:41 UTC")
        );
        assert_eq!(unix_nanos_date(0), None);
        assert_eq!(unix_nanos_date(-1), None);
        assert_eq!(unix_nanos_date(i64::MAX), None);
        let facts = value_facts(&nanos.to_le_bytes());
        assert!(facts.contains(&("Unix time LE", "12 Jan 2020, 09:41 UTC".to_owned())));
        assert!(!facts.iter().any(|(name, _)| *name == "Unix time BE"));
    }

    #[test]
    fn selection_facts_give_decimal_and_hex() {
        assert_eq!(
            selection_facts(&(0xA3F0..0xA3F4)),
            vec![
                ("Offset", "41,968 (0xA3F0)".to_owned()),
                ("Length", "4 bytes (0x4)".to_owned()),
            ]
        );
        assert_eq!(selection_facts(&(0..1))[1].1, "1 byte (0x1)");
    }

    fn field(range: Range<u64>, name: &'static str) -> Field {
        Field {
            range,
            name,
            value: String::new(),
            note: "",
        }
    }

    #[test]
    fn fields_touched_by_the_selection_are_found() {
        let structures = vec![Structure {
            title: "PNG image header",
            fields: vec![
                field(0..8, "Signature"),
                field(8..12, "Header length"),
                field(16..20, "Width"),
                field(20..24, "Height"),
            ],
        }];
        let names = |range: Range<u64>| -> Vec<&str> {
            fields_at(&structures, &range)
                .into_iter()
                .map(|(_, field)| field.name)
                .collect()
        };
        assert_eq!(names(17..18), ["Width"]);
        assert_eq!(names(7..9), ["Signature", "Header length"]);
        assert!(names(12..16).is_empty(), "the gap between fields");
        assert!(names(24..25).is_empty(), "the end is not included");
        assert_eq!(names(0..100).len(), 4);
        let many = vec![Structure {
            title: "Many",
            fields: (0..20).map(|i| field(i..i + 1, "Byte")).collect(),
        }];
        assert_eq!(fields_at(&many, &(0..20)).len(), MAX_FIELDS_SHOWN);
    }

    // --- copying ---

    #[test]
    fn the_clipboard_text_is_hex_and_capped() {
        assert_eq!(hex_text(&[]), "");
        assert_eq!(hex_text(&[0x0A, 0xFF]), "0A FF");
        let line: Vec<u8> = (0..17).collect();
        assert_eq!(
            hex_text(&line),
            "00 01 02 03 04 05 06 07 08 09 0A 0B 0C 0D 0E 0F\n10"
        );
        assert_eq!(copy_len(10), 10);
        assert_eq!(copy_len(MAX_COPY), MAX_COPY);
        assert_eq!(copy_len(u64::MAX), MAX_COPY);
        let full = hex_text(&vec![0u8; MAX_COPY as usize]);
        assert_eq!(full.len(), MAX_COPY as usize * 3 - 1);
    }
}
