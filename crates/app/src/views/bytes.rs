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

//! The Bytes tab of Quick Look: the file's bytes as hex and text, 16 to a row, with a side column
//! that says what the selected bytes hold. Design: `m5-quicklook-copy.md` 4.1 and 4.2.
//!
//! The rules (which rows the list holds, which pages are in memory, what is selected, how a row
//! and a value are written) live in `state/bytes.rs` and are tested there. This file draws them
//! and runs the reads on the background executor: a page of 64 KiB at a time, asked for by the
//! row that needs it, answered under the generation it was asked in.
//!
//! The list is one 4 MiB window of the stream ([`ByteWindow`]); scrolling near an end moves the
//! window by half its size and the scroll offset by the same number of rows in the same update,
//! so the bytes under the eye do not move.
//!
//! Mouse: one handler on the grid turns a position into a byte (the list is uniform, so the row
//! follows from the scroll offset and the column from the character width of the mono face).
//! Keys: the grid has the `BytesGrid` context, deeper than the panel's, so its arrow keys move the
//! selection and not the file list's; the "Go to byte" field has `BytesGoTo` for the same reason.

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use disk_atlas_lib::fs::{NodeId, Stream};
use disk_atlas_lib::io::block::BlockSource;
use gpui::{
    AnyElement, Bounds, ClipboardItem, Context, Div, Entity, FocusHandle, Focusable, FontWeight,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Render, ScrollStrategy,
    SharedString, Subscription, Task, TextRun, UniformListScrollHandle, Window, div, point,
    prelude::*, px, transparent_black, uniform_list,
};

use crate::components::copy_ui::measure;
use crate::keymap::{BYTES_GO_TO, BYTES_GRID, bytes as actions};
use crate::preview::structure::{Structure, describe};
use crate::state::bytes::{
    self, BYTES_PER_ROW, ByteWindow, GO_TO_MAX_CHARS, HEX_ROW_CHARS, Item, Mark, Page, PageCache,
    Selection, Shift, Step, TextColumn,
};
use crate::state::quick_look::{LoadState, QuickLook};
use crate::theme::{
    Colors, Theme, fonts, metrics,
    scale::{radius, space, text},
};

/// Bytes of the stream's start the structure explanations read.
const HEAD_BYTES: usize = 4096;

/// Characters between the offset and the hex column, between the hex and the text column.
const GAP_CHARS: usize = 2;

/// Narrowest the grid is assumed to be before the character width is measured.
const FALLBACK_CHAR_WIDTH: Pixels = px(7.8);

/// What the opened stream was opened for: the load of the panel it belongs to, the file and the
/// fork. A change of any of them is another stream.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Key {
    generation: u64,
    node: NodeId,
    stream: Stream,
}

/// The stream, once opened, and everything the view holds about it.
struct Data {
    source: Arc<dyn BlockSource>,
    len: u64,
    /// Which opening this is, so a page read for a stream that is gone is not taken for this one's.
    serial: u64,
    window: ByteWindow,
    pages: PageCache,
    selection: Option<Selection>,
    /// Header structures found at the start of the stream.
    structures: Vec<Structure>,
    /// Extents of the structure fields the selection touches, for the grid's field marks.
    touched: Vec<Range<u64>>,
    column: TextColumn,
    digits: usize,
}

/// Where the opening of the stream stands.
enum Load {
    /// Nothing to show yet: the file's details are still loading.
    Idle,
    Opening,
    Failed(SharedString),
    Ready(Box<Data>),
}

/// The "Go to byte" field.
struct GoTo {
    text: String,
    error: Option<String>,
}

/// The Bytes tab.
pub struct BytesView {
    quick_look: Entity<QuickLook>,
    key: Option<Key>,
    state: Load,
    serial: u64,
    open_task: Option<Task<()>>,
    /// The grid's focus: arrows move the selection while it holds.
    focus: FocusHandle,
    go_to: Option<GoTo>,
    go_to_focus: FocusHandle,
    /// Ctrl+G came before the stream was read.
    go_to_pending: bool,
    scroll: UniformListScrollHandle,
    /// Where the grid was last drawn, for turning a mouse position into a byte.
    grid_bounds: Rc<Cell<Bounds<Pixels>>>,
    char_width: Option<Pixels>,
    /// The mouse button is down on the grid: moving extends the selection.
    dragging: bool,
    /// A window move is scheduled; asking for it again before it happens would repeat it.
    shifting: bool,
    _subscription: Subscription,
}

impl BytesView {
    /// The view of whatever `quick_look` shows.
    pub fn new(quick_look: Entity<QuickLook>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&quick_look, |_, _, cx| cx.notify());
        Self {
            quick_look,
            key: None,
            state: Load::Idle,
            serial: 0,
            open_task: None,
            focus: cx.focus_handle().tab_stop(true),
            go_to: None,
            go_to_focus: cx.focus_handle(),
            go_to_pending: false,
            scroll: UniformListScrollHandle::new(),
            grid_bounds: Rc::new(Cell::new(Bounds::default())),
            char_width: None,
            dragging: false,
            shifting: false,
            _subscription: subscription,
        }
    }

    /// The grid's tab stop.
    pub fn focus_stop(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// Ctrl+G: opens the "Go to byte" field, when there is a stream to go in.
    pub fn open_go_to(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.state {
            Load::Ready(data) if data.len > 0 => {}
            // An empty stream has no offset to go to.
            Load::Ready(_) | Load::Failed(_) => return,
            // The tab was just opened: the stream is not read yet. Open the field when it is.
            Load::Idle | Load::Opening => {
                self.go_to_pending = true;
                cx.notify();
                return;
            }
        }
        if self.go_to.is_none() {
            self.go_to = Some(GoTo {
                text: String::new(),
                error: None,
            });
        }
        window.focus(&self.go_to_focus);
        cx.notify();
    }

    // ---- opening the stream ----------------------------------------------------------------

    /// Opens the stream the panel shows when it is not the one held. Called while drawing; it
    /// only starts a task.
    fn ensure_stream(&mut self, cx: &mut Context<Self>) {
        let (wanted, failed) = {
            let quick_look = self.quick_look.read(cx);
            match (quick_look.subject(), quick_look.details()) {
                (Some(subject), LoadState::Ready(details))
                    if details.stream == quick_look.stream() =>
                {
                    let key = Key {
                        generation: quick_look.generation(),
                        node: details.metadata.node,
                        stream: details.stream,
                    };
                    (Some((key, Arc::clone(&subject.fs))), None)
                }
                (_, LoadState::Failed(text)) => (None, Some(text.clone())),
                _ => (None, None),
            }
        };
        let Some((key, fs)) = wanted else {
            // The file's details are not here (yet): nothing to read, and whatever was read
            // before belongs to a file that is gone.
            if self.key.take().is_some() {
                self.serial += 1;
                self.open_task = None;
                self.go_to = None;
            }
            self.state = failed.map_or(Load::Idle, Load::Failed);
            return;
        };
        if self.key.as_ref() == Some(&key) {
            return;
        }
        self.key = Some(key.clone());
        self.state = Load::Opening;
        self.go_to = None;
        self.dragging = false;
        self.serial += 1;
        let serial = self.serial;
        self.open_task = Some(cx.spawn(async move |this, cx| {
            let (node, stream) = (key.node, key.stream);
            let opened = cx
                .background_executor()
                .spawn(async move { fs.open(node, stream) })
                .await;
            this.update(cx, |this, cx| {
                if this.serial != serial {
                    return;
                }
                this.state = match opened {
                    Ok(source) => {
                        this.scroll
                            .0
                            .borrow()
                            .base_handle
                            .set_offset(point(Pixels::ZERO, Pixels::ZERO));
                        Load::Ready(Box::new(Data::new(source, serial)))
                    }
                    Err(error) => {
                        Load::Failed(format!("The bytes could not be read ({error}).").into())
                    }
                };
                cx.notify();
            })
            .ok();
        }));
    }

    // ---- pages -----------------------------------------------------------------------------

    /// Starts reading `page` unless it is in memory or on its way.
    fn request_page(&mut self, page: u64, cx: &mut Context<Self>) {
        let Load::Ready(data) = &mut self.state else {
            return;
        };
        if !data.pages.want(page) {
            return;
        }
        let (generation, serial) = (data.pages.generation(), data.serial);
        let source = Arc::clone(&data.source);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { bytes::read_page(source.as_ref(), page) })
                .await;
            this.update(cx, |this, cx| {
                let Load::Ready(data) = &mut this.state else {
                    return;
                };
                if data.serial != serial || !data.pages.arrive(generation, page, result) {
                    return;
                }
                if page == 0
                    && let Some(Page::Ready(head)) = data.pages.get(0)
                {
                    let head = &head[..head.len().min(HEAD_BYTES)];
                    data.structures = describe(head, this_stream(&this.key), data.len);
                    data.refresh_touched();
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ---- the window ------------------------------------------------------------------------

    /// The list row at the top of the grid.
    fn top_row(&self) -> usize {
        let offset = self.scroll.0.borrow().base_handle.offset().y;
        let rows = (-offset / metrics::BYTES_ROW_HEIGHT).floor();
        if rows.is_finite() && rows > 0.0 {
            rows as usize
        } else {
            0
        }
    }

    /// Moves the window one half, keeping the bytes under the eye where they are.
    fn shift_window(&mut self, shift: Shift, cx: &mut Context<Self>) {
        self.shifting = false;
        let top = self.top_row();
        let Load::Ready(data) = &mut self.state else {
            return;
        };
        let Some((next, index)) = data.window.shift_keeping(shift, top) else {
            return;
        };
        data.window = next;
        let handle = self.scroll.0.borrow().base_handle.clone();
        let offset = handle.offset();
        let rows = index as f32 - top as f32;
        handle.set_offset(point(offset.x, offset.y - metrics::BYTES_ROW_HEIGHT * rows));
        cx.notify();
    }

    /// Makes `offset` visible: moves the window when it does not hold it, then scrolls.
    fn reveal(&mut self, offset: u64, strategy: ScrollStrategy, strict: bool) {
        let Load::Ready(data) = &mut self.state else {
            return;
        };
        let moved = !data.window.contains(offset);
        if moved {
            data.window = ByteWindow::around(data.len, offset);
        }
        if let Some(index) = data.window.index_of(offset) {
            if moved || strict {
                self.scroll.scroll_to_item_strict(index, strategy);
            } else {
                self.scroll.scroll_to_item(index, strategy);
            }
        }
    }

    // ---- selection -------------------------------------------------------------------------

    fn set_selection(&mut self, selection: Selection, cx: &mut Context<Self>) {
        if let Load::Ready(data) = &mut self.state {
            data.selection = Some(selection);
            data.refresh_touched();
        }
        cx.notify();
    }

    /// The first byte of the row at the top of the grid.
    fn first_visible_byte(&self) -> u64 {
        let top = self.top_row();
        match &self.state {
            Load::Ready(data) => match data.window.item(top) {
                Some(Item::Row(offset)) => offset,
                _ => data.window.start(),
            },
            _ => 0,
        }
    }

    /// An arrow key: one byte or one row, from the head of the selection.
    fn step(&mut self, step: Step, extend: bool, cx: &mut Context<Self>) {
        let first = self.first_visible_byte();
        let Load::Ready(data) = &self.state else {
            return;
        };
        if data.len == 0 {
            return;
        }
        let Some(current) = data.selection else {
            // The first key press selects the byte at the top of the grid.
            self.set_selection(Selection::at(first), cx);
            return;
        };
        let next = current.moved(step, extend, data.len);
        let forward = matches!(step, Step::Right | Step::Down);
        self.set_selection(next, cx);
        let strategy = if forward {
            ScrollStrategy::Bottom
        } else {
            ScrollStrategy::Top
        };
        self.reveal(next.head, strategy, false);
    }

    /// Page Up and Page Down: the head moves by the rows the grid shows.
    fn page(&mut self, down: bool, cx: &mut Context<Self>) {
        let first = self.first_visible_byte();
        let rows = (self.grid_bounds.get().size.height / metrics::BYTES_ROW_HEIGHT)
            .floor()
            .max(1.0) as u64;
        let Load::Ready(data) = &self.state else {
            return;
        };
        if data.len == 0 {
            return;
        }
        let Some(current) = data.selection else {
            self.set_selection(Selection::at(first), cx);
            return;
        };
        let by = rows.saturating_mul(BYTES_PER_ROW);
        let head = if down {
            current.head.saturating_add(by).min(data.len - 1)
        } else {
            current.head.saturating_sub(by)
        };
        let next = Selection::at(head);
        self.set_selection(next, cx);
        let strategy = if down {
            ScrollStrategy::Bottom
        } else {
            ScrollStrategy::Top
        };
        self.reveal(head, strategy, false);
    }

    /// Home and End: the first and the last byte of the stream.
    fn jump(&mut self, to_end: bool, cx: &mut Context<Self>) {
        let Load::Ready(data) = &self.state else {
            return;
        };
        if data.len == 0 {
            return;
        }
        let target = if to_end { data.len - 1 } else { 0 };
        self.set_selection(Selection::at(target), cx);
        let strategy = if to_end {
            ScrollStrategy::Bottom
        } else {
            ScrollStrategy::Top
        };
        self.reveal(target, strategy, true);
    }

    /// Ctrl+C: the selected bytes as hex text, at most 1 MiB of them.
    fn copy(&mut self, cx: &mut Context<Self>) {
        let Load::Ready(data) = &self.state else {
            return;
        };
        let Some(selection) = data.selection else {
            return;
        };
        let range = selection.range();
        let wanted = bytes::copy_len(range.end - range.start);
        let source = Arc::clone(&data.source);
        cx.spawn(async move |this, cx| {
            let text = cx
                .background_executor()
                .spawn(async move {
                    // At most MAX_COPY, 1 MiB.
                    let mut buf = vec![0u8; wanted as usize];
                    source
                        .read_at(range.start, &mut buf)
                        .map(|()| bytes::hex_text(&buf))
                })
                .await;
            this.update(cx, |_, cx| match text {
                Ok(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
                Err(error) => tracing::warn!(%error, "the selected bytes could not be copied"),
            })
            .ok();
        })
        .detach();
    }

    // ---- the mouse -------------------------------------------------------------------------

    /// The byte under `position`. With `clamp`, a position outside the columns or past the rows
    /// gives the nearest byte (a drag that leaves the grid); without, `None`.
    fn byte_at(&self, position: gpui::Point<Pixels>, clamp: bool) -> Option<u64> {
        let Load::Ready(data) = &self.state else {
            return None;
        };
        if data.len == 0 {
            return None;
        }
        let bounds = self.grid_bounds.get();
        let ch = self.char_width.unwrap_or(FALLBACK_CHAR_WIDTH);
        let scrolled = self.scroll.0.borrow().base_handle.offset().y;
        let y = position.y - bounds.origin.y - scrolled;
        let mut index = (y / metrics::BYTES_ROW_HEIGHT).floor();
        if !index.is_finite() {
            return None;
        }
        let rows = data.window.list_len();
        if rows == 0 {
            return None;
        }
        if clamp {
            index = index.clamp(0.0, (rows - 1) as f32);
        } else if index < 0.0 {
            return None;
        }
        let row = match data.window.item(index as usize)? {
            Item::Row(row) => row,
            // The "more" rows hold no bytes; a drag onto one stops at the window's edge.
            Item::Earlier if clamp => data.window.start(),
            Item::Later if clamp => data.window.end().saturating_sub(1),
            _ => return None,
        };
        let x = position.x - bounds.origin.x - space::S16;
        let hex_start = ch * (data.digits + GAP_CHARS) as f32;
        let hex_end = hex_start + ch * HEX_ROW_CHARS as f32;
        let text_start = hex_end + ch * GAP_CHARS as f32;
        let column = if x >= hex_start && x < hex_end {
            bytes::hex_column_at(((x - hex_start) / ch).floor() as usize)
        } else if x >= text_start && x < text_start + ch * BYTES_PER_ROW as f32 {
            (((x - text_start) / ch).floor() as usize).min(15)
        } else if clamp {
            if x < hex_start { 0 } else { 15 }
        } else {
            return None;
        };
        let offset = row.saturating_add(column as u64);
        if offset >= data.len {
            return clamp.then(|| data.len - 1);
        }
        Some(offset)
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus);
        let Some(offset) = self.byte_at(event.position, false) else {
            return;
        };
        let anchor = match &self.state {
            Load::Ready(data) if event.modifiers.shift => {
                data.selection.map_or(offset, |selection| selection.anchor)
            }
            _ => offset,
        };
        self.dragging = true;
        self.set_selection(
            Selection {
                anchor,
                head: offset,
            },
            cx,
        );
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !self.dragging || event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        let Some(offset) = self.byte_at(event.position, true) else {
            return;
        };
        let Load::Ready(data) = &self.state else {
            return;
        };
        let Some(selection) = data.selection else {
            return;
        };
        if selection.head != offset {
            self.set_selection(
                Selection {
                    anchor: selection.anchor,
                    head: offset,
                },
                cx,
            );
        }
    }

    // ---- Go to byte ------------------------------------------------------------------------

    fn close_go_to(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.go_to.take().is_some() {
            window.focus(&self.focus);
            cx.notify();
        }
    }

    fn confirm_go_to(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(go_to), Load::Ready(data)) = (&mut self.go_to, &self.state) else {
            return;
        };
        match bytes::go_to_target(&go_to.text, data.len) {
            Ok(target) => {
                self.go_to = None;
                self.set_selection(Selection::at(target), cx);
                self.reveal(target, ScrollStrategy::Center, true);
                window.focus(&self.focus);
            }
            Err(error) => {
                go_to.error = Some(error.message());
                cx.notify();
            }
        }
    }

    /// Typing into the field: the characters an offset is written with, and Backspace.
    fn go_to_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(go_to) = &mut self.go_to else {
            return;
        };
        let keystroke = &event.keystroke;
        if keystroke.modifiers.control || keystroke.modifiers.platform || keystroke.modifiers.alt {
            return;
        }
        if keystroke.key == "backspace" {
            go_to.text.pop();
        } else if let Some(typed) = &keystroke.key_char {
            for c in typed.chars().filter(|&c| bytes::go_to_accepts(c)) {
                if go_to.text.chars().count() < GO_TO_MAX_CHARS {
                    go_to.text.push(c);
                }
            }
        } else {
            return;
        }
        go_to.error = None;
        cx.notify();
    }
}

/// The stream the held key is of.
fn this_stream(key: &Option<Key>) -> Stream {
    key.as_ref().map_or(Stream::Data, |key| key.stream)
}

impl Data {
    fn new(source: Arc<dyn BlockSource>, serial: u64) -> Self {
        let len = source.len();
        let mut pages = PageCache::default();
        pages.reset();
        Self {
            source,
            len,
            serial,
            window: ByteWindow::new(len),
            pages,
            selection: None,
            structures: Vec::new(),
            touched: Vec::new(),
            column: TextColumn::Ascii,
            digits: bytes::offset_digits(len),
        }
    }

    /// Recomputes which structure fields the selection touches.
    fn refresh_touched(&mut self) {
        self.touched = match self.selection {
            Some(selection) => bytes::fields_at(&self.structures, &selection.range())
                .into_iter()
                .map(|(_, field)| field.range.clone())
                .collect(),
            None => Vec::new(),
        };
    }
}

// ---- drawing -------------------------------------------------------------------------------

/// A piece of a row's text in the grid's face, drawn in `mark`'s colours.
fn piece(text: &str, mark: Mark, colors: &Colors) -> Div {
    let piece = div()
        .flex_none()
        .h_full()
        .flex()
        .items_center()
        .whitespace_nowrap()
        .child(SharedString::from(text.to_owned()));
    match mark {
        Mark::None => piece,
        Mark::Field => piece.bg(colors.field),
        Mark::Selected => piece.bg(colors.mark).text_color(colors.on_mark),
    }
}

/// `text` cut into pieces at the ends of the marked runs; `span` turns a run of byte columns into
/// the range of characters of `text` it covers.
fn pieces(
    text: &str,
    runs: &[(Range<usize>, Mark)],
    span: impl Fn(&Range<usize>) -> Range<usize>,
    colors: &Colors,
) -> Vec<Div> {
    let mut out = Vec::new();
    let mut at = 0;
    for (columns, mark) in runs {
        let range = span(columns);
        let (start, end) = (range.start.min(text.len()), range.end.min(text.len()));
        if start < at || start >= end {
            continue;
        }
        if start > at {
            out.push(piece(&text[at..start], Mark::None, colors));
        }
        out.push(piece(&text[start..end], *mark, colors));
        at = end;
    }
    if at < text.len() {
        out.push(piece(&text[at..], Mark::None, colors));
    }
    out
}

/// The row of `Earlier bytes` or `Later bytes`.
fn more_row(label: &'static str, id: &'static str, colors: &Colors) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .w_full()
        .h(metrics::BYTES_ROW_HEIGHT)
        .flex()
        .items_center()
        .px(space::S16)
        .text_color(colors.ink3)
        .font(fonts::ui_font())
        .cursor_pointer()
        .hover(|style| style.bg(colors.field))
        .child(label)
}

impl BytesView {
    /// The rows of the list that are on screen.
    fn render_rows(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let colors = Theme::get(cx).colors;
        let ch = self.char_width.unwrap_or(FALLBACK_CHAR_WIDTH);
        let mut wanted: Vec<u64> = Vec::new();
        let mut shift = None;
        let mut rows = Vec::with_capacity(range.len());
        if let Load::Ready(data) = &mut self.state {
            if !self.shifting {
                shift = data.window.edge(range.start, range.end);
            }
            let selection = data.selection.map(|selection| selection.range());
            let mut hex = String::with_capacity(HEX_ROW_CHARS);
            let mut text = String::with_capacity(BYTES_PER_ROW as usize);
            for index in range.clone() {
                let Some(item) = data.window.item(index) else {
                    continue;
                };
                let offset = match item {
                    Item::Earlier => {
                        rows.push(
                            more_row("Earlier bytes", "bytes-earlier", &colors)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.shift_window(Shift::Earlier, cx);
                                }))
                                .into_any_element(),
                        );
                        continue;
                    }
                    Item::Later => {
                        rows.push(
                            more_row("Later bytes", "bytes-later", &colors)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.shift_window(Shift::Later, cx);
                                }))
                                .into_any_element(),
                        );
                        continue;
                    }
                    Item::Row(offset) => offset,
                };
                let page = bytes::page_of(offset);
                let page_state = data.pages.get(page).cloned();
                let mut offset_text = String::new();
                bytes::write_offset(offset, data.digits, &mut offset_text);
                let row = div()
                    .flex_none()
                    .w_full()
                    .h(metrics::BYTES_ROW_HEIGHT)
                    .flex()
                    .flex_row()
                    .items_center()
                    .px(space::S16)
                    .child(
                        div()
                            .flex_none()
                            .w(ch * (data.digits + GAP_CHARS) as f32)
                            .text_color(colors.ink3)
                            .child(SharedString::from(offset_text)),
                    );
                let row = match page_state {
                    None => {
                        wanted.push(page);
                        row
                    }
                    Some(Page::Failed(_)) => row.child(
                        div()
                            .flex_none()
                            .text_color(colors.ink3)
                            .font(fonts::ui_font())
                            .child("unreadable"),
                    ),
                    Some(Page::Ready(page_bytes)) => {
                        let from = (offset % bytes::PAGE) as usize;
                        let row_bytes = page_bytes.get(from..).unwrap_or(&[]);
                        let row_bytes = &row_bytes[..row_bytes.len().min(BYTES_PER_ROW as usize)];
                        hex.clear();
                        text.clear();
                        bytes::write_hex_row(row_bytes, &mut hex);
                        let mut starts = [0usize; 17];
                        bytes::write_text_row(row_bytes, data.column, &mut text, &mut starts);
                        let marks = bytes::row_marks(
                            offset,
                            row_bytes.len(),
                            selection.as_ref(),
                            &data.touched,
                        );
                        let runs = bytes::mark_runs(&marks[..row_bytes.len()]);
                        let hex_pieces = pieces(
                            &hex,
                            &runs,
                            |columns| {
                                bytes::hex_column_start(columns.start)
                                    ..bytes::hex_column_start(columns.end - 1) + 2
                            },
                            &colors,
                        );
                        let text_pieces = pieces(
                            &text,
                            &runs,
                            |columns| starts[columns.start]..starts[columns.end],
                            &colors,
                        );
                        row.child(
                            div()
                                .flex_none()
                                .w(ch * HEX_ROW_CHARS as f32)
                                .h_full()
                                .flex()
                                .flex_row()
                                .items_center()
                                .children(hex_pieces),
                        )
                        .child(div().flex_none().w(ch * GAP_CHARS as f32))
                        .child(
                            div()
                                .flex_none()
                                .w(ch * BYTES_PER_ROW as f32)
                                .h_full()
                                .flex()
                                .flex_row()
                                .items_center()
                                .children(text_pieces),
                        )
                    }
                };
                rows.push(row.into_any_element());
            }
        }
        if let Some(shift) = shift {
            self.shifting = true;
            cx.defer_in(window, move |this, _, cx| this.shift_window(shift, cx));
        }
        for page in wanted {
            self.request_page(page, cx);
        }
        rows
    }

    /// "Offset   00 01 02 ... 0F   Text" above the grid.
    fn render_head(&self, ch: Pixels, digits: usize, colors: &Colors) -> Div {
        let mut hex = String::new();
        let columns: Vec<u8> = (0..BYTES_PER_ROW as u8).collect();
        bytes::write_hex_row(&columns, &mut hex);
        div()
            .flex_none()
            .w_full()
            .h(metrics::BYTES_ROW_HEIGHT)
            .flex()
            .flex_row()
            .items_center()
            .px(space::S16)
            .whitespace_nowrap()
            .text_color(colors.ink3)
            .border_b_1()
            .border_color(colors.line)
            .child(
                div()
                    .flex_none()
                    .w(ch * (digits + GAP_CHARS) as f32)
                    .child("Offset"),
            )
            .child(
                div()
                    .flex_none()
                    .w(ch * HEX_ROW_CHARS as f32)
                    .child(SharedString::from(hex)),
            )
            .child(div().flex_none().w(ch * GAP_CHARS as f32))
            .child(div().flex_none().child("Text"))
    }

    /// The "Go to byte" field above the grid.
    fn render_go_to(&self, go_to: &GoTo, colors: &Colors, cx: &mut Context<Self>) -> Div {
        let ink = colors.ink;
        let typed = if go_to.text.is_empty() {
            div()
                .text_color(colors.ink3)
                .child("A number, or hex starting with 0x")
        } else {
            div().child(SharedString::from(go_to.text.clone()))
        };
        div()
            .flex_none()
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(space::S8)
            .px(space::S16)
            .py(space::S8)
            .border_b_1()
            .border_color(colors.line)
            .text_size(text::SMALL.size)
            .child(
                div()
                    .flex_none()
                    .text_color(colors.ink2)
                    .child("Go to byte"),
            )
            .child(
                div()
                    .id("bytes-go-to")
                    .key_context(BYTES_GO_TO)
                    .track_focus(&self.go_to_focus)
                    .on_action(cx.listener(|this, _: &actions::GoToConfirm, window, cx| {
                        this.confirm_go_to(window, cx);
                    }))
                    .on_action(cx.listener(|this, _: &actions::GoToClose, window, cx| {
                        this.close_go_to(window, cx);
                    }))
                    .on_action(cx.listener(|_, _: &actions::Ignore, _, _| {}))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        this.go_to_key(event, cx);
                    }))
                    .flex_1()
                    .min_w_0()
                    .h(metrics::QUICK_LOOK_FORK_HEIGHT)
                    .px(space::S8)
                    .flex()
                    .items_center()
                    .rounded(radius::ROW)
                    .bg(colors.field)
                    .border_1()
                    .border_color(transparent_black())
                    .focus(move |style| style.border_color(ink))
                    .child(typed),
            )
            .children(go_to.error.clone().map(|error| {
                div()
                    .flex_none()
                    .text_color(colors.ink2)
                    .child(SharedString::from(error))
            }))
    }

    /// What the selection holds, in words: its place, its value, and the structure fields it
    /// touches.
    fn render_side(&mut self, colors: &Colors) -> gpui::Stateful<Div> {
        let side = div()
            .id("bytes-side")
            .flex_none()
            .w(metrics::BYTES_SIDE_WIDTH)
            .h_full()
            .flex()
            .flex_col()
            .gap(space::S12)
            .p(space::S16)
            .border_l_1()
            .border_color(colors.line)
            .overflow_y_scroll()
            .whitespace_normal()
            .text_size(text::SMALL.size);
        let Load::Ready(data) = &mut self.state else {
            return side;
        };
        let fact = |label: &str, value: String| {
            div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_color(colors.ink3)
                        .child(SharedString::from(label.to_owned())),
                )
                .child(div().child(SharedString::from(value)))
        };
        let mut side = side.child(fact("Stream", format_len(data.len)));
        let Some(selection) = data.selection else {
            return side.child(
                div()
                    .text_color(colors.ink3)
                    .child("Click a byte to see what it holds. Ctrl+G goes to an offset."),
            );
        };
        let range = selection.range();
        side = side.children(
            bytes::selection_facts(&range)
                .into_iter()
                .map(|(label, value)| fact(label, value)),
        );
        let len = range.end - range.start;
        if bytes::has_value(len) {
            // 1, 2, 4 or 8 bytes.
            let mut buf = [0u8; 8];
            let slice = &mut buf[..len as usize];
            if data.pages.copy(range.start, slice) {
                side = side.children(
                    bytes::value_facts(slice)
                        .into_iter()
                        .map(|(label, value)| fact(label, value)),
                );
            }
        }
        for (structure, field) in bytes::fields_at(&data.structures, &range) {
            side = side.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::S2)
                    .child(
                        div()
                            .text_color(colors.ink3)
                            .child(SharedString::from(structure.title)),
                    )
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .child(SharedString::from(field.name)),
                    )
                    .child(div().child(SharedString::from(field.value.clone())))
                    .child(
                        div()
                            .text_color(colors.ink2)
                            .child(SharedString::from(field.note)),
                    ),
            );
        }
        if let Some((page, error)) = data.pages.last_failure() {
            side = side.child(
                div()
                    .text_color(colors.ink2)
                    .child(SharedString::from(format!(
                        "Bytes from {} could not be read: {error}",
                        bytes::number_text(page.saturating_mul(bytes::PAGE))
                    ))),
            );
        }
        side
    }
}

/// The width of one character of the grid's face at its size, from shaping a run of them (the
/// width the text is laid out with, which the font's own advance table does not always match).
fn measure_char(window: &Window) -> Pixels {
    const SAMPLE: &str = "0000000000000000";
    let run = TextRun {
        len: SAMPLE.len(),
        font: fonts::mono_font(),
        color: transparent_black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let line = window.text_system().shape_line(
        SharedString::new_static(SAMPLE),
        metrics::BYTES_TEXT_SIZE,
        &[run],
        None,
    );
    let width = line.width / SAMPLE.len() as f32;
    if width > Pixels::ZERO {
        width
    } else {
        FALLBACK_CHAR_WIDTH
    }
}

/// "1,204 bytes (0x4B4)"
fn format_len(len: u64) -> String {
    if len == 1 {
        "1 byte (0x1)".to_owned()
    } else {
        format!("{} bytes (0x{len:X})", crate::format::grouped(len))
    }
}

impl Focusable for BytesView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for BytesView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.char_width.is_none() {
            self.char_width = Some(measure_char(window));
        }
        self.ensure_stream(cx);
        if self.go_to_pending && matches!(&self.state, Load::Ready(_)) {
            self.go_to_pending = false;
            self.open_go_to(window, cx);
        }
        let colors = Theme::get(cx).colors;
        let message = |text: &str| {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(colors.ink3)
                .child(SharedString::from(text.to_owned()))
        };
        let (count, digits) = match &self.state {
            Load::Idle => return message("Reading the file\u{2026}").into_any_element(),
            Load::Opening => return message("Opening the bytes\u{2026}").into_any_element(),
            Load::Failed(text) => return message(text).into_any_element(),
            Load::Ready(data) if data.len == 0 => {
                return message("This stream is empty.").into_any_element();
            }
            Load::Ready(data) => (data.window.list_len(), data.digits),
        };
        let ch = self.char_width.unwrap_or(FALLBACK_CHAR_WIDTH);
        let ink2 = colors.ink2;

        let list = uniform_list("bytes-grid", count, cx.processor(Self::render_rows))
            .track_scroll(self.scroll.clone())
            .size_full();
        let grid = div()
            .id("bytes-grid-area")
            .key_context(BYTES_GRID)
            .track_focus(&self.focus)
            .on_action(
                cx.listener(|this, _: &actions::Left, _, cx| this.step(Step::Left, false, cx)),
            )
            .on_action(
                cx.listener(|this, _: &actions::Right, _, cx| this.step(Step::Right, false, cx)),
            )
            .on_action(cx.listener(|this, _: &actions::Up, _, cx| this.step(Step::Up, false, cx)))
            .on_action(
                cx.listener(|this, _: &actions::Down, _, cx| this.step(Step::Down, false, cx)),
            )
            .on_action(
                cx.listener(|this, _: &actions::ExtendLeft, _, cx| this.step(Step::Left, true, cx)),
            )
            .on_action(
                cx.listener(|this, _: &actions::ExtendRight, _, cx| {
                    this.step(Step::Right, true, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &actions::ExtendUp, _, cx| this.step(Step::Up, true, cx)),
            )
            .on_action(
                cx.listener(|this, _: &actions::ExtendDown, _, cx| this.step(Step::Down, true, cx)),
            )
            .on_action(cx.listener(|this, _: &actions::PageUp, _, cx| this.page(false, cx)))
            .on_action(cx.listener(|this, _: &actions::PageDown, _, cx| this.page(true, cx)))
            .on_action(cx.listener(|this, _: &actions::Start, _, cx| this.jump(false, cx)))
            .on_action(cx.listener(|this, _: &actions::End, _, cx| this.jump(true, cx)))
            .on_action(cx.listener(|this, _: &actions::Copy, _, cx| this.copy(cx)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.mouse_down(event, window, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                this.mouse_move(event, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.dragging = false),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.dragging = false),
            )
            .relative()
            .flex_1()
            .min_h_0()
            .border_1()
            .border_color(transparent_black())
            .focus(move |style| style.border_color(ink2))
            .child(measure(Rc::clone(&self.grid_bounds)))
            .child(list);
        let go_to = self
            .go_to
            .as_ref()
            .map(|go_to| self.render_go_to(go_to, &colors, cx));
        // The side column gives way when the window is too narrow for the whole grid.
        let grid_need =
            space::S16 * 2.0 + ch * (digits + GAP_CHARS + HEX_ROW_CHARS + GAP_CHARS + 16) as f32;
        let room = window.viewport_size().width - metrics::QUICK_LOOK_WELL_MARGIN * 2.0;
        let side =
            (room >= grid_need + metrics::BYTES_SIDE_WIDTH).then(|| self.render_side(&colors));

        div()
            .size_full()
            .flex()
            .flex_row()
            .font(fonts::mono_font())
            .text_size(metrics::BYTES_TEXT_SIZE)
            .whitespace_nowrap()
            .text_color(colors.ink)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .children(go_to)
                    .child(self.render_head(ch, digits, &colors))
                    .child(grid),
            )
            .children(side.map(|side| side.font(fonts::ui_font())))
            .into_any_element()
    }
}
