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

//! The search field in the top bar: the app's one text input.
//!
//! It is a one-line editor built the way GPUI's `input` example builds one: an entity that
//! implements [`EntityInputHandler`] (so typing, composition and the platform's text services
//! work) and a custom element that shapes the line, paints the selection and the caret, and
//! registers the input handler. The field only holds text and emits [`SearchFieldEvent`]s; the
//! workspace turns those into [`Search`] calls, and the search runs on a background executor.
//!
//! The editing itself lives in [`LineBuffer`], which knows nothing about GPUI's windows, so it is
//! tested directly. Motion is by `char`, not by grapheme cluster, because the grapheme crate is
//! not a dependency.

use std::ops::Range;
use std::time::Duration;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, DispatchPhase, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable,
    GlobalElementId, InspectorElementId, IntoElement, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, Render, ShapedLine, SharedString,
    Style, Subscription, TextRun, UTF16Selection, UnderlineStyle, Window, div, fill, point,
    prelude::*, relative, size, transparent_black,
};

use crate::components::menu::MenuSlot;
use crate::format::grouped;
use crate::keymap::{SEARCH_FIELD, menu as menu_keys, search_field as actions};
use crate::state::listing::Listing;
use crate::state::menus::{BUILT, MenuState, MenuTarget, build, file_manager_name};
use crate::state::navigation::Navigation;
use crate::state::order::SortSpec;
use crate::state::search::{IndexState, Search, SearchEvent};
use crate::theme::{
    Theme, metrics,
    scale::{radius, size, space, text},
};

/// Most bytes the field holds. A query this long is already far beyond anything searched for,
/// and the bound keeps a huge paste from stalling the line shaping.
const MAX_TEXT_BYTES: usize = 1024;

// ---------------------------------------------------------------------------------------------
// Text offsets
// ---------------------------------------------------------------------------------------------

/// The largest char boundary of `text` that is at or before `offset` (and at most `text.len()`).
fn floor_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// The start of the char before `offset`; 0 at the start.
fn previous_boundary(text: &str, offset: usize) -> usize {
    let offset = floor_boundary(text, offset);
    text[..offset]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

/// The end of the char at `offset`; the text's length at the end.
fn next_boundary(text: &str, offset: usize) -> usize {
    let offset = floor_boundary(text, offset);
    text[offset..]
        .chars()
        .next()
        .map_or(text.len(), |ch| offset + ch.len_utf8())
}

/// The UTF-8 byte offset of UTF-16 code unit `offset`. An offset inside a surrogate pair rounds
/// up to the end of that char; one past the end gives the text's length.
fn offset_from_utf16(text: &str, offset: usize) -> usize {
    let mut utf8_offset = 0;
    let mut utf16_count = 0;
    for ch in text.chars() {
        if utf16_count >= offset {
            break;
        }
        utf16_count += ch.len_utf16();
        utf8_offset += ch.len_utf8();
    }
    utf8_offset
}

/// The UTF-16 code unit offset of UTF-8 byte `offset`.
fn offset_to_utf16(text: &str, offset: usize) -> usize {
    let mut utf16_offset = 0;
    let mut utf8_count = 0;
    for ch in text.chars() {
        if utf8_count >= offset {
            break;
        }
        utf8_count += ch.len_utf8();
        utf16_offset += ch.len_utf16();
    }
    utf16_offset
}

/// `text` without line breaks, cut to at most `room` bytes on a char boundary.
fn sanitize(text: &str, room: usize) -> String {
    let mut out = String::with_capacity(text.len().min(room));
    for ch in text.chars().filter(|ch| !matches!(ch, '\n' | '\r')) {
        if out.len() + ch.len_utf8() > room {
            break;
        }
        out.push(ch);
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The editing state
// ---------------------------------------------------------------------------------------------

/// One line of text with a selection and an IME composition. Every offset is a UTF-8 byte offset
/// on a char boundary; offsets that come from outside are clamped to that before use.
#[derive(Debug, Clone, Default)]
struct LineBuffer {
    text: SharedString,
    /// The selection; empty is a caret.
    selected: Range<usize>,
    /// True when the caret is at the start of a non-empty selection.
    reversed: bool,
    /// The text being composed by an input method.
    marked: Option<Range<usize>>,
}

impl LineBuffer {
    fn cursor(&self) -> usize {
        if self.reversed {
            self.selected.start
        } else {
            self.selected.end
        }
    }

    /// `range` with both ends on char boundaries inside the text, in order.
    fn normalize(&self, range: Range<usize>) -> Range<usize> {
        let a = floor_boundary(&self.text, range.start);
        let b = floor_boundary(&self.text, range.end);
        a.min(b)..a.max(b)
    }

    /// The selected text; empty for a caret.
    fn selected_text(&self) -> &str {
        let range = self.normalize(self.selected.clone());
        self.text.get(range).unwrap_or_default()
    }

    /// Replaces the whole text, with the caret at the end. True when the text changed.
    fn set_text(&mut self, text: &str) -> bool {
        let text = sanitize(text, MAX_TEXT_BYTES);
        let changed = text != self.text;
        let end = text.len();
        self.text = text.into();
        self.selected = end..end;
        self.reversed = false;
        self.marked = None;
        changed
    }

    fn move_to(&mut self, offset: usize) {
        let offset = floor_boundary(&self.text, offset);
        self.selected = offset..offset;
        self.reversed = false;
    }

    /// Moves the caret end of the selection to `offset`, keeping the other end.
    fn select_to(&mut self, offset: usize) {
        let offset = floor_boundary(&self.text, offset);
        if self.reversed {
            self.selected.start = offset;
        } else {
            self.selected.end = offset;
        }
        if self.selected.end < self.selected.start {
            self.reversed = !self.reversed;
            self.selected = self.selected.end..self.selected.start;
        }
    }

    fn select_all(&mut self) {
        self.selected = 0..self.text.len();
        self.reversed = false;
    }

    fn left(&mut self) {
        if self.selected.is_empty() {
            self.move_to(previous_boundary(&self.text, self.cursor()));
        } else {
            self.move_to(self.selected.start);
        }
    }

    fn right(&mut self) {
        if self.selected.is_empty() {
            self.move_to(next_boundary(&self.text, self.selected.end));
        } else {
            self.move_to(self.selected.end);
        }
    }

    fn select_left(&mut self) {
        self.select_to(previous_boundary(&self.text, self.cursor()));
    }

    fn select_right(&mut self) {
        self.select_to(next_boundary(&self.text, self.cursor()));
    }

    fn home(&mut self) {
        self.move_to(0);
    }

    fn end(&mut self) {
        self.move_to(self.text.len());
    }

    fn select_home(&mut self) {
        self.select_to(0);
    }

    fn select_end(&mut self) {
        self.select_to(self.text.len());
    }

    /// Backspace: deletes the selection, or the char before the caret. True when text changed.
    fn delete_backward(&mut self) -> bool {
        if self.selected.is_empty() {
            self.select_to(previous_boundary(&self.text, self.cursor()));
        }
        self.replace_selection("")
    }

    /// Delete: deletes the selection, or the char after the caret. True when text changed.
    fn delete_forward(&mut self) -> bool {
        if self.selected.is_empty() {
            self.select_to(next_boundary(&self.text, self.cursor()));
        }
        self.replace_selection("")
    }

    /// Replaces the selection with `text` (line breaks dropped). True when the text changed.
    fn replace_selection(&mut self, text: &str) -> bool {
        let range = self.selected.clone();
        self.replace(Some(range), text)
    }

    /// Replaces `range` (default: the composition, else the selection) with `new_text`, drops
    /// the composition and puts the caret after the new text. True when the text changed.
    fn replace(&mut self, range: Option<Range<usize>>, new_text: &str) -> bool {
        let (changed, start, inserted) = self.splice(range, new_text);
        let caret = start + inserted.len();
        self.selected = caret..caret;
        self.reversed = false;
        self.marked = None;
        changed
    }

    /// Replaces `range` with `new_text` and marks the new text as the composition. The new
    /// selection is `selected_utf16`, in UTF-16 units relative to the start of `new_text`, or a
    /// caret after it. True when the text changed.
    fn replace_and_mark(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        selected_utf16: Option<Range<usize>>,
    ) -> bool {
        let (changed, start, inserted) = self.splice(range, new_text);
        self.marked = (!inserted.is_empty()).then(|| start..start + inserted.len());
        let selected = match selected_utf16 {
            Some(relative) => {
                let a = start + offset_from_utf16(&inserted, relative.start);
                let b = start + offset_from_utf16(&inserted, relative.end);
                a.min(b)..a.max(b)
            }
            None => {
                let end = start + inserted.len();
                end..end
            }
        };
        self.selected = selected;
        self.reversed = false;
        changed
    }

    /// The shared part of the two replacements: builds the new text. Returns whether it differs,
    /// where the new text starts and what was inserted. Leaves the selection and composition to
    /// the caller.
    fn splice(&mut self, range: Option<Range<usize>>, new_text: &str) -> (bool, usize, String) {
        let range = range
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selected.clone());
        let range = self.normalize(range);
        let room = MAX_TEXT_BYTES.saturating_sub(self.text.len().saturating_sub(range.len()));
        let inserted = sanitize(new_text, room);
        let mut text = String::with_capacity(self.text.len() - range.len() + inserted.len());
        text.push_str(&self.text[..range.start]);
        text.push_str(&inserted);
        text.push_str(&self.text[range.end..]);
        let changed = text != self.text;
        self.text = text.into();
        (changed, range.start, inserted)
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        offset_to_utf16(&self.text, range.start)..offset_to_utf16(&self.text, range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        let a = offset_from_utf16(&self.text, range.start);
        let b = offset_from_utf16(&self.text, range.end);
        a.min(b)..a.max(b)
    }
}

// ---------------------------------------------------------------------------------------------
// The text at the right of the field
// ---------------------------------------------------------------------------------------------

/// What the field says at its right edge.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Status {
    /// The index of the current source is being built.
    Indexing { entries: u64 },
    /// Results are shown.
    Results { count: usize, elapsed: Duration },
    /// The index could not be built.
    Unavailable,
}

/// "1 item", "312,000 items".
fn items_text(count: u64) -> String {
    if count == 1 {
        "1 item".to_string()
    } else {
        format!("{} items", grouped(count))
    }
}

/// "<1 ms" below half a millisecond, else whole milliseconds.
fn elapsed_text(elapsed: Duration) -> String {
    let millis = elapsed.as_secs_f64() * 1e3;
    if millis < 0.5 {
        "<1 ms".to_string()
    } else {
        format!("{} ms", grouped(millis.round() as u64))
    }
}

fn status_text(status: Status) -> String {
    match status {
        Status::Indexing { entries } => format!("Indexing… {}", items_text(entries)),
        Status::Results { count, elapsed } => {
            let results = if count == 1 {
                "1 result".to_string()
            } else {
                format!("{} results", grouped(count as u64))
            };
            format!("{results} · {}", elapsed_text(elapsed))
        }
        Status::Unavailable => "Unavailable".to_string(),
    }
}

/// "Search <name>" for the source shown, "Search" outside any source.
fn placeholder_text(source: Option<&str>) -> String {
    match source {
        Some(name) if !name.is_empty() => format!("Search {name}"),
        _ => "Search".to_string(),
    }
}

// ---------------------------------------------------------------------------------------------
// The entity
// ---------------------------------------------------------------------------------------------

/// What the field tells the workspace.
pub enum SearchFieldEvent {
    /// The text changed, by typing, pasting, cutting, deleting or composing.
    Edited(SharedString),
    /// Escape was pressed in the field.
    Escape,
    /// Down or Enter was pressed: focus moves to the first result.
    ToList,
}

impl EventEmitter<SearchFieldEvent> for SearchField {}

/// The one-line search input of the top bar.
pub struct SearchField {
    buffer: LineBuffer,
    placeholder: SharedString,
    focus_handle: FocusHandle,
    /// The shaped line and its bounds as last painted, for hit-testing and the input handler.
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    /// How far the text is scrolled left to keep the caret in view.
    scroll_x: Pixels,
    is_selecting: bool,
    search: Entity<Search>,
    listing: Entity<Listing>,
    navigation: Entity<Navigation>,
    /// The Cut, Copy, Paste menu, while it is open.
    menu: MenuSlot,
    _subscriptions: Vec<Subscription>,
}

impl SearchField {
    /// Creates the field. It reads `search`, `listing` and `navigation` only to draw its
    /// placeholder and its right-hand status text.
    pub fn new(
        search: Entity<Search>,
        listing: Entity<Listing>,
        navigation: Entity<Navigation>,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = vec![
            cx.subscribe(&search, |_, _, _: &SearchEvent, cx| cx.notify()),
            cx.observe(&listing, |_, _, cx| cx.notify()),
            cx.observe(&navigation, |_, _, cx| cx.notify()),
        ];
        Self {
            buffer: LineBuffer::default(),
            placeholder: SharedString::default(),
            focus_handle: cx.focus_handle().tab_stop(true),
            last_layout: None,
            last_bounds: None,
            scroll_x: Pixels::ZERO,
            is_selecting: false,
            search,
            listing,
            navigation,
            menu: MenuSlot::default(),
            _subscriptions: subscriptions,
        }
    }

    /// Opens the Cut, Copy, Paste menu at `at`. The field keeps the focus, and chosen items go
    /// to it.
    fn open_menu_at(&mut self, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        let target = MenuTarget::TextField {
            has_selection: !self.buffer.selected_text().is_empty(),
        };
        let state = MenuState {
            built: BUILT,
            can_paste: cx
                .read_from_clipboard()
                .and_then(|item| item.text())
                .is_some_and(|text| !text.is_empty()),
            sort: SortSpec::default(),
            file_manager: file_manager_name(),
            show_hidden: false,
            show_system: false,
        };
        let items = build(&target, &state);
        let focus = self.focus_handle.clone();
        self.menu
            .show(items, at, Some(focus), window, cx, |this, _, _, cx| {
                this.menu.clear();
                cx.notify();
            });
    }

    /// The Menu key and Shift+F10: under the field's start.
    fn open_menu(&mut self, _: &menu_keys::Open, window: &mut Window, cx: &mut Context<Self>) {
        let at = self.last_bounds.map_or_else(
            || window.mouse_position(),
            |bounds| bounds.bottom_left() + point(Pixels::ZERO, space::S4),
        );
        self.open_menu_at(at, window, cx);
    }

    /// True when the field holds no text.
    pub fn is_empty(&self) -> bool {
        self.buffer.text.is_empty()
    }

    /// Focuses the field and selects its text (Ctrl+F).
    pub fn focus_and_select_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        self.buffer.select_all();
        cx.notify();
    }

    /// Replaces the text without emitting [`SearchFieldEvent::Edited`].
    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.buffer.set_text(text) {
            self.scroll_x = Pixels::ZERO;
            cx.notify();
        }
    }

    /// Redraws, and tells the workspace when the text changed.
    fn finish(&mut self, changed: bool, cx: &mut Context<Self>) {
        if changed {
            cx.emit(SearchFieldEvent::Edited(self.buffer.text.clone()));
        }
        cx.notify();
    }

    /// What the field says at its right edge now.
    fn status(&self, cx: &App) -> Option<Status> {
        let search = self.search.read(cx);
        let active = search.is_active();
        if active && let Some(IndexState::Building { entries }) = search.current_index_state(cx) {
            return Some(Status::Indexing { entries: *entries });
        }
        if let Some(hits) = self.listing.read(cx).hits() {
            return Some(Status::Results {
                count: hits.len(),
                elapsed: hits.elapsed,
            });
        }
        if active && let Some(IndexState::Failed(_)) = search.current_index_state(cx) {
            return Some(Status::Unavailable);
        }
        None
    }

    /// The name of the source shown, from the first crumb of the current location.
    fn source_name(&self, cx: &App) -> Option<SharedString> {
        let navigation = self.navigation.read(cx);
        let crumb = navigation.current()?.crumbs().first()?;
        Some(crumb.name.clone())
    }

    fn left(&mut self, _: &actions::Left, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.left();
        cx.notify();
    }

    fn right(&mut self, _: &actions::Right, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.right();
        cx.notify();
    }

    fn select_left(&mut self, _: &actions::SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.select_left();
        cx.notify();
    }

    fn select_right(&mut self, _: &actions::SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.select_right();
        cx.notify();
    }

    fn home(&mut self, _: &actions::Home, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.home();
        cx.notify();
    }

    fn end(&mut self, _: &actions::End, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.end();
        cx.notify();
    }

    fn select_home(&mut self, _: &actions::SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.select_home();
        cx.notify();
    }

    fn select_end(&mut self, _: &actions::SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.select_end();
        cx.notify();
    }

    fn select_all(&mut self, _: &actions::SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.buffer.select_all();
        cx.notify();
    }

    fn backspace(&mut self, _: &actions::Backspace, _: &mut Window, cx: &mut Context<Self>) {
        let changed = self.buffer.delete_backward();
        self.finish(changed, cx);
    }

    fn delete(&mut self, _: &actions::Delete, _: &mut Window, cx: &mut Context<Self>) {
        let changed = self.buffer.delete_forward();
        self.finish(changed, cx);
    }

    fn copy(&mut self, _: &actions::Copy, _: &mut Window, cx: &mut Context<Self>) {
        let selected = self.buffer.selected_text();
        if !selected.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(selected.to_string()));
        }
    }

    fn cut(&mut self, _: &actions::Cut, _: &mut Window, cx: &mut Context<Self>) {
        let selected = self.buffer.selected_text();
        if selected.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(selected.to_string()));
        let changed = self.buffer.replace_selection("");
        self.finish(changed, cx);
    }

    fn paste(&mut self, _: &actions::Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let changed = self.buffer.replace_selection(&text);
            self.finish(changed, cx);
        }
    }

    fn escape(&mut self, _: &actions::Escape, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(SearchFieldEvent::Escape);
    }

    fn move_to_list(&mut self, _: &actions::ToList, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(SearchFieldEvent::ToList);
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle);
        self.is_selecting = true;
        let offset = self.index_for_mouse_position(event.position);
        if event.modifiers.shift {
            self.buffer.select_to(offset);
        } else {
            self.buffer.move_to(offset);
        }
        cx.notify();
    }

    /// Extends the selection to the pointer while the button is held (the element registers this
    /// for the whole window, so a drag may leave the field).
    fn drag_to(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if event.pressed_button != Some(MouseButton::Left) {
            self.is_selecting = false;
            return;
        }
        let offset = self.index_for_mouse_position(event.position);
        self.buffer.select_to(offset);
        cx.notify();
    }

    /// The byte offset of the char boundary nearest `position`.
    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.buffer.text.is_empty() {
            return 0;
        }
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if line.text != self.buffer.text {
            return 0;
        }
        let x = position.x - bounds.left() + self.scroll_x;
        floor_boundary(&self.buffer.text, line.closest_index_for_x(x))
    }
}

impl Focusable for SearchField {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for SearchField {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.buffer.range_from_utf16(&range_utf16);
        actual_range.replace(self.buffer.range_to_utf16(&range));
        self.buffer.text.get(range).map(str::to_string)
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.buffer.range_to_utf16(&self.buffer.selected),
            reversed: self.buffer.reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.buffer
            .marked
            .as_ref()
            .map(|range| self.buffer.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.buffer.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16.map(|range| self.buffer.range_from_utf16(&range));
        let changed = self.buffer.replace(range, new_text);
        self.finish(changed, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16.map(|range| self.buffer.range_from_utf16(&range));
        let changed = self
            .buffer
            .replace_and_mark(range, new_text, new_selected_range_utf16);
        self.finish(changed, cx);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let line = self.last_layout.as_ref()?;
        let range = self.buffer.range_from_utf16(&range_utf16);
        let left = bounds.left() - self.scroll_x;
        Some(Bounds::from_corners(
            point(left + line.x_for_index(range.start), bounds.top()),
            point(left + line.x_for_index(range.end), bounds.bottom()),
        ))
    }

    fn character_index_for_point(
        &mut self,
        position: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let local = self.last_bounds?.localize(&position)?;
        let line = self.last_layout.as_ref()?;
        if line.text != self.buffer.text {
            return None;
        }
        let utf8_index = line.index_for_x(local.x + self.scroll_x)?;
        Some(offset_to_utf16(&self.buffer.text, utf8_index))
    }
}

// ---------------------------------------------------------------------------------------------
// The text element
// ---------------------------------------------------------------------------------------------

/// Shapes and paints the field's line and registers the input handler.
struct TextElement {
    field: Entity<SearchField>,
}

/// What `prepaint` measured for `paint`.
struct Prepaint {
    line: ShapedLine,
    selection: Option<PaintQuad>,
    caret: Option<PaintQuad>,
    scroll_x: Pixels,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let colors = Theme::get(cx).colors;
        let field = self.field.read(cx);
        let content = field.buffer.text.clone();
        let selected = field.buffer.selected.clone();
        let cursor = field.buffer.cursor();
        let marked = field.buffer.marked.clone();
        let mut scroll_x = field.scroll_x;

        let showing_placeholder = content.is_empty();
        let (display, color) = if showing_placeholder {
            (field.placeholder.clone(), colors.ink3)
        } else {
            (content, colors.ink)
        };

        let style = window.text_style();
        let run = TextRun {
            len: display.len(),
            font: style.font(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let marked = marked.filter(|marked| {
            !showing_placeholder
                && marked.start <= marked.end
                && marked.end <= display.len()
                && display.is_char_boundary(marked.start)
                && display.is_char_boundary(marked.end)
        });
        let runs = if let Some(marked) = marked {
            vec![
                TextRun {
                    len: marked.start,
                    ..run.clone()
                },
                TextRun {
                    len: marked.end - marked.start,
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: metrics::HAIRLINE,
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: display.len() - marked.end,
                    ..run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![run]
        };

        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(display, font_size, &runs, None);

        // Keep the caret in view when the text is wider than the field.
        let caret_width = metrics::CARET_WIDTH;
        let caret_x = line.x_for_index(cursor);
        let width = bounds.size.width;
        if showing_placeholder {
            scroll_x = Pixels::ZERO;
        } else if caret_x < scroll_x {
            scroll_x = caret_x;
        } else if caret_x + caret_width - scroll_x > width {
            scroll_x = caret_x + caret_width - width;
        }
        let max_scroll = (line.width + caret_width - width).max(Pixels::ZERO);
        scroll_x = scroll_x.clamp(Pixels::ZERO, max_scroll);

        let left = bounds.left() - scroll_x;
        let (selection, caret) = if selected.is_empty() {
            let caret = fill(
                Bounds::new(
                    point(left + caret_x, bounds.top()),
                    size(caret_width, bounds.size.height),
                ),
                colors.ink,
            );
            (None, Some(caret))
        } else {
            let selection = fill(
                Bounds::from_corners(
                    point(left + line.x_for_index(selected.start), bounds.top()),
                    point(left + line.x_for_index(selected.end), bounds.bottom()),
                ),
                // `line` is nearly the field's own color by day; `raised` shows on both palettes.
                colors.raised,
            );
            (Some(selection), None)
        };

        Prepaint {
            line,
            selection,
            caret,
            scroll_x,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.field.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.field.clone()),
            cx,
        );

        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }
        let origin = point(bounds.left() - prepaint.scroll_x, bounds.top());
        if let Err(error) = prepaint
            .line
            .paint(origin, window.line_height(), window, cx)
        {
            tracing::warn!(%error, "the search field could not paint its text");
        }
        if focus_handle.is_focused(window)
            && let Some(caret) = prepaint.caret.take()
        {
            window.paint_quad(caret);
        }

        // A drag that started in the field follows the pointer anywhere in the window.
        if self.field.read(cx).is_selecting {
            let field = self.field.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                if phase == DispatchPhase::Bubble {
                    field.update(cx, |field, cx| field.drag_to(event, cx));
                }
            });
            let field = self.field.clone();
            window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                if phase == DispatchPhase::Bubble {
                    field.update(cx, |field, _| field.is_selecting = false);
                }
            });
        }

        let line = prepaint.line.clone();
        let scroll_x = prepaint.scroll_x;
        self.field.update(cx, |field, _| {
            field.last_layout = Some(line);
            field.last_bounds = Some(bounds);
            field.scroll_x = scroll_x;
        });
    }
}

impl Render for SearchField {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Theme::get(cx).colors;
        let focused = self.focus_handle.is_focused(window);

        let placeholder = placeholder_text(self.source_name(cx).as_ref().map(SharedString::as_str));
        if self.placeholder != placeholder {
            self.placeholder = placeholder.into();
        }
        let status = self.status(cx).map(status_text);

        div()
            .key_context(SEARCH_FIELD)
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::open_menu))
            .on_action(cx.listener(Self::escape))
            .on_action(cx.listener(Self::move_to_list))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.open_menu_at(event.position, window, cx);
                }),
            )
            // Out of the row's flow, so the gap does not open while the menu is shown.
            .children(self.menu.entity().map(|menu| div().absolute().child(menu)))
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .gap(space::S8)
            .w(metrics::SEARCH_FIELD_WIDTH)
            .h(size::CONTROL)
            .px(space::S8)
            .rounded(radius::FIELD)
            .bg(colors.field)
            .when(!focused, |field| {
                field.hover(|style| style.bg(colors.hover))
            })
            // The border is always there so the text does not move when focus arrives.
            .border_1()
            .border_color(if focused {
                colors.ink
            } else {
                transparent_black()
            })
            .text_size(text::BODY.size)
            .text_color(colors.ink)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .items_center()
                    .overflow_hidden()
                    .child(TextElement { field: cx.entity() }),
            )
            .children(status.map(|status| {
                div()
                    .flex_none()
                    .whitespace_nowrap()
                    .text_size(text::SMALL.size)
                    .text_color(colors.ink3)
                    .child(status)
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(text: &str, selected: Range<usize>) -> LineBuffer {
        LineBuffer {
            text: text.to_string().into(),
            selected,
            reversed: false,
            marked: None,
        }
    }

    #[test]
    fn left_and_right_move_by_whole_chars() {
        // é is 2 bytes, 日 and 本 are 3 each, the emoji is 4.
        let text = "é日本🙂";
        let mut b = buffer(text, text.len()..text.len());
        let mut seen = vec![b.cursor()];
        for _ in 0..4 {
            b.left();
            seen.push(b.cursor());
        }
        assert_eq!(seen, [12, 8, 5, 2, 0]);
        b.left();
        assert_eq!(b.cursor(), 0, "left at the start stays");

        let mut seen = vec![b.cursor()];
        for _ in 0..4 {
            b.right();
            seen.push(b.cursor());
        }
        assert_eq!(seen, [0, 2, 5, 8, 12]);
        b.right();
        assert_eq!(b.cursor(), 12, "right at the end stays");
    }

    #[test]
    fn boundaries_snap_from_the_middle_of_a_char() {
        let text = "é日";
        assert_eq!(floor_boundary(text, 1), 0);
        assert_eq!(floor_boundary(text, 4), 2);
        assert_eq!(floor_boundary(text, 99), 5);
        assert_eq!(previous_boundary(text, 5), 2);
        assert_eq!(previous_boundary(text, 4), 0);
        assert_eq!(next_boundary(text, 1), 2);
        assert_eq!(next_boundary(text, 5), 5);
        assert_eq!(previous_boundary(text, 0), 0);
    }

    #[test]
    fn left_and_right_collapse_a_selection_to_its_edge() {
        let mut b = buffer("abcdef", 1..4);
        b.left();
        assert_eq!(b.selected, 1..1);
        let mut b = buffer("abcdef", 1..4);
        b.right();
        assert_eq!(b.selected, 4..4);
    }

    #[test]
    fn shift_motion_extends_and_flips_the_selection() {
        let mut b = buffer("日本語", 3..3);
        b.select_right();
        assert_eq!((b.selected.clone(), b.reversed), (3..6, false));
        b.select_left();
        b.select_left();
        assert_eq!((b.selected.clone(), b.reversed), (0..3, true));
        b.select_end();
        assert_eq!((b.selected.clone(), b.reversed), (3..9, false));
        b.select_home();
        assert_eq!((b.selected.clone(), b.reversed), (0..3, true));
    }

    #[test]
    fn backspace_deletes_a_selection() {
        let mut b = buffer("hello world", 5..11);
        assert!(b.delete_backward());
        assert_eq!(b.text.as_str(), "hello");
        assert_eq!(b.selected, 5..5);
    }

    #[test]
    fn backspace_deletes_one_multibyte_char() {
        let mut b = buffer("a日", 4..4);
        assert!(b.delete_backward());
        assert_eq!(b.text.as_str(), "a");
        assert_eq!(b.selected, 1..1);
    }

    #[test]
    fn backspace_at_the_start_does_nothing() {
        let mut b = buffer("abc", 0..0);
        assert!(!b.delete_backward());
        assert_eq!(b.text.as_str(), "abc");
        assert_eq!(b.selected, 0..0);
    }

    #[test]
    fn delete_at_the_end_does_nothing() {
        let mut b = buffer("abc", 3..3);
        assert!(!b.delete_forward());
        assert_eq!(b.text.as_str(), "abc");
        assert_eq!(b.selected, 3..3);
    }

    #[test]
    fn delete_removes_the_char_after_the_caret() {
        let mut b = buffer("é!", 0..0);
        assert!(b.delete_forward());
        assert_eq!(b.text.as_str(), "!");
        assert_eq!(b.selected, 0..0);
    }

    #[test]
    fn inserting_over_a_selection_replaces_it() {
        let mut b = buffer("hello world", 6..11);
        assert!(b.replace_selection("日本"));
        assert_eq!(b.text.as_str(), "hello 日本");
        assert_eq!(b.selected, 12..12);
    }

    #[test]
    fn inserting_the_same_text_is_not_a_change() {
        let mut b = buffer("abc", 1..2);
        assert!(!b.replace_selection("b"));
        assert_eq!(b.selected, 2..2);
    }

    #[test]
    fn pasted_newlines_are_stripped() {
        let mut b = buffer("ab", 1..1);
        assert!(b.replace_selection("x\r\ny\nz"));
        assert_eq!(b.text.as_str(), "axyzb");
        assert_eq!(b.selected, 4..4);
        assert_eq!(sanitize("\n\r", 10), "");
    }

    #[test]
    fn text_is_capped_on_a_char_boundary() {
        let mut b = LineBuffer::default();
        b.replace_selection(&"日".repeat(1000));
        assert_eq!(b.text.len(), MAX_TEXT_BYTES - MAX_TEXT_BYTES % 3);
        assert!(b.text.is_char_boundary(b.text.len()));
        // Typing at the cap adds nothing.
        let before = b.text.clone();
        assert!(!b.replace_selection("日"));
        assert_eq!(b.text, before);
        // But replacing a selection still can.
        b.select_all();
        assert!(b.replace_selection("x"));
        assert_eq!(b.text.as_str(), "x");
    }

    #[test]
    fn set_text_puts_the_caret_at_the_end_and_reports_changes() {
        let mut b = buffer("old", 0..3);
        assert!(b.set_text("new 日"));
        assert_eq!(b.selected, 7..7);
        assert!(!b.set_text("new 日"));
        assert!(b.set_text(""));
        assert_eq!(b.selected, 0..0);
    }

    #[test]
    fn out_of_range_offsets_are_clamped() {
        let mut b = buffer("é日", 0..0);
        b.move_to(1);
        assert_eq!(b.selected, 0..0, "inside a char rounds down");
        b.move_to(999);
        assert_eq!(b.selected, 5..5);
        assert!(
            b.replace(Some(Range { start: 40, end: 2 }), "x"),
            "a reversed, far range is tamed"
        );
        assert_eq!(b.text.as_str(), "éx");
    }

    #[test]
    fn utf16_offsets_on_multibyte_text() {
        // Bytes:  a=0, é=1..3, 日=3..6, 🙂=6..10 ; UTF-16: a=0, é=1, 日=2, 🙂=3..5
        let text = "aé日🙂";
        for (utf8, utf16) in [(0, 0), (1, 1), (3, 2), (6, 3), (10, 5)] {
            assert_eq!(offset_to_utf16(text, utf8), utf16, "to utf16 {utf8}");
            assert_eq!(offset_from_utf16(text, utf16), utf8, "from utf16 {utf16}");
        }
        // Inside the surrogate pair rounds up to the end of the emoji.
        assert_eq!(offset_from_utf16(text, 4), 10);
        // Past the end clamps to the length.
        assert_eq!(offset_from_utf16(text, 99), 10);
        assert_eq!(offset_to_utf16(text, 99), 5);
    }

    #[test]
    fn utf16_ranges_round_trip() {
        let b = buffer("aé日🙂", 0..0);
        assert_eq!(b.range_to_utf16(&(1..6)), 1..3);
        assert_eq!(b.range_from_utf16(&(1..3)), 1..6);
        // Reversed input is put in order.
        assert_eq!(b.range_from_utf16(&Range { start: 3, end: 1 }), 1..6);
    }

    #[test]
    fn composition_is_marked_and_replaced() {
        let mut b = buffer("ab", 1..1);
        assert!(b.replace_and_mark(None, "にほ", Some(3..6)));
        assert_eq!(b.text.as_str(), "aにほb");
        assert_eq!(b.marked, Some(1..7));
        // The selection is given in UTF-16 units relative to the composed text: 1..2 is 本.
        assert!(b.replace_and_mark(None, "日本", Some(1..2)));
        assert_eq!(b.text.as_str(), "a日本b");
        assert_eq!(b.marked, Some(1..7));
        assert_eq!(b.selected, 4..7);
        // Committing replaces the composition.
        assert!(b.replace(None, "日本語"));
        assert_eq!(b.text.as_str(), "a日本語b");
        assert_eq!(b.marked, None);
        assert_eq!(b.selected, 10..10);
    }

    #[test]
    fn grouped_counts() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1_204), "1,204");
        assert_eq!(grouped(312_000), "312,000");
        assert_eq!(grouped(5_000_000), "5,000,000");
    }

    #[test]
    fn elapsed_is_rounded_milliseconds() {
        assert_eq!(elapsed_text(Duration::ZERO), "<1 ms");
        assert_eq!(elapsed_text(Duration::from_micros(499)), "<1 ms");
        assert_eq!(elapsed_text(Duration::from_micros(500)), "1 ms");
        assert_eq!(elapsed_text(Duration::from_micros(2_400)), "2 ms");
        assert_eq!(elapsed_text(Duration::from_micros(2_600)), "3 ms");
        assert_eq!(elapsed_text(Duration::from_millis(1_500)), "1,500 ms");
    }

    #[test]
    fn status_strings() {
        let results = |count, micros| {
            status_text(Status::Results {
                count,
                elapsed: Duration::from_micros(micros),
            })
        };
        assert_eq!(results(1_204, 3_000), "1,204 results · 3 ms");
        assert_eq!(results(1, 3_000), "1 result · 3 ms");
        assert_eq!(results(0, 100), "0 results · <1 ms");
        assert_eq!(
            status_text(Status::Indexing { entries: 312_000 }),
            "Indexing… 312,000 items"
        );
        assert_eq!(
            status_text(Status::Indexing { entries: 1 }),
            "Indexing… 1 item"
        );
        assert_eq!(status_text(Status::Unavailable), "Unavailable");
    }

    #[test]
    fn placeholder_names_the_source() {
        assert_eq!(
            placeholder_text(Some("Macintosh HD")),
            "Search Macintosh HD"
        );
        assert_eq!(placeholder_text(Some("")), "Search");
        assert_eq!(placeholder_text(None), "Search");
    }
}
