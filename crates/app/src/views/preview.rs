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

//! The Quick Look panel: the body of Quick Look's own window (`views/aux_window.rs`) with the
//! Preview, Bytes and Details tabs.
//!
//! The view holds no data of its own beyond focus, scroll positions and two small caches of
//! what it derived from `QuickLook` (the widest text row, the Details rows). Everything that
//! reads a disk lives in `state/quick_look.rs` and runs on the background executor; this file
//! only draws what has arrived. Design: `.claude/docs/design/m5-quicklook-copy.md` §4 and
//! `r1-redesign.md` 6.
//!
//! Keys: the window has no workspace, so the panel's root holds focus and handles every
//! Quick Look action itself: the arrow keys move the selection of the main window's list and
//! the panel follows it, Tab cycles inside the panel ([`QuickLookPanel::cycle_focus`]), and
//! Escape or Space closes the window. The actions are in `keymap.rs`.
//! [`QuickLookWindow`] is how a workspace opens that window and notices it closing.

use std::ops::Range;
use std::sync::Arc;

use disk_atlas_lib::fs::{NodeKind, Stream};
use disk_atlas_lib::search::kind::classify_entry;
use gpui::{
    Animation, AnimationExt, AnyElement, AnyWindowHandle, App, ClickEvent, Context, Div, ElementId,
    Empty, Entity, FocusHandle, Focusable, FontWeight, Hsla, ImageSource, IntoElement,
    ListHorizontalSizingBehavior, MouseButton, ObjectFit, Pixels, Render, SharedString, Stateful,
    StyledImage, Subscription, UniformListScrollHandle, Window, div, ease_out_quint, img, point,
    prelude::*, px, size, svg, transparent_black, uniform_list,
};

use crate::assets::icon;
use crate::components::details::display_line;
use crate::format;
use crate::keymap::{self, QUICK_LOOK, QUICK_LOOK_BUTTON, controls, quick_look as actions};
use crate::preview::{ImagePreview, MAX_TEXT_BYTES, Preview, TextPreview};
use crate::state::facts::{DetailRow, kind_text, tab_rows};
use crate::state::listing::Motion;
use crate::state::quick_look::{LoadState, QuickLook, Tab, tab_after_fork_switch};
use crate::theme::{
    Colors, Glyph, Theme, fonts, metrics, motion,
    scale::{radius, size, space, text},
};
use crate::views::aux_window::{self, AuxOptions};

/// A horizontal key press scrolls the text by this fraction of its width.
const HORIZONTAL_STEP_DIVISOR: f32 = 4.0;

/// The stop after `current` in a ring of `len` focus stops, or before it. `current` is `None`
/// when focus is outside the ring: forward enters at the first stop, backward at the last.
fn cycle(current: Option<usize>, len: usize, forward: bool) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match (current, forward) {
        (None, true) => 0,
        (None, false) => len - 1,
        (Some(at), true) => (at + 1) % len,
        (Some(at), false) => (at + len - 1) % len,
    })
}

/// "1,024 × 768 pixels", "800 × 600 pixels, 12 frames"
fn pixels_text(width: u32, height: u32, frames: usize) -> String {
    let mut text = format!(
        "{} \u{d7} {} pixels",
        format::grouped(u64::from(width)),
        format::grouped(u64::from(height))
    );
    if frames > 1 {
        text.push_str(&format!(", {} frames", format::grouped(frames as u64)));
    }
    text
}

/// The header's second line: "PNG image \u{b7} 1,024 \u{d7} 768 pixels \u{b7} 2.1 MB". Parts that
/// are not known yet are left out.
fn meta_line(kind: &str, pixels: Option<String>, size: Option<u64>) -> String {
    let mut parts = vec![kind.to_owned()];
    parts.extend(pixels);
    parts.extend(size.map(format::size));
    parts.join(" \u{b7} ")
}

/// The line under the header when the text is cut: "Showing the first 256 KB".
fn truncated_note() -> String {
    format!("Showing the first {}", format::size(MAX_TEXT_BYTES as u64))
}

/// The row of `text` with the most characters, tabs counted as the four spaces they are drawn
/// as. Its width sizes the list, so horizontal scrolling reaches the end of the longest line.
fn widest_row(text: &TextPreview) -> usize {
    let mut best = (0, 0);
    for index in 0..text.row_count() {
        let Some(row) = text.row(index) else { continue };
        let tabs = row.bytes().filter(|&byte| byte == b'\t').count();
        let width = row.chars().count() + tabs * 3;
        if width > best.1 {
            best = (index, width);
        }
    }
    best.0
}

/// The Bytes tab. A placeholder until task 5b builds `views/bytes.rs`; that task replaces this
/// one function.
fn bytes_tab(colors: &Colors) -> Div {
    message(
        "The bytes view is not built yet.",
        Some("It will show the file's bytes as hex and text."),
        colors,
    )
}

/// A centred sentence, with an optional line of advice under it.
fn message(text: &str, hint: Option<&str>, colors: &Colors) -> Div {
    div()
        .size_full()
        .flex()
        .flex_col()
        .gap(space::S8)
        .items_center()
        .justify_center()
        .px(space::S16)
        .text_color(colors.ink3)
        .child(SharedString::from(text.to_owned()))
        .children(hint.map(|hint| SharedString::from(hint.to_owned())))
}

/// The Quick Look panel.
pub struct QuickLookPanel {
    quick_look: Entity<QuickLook>,
    /// Tab stops of the header, in order: the three tabs, then the fork switch's two buttons
    /// (while it shows), then the close button.
    tab_focus: [FocusHandle; 3],
    fork_focus: [FocusHandle; 2],
    close_focus: FocusHandle,
    text_scroll: UniformListScrollHandle,
    details_scroll: UniformListScrollHandle,
    /// The text list's widest row, with the generation it was found for.
    widest: Option<(u64, usize)>,
    /// The generation whose text the scroll position belongs to.
    scrolled_for: u64,
    /// The Details tab's rows, with the generation they were built for.
    detail_rows: Option<(u64, Arc<[DetailRow]>)>,
    /// Read once: the fade is not played.
    reduced_motion: bool,
    /// Held by the panel's root, so the `QuickLook` key context is on the focus path while the
    /// window is open (a context on an ancestor that is not focused is not in the stack).
    focus: FocusHandle,
    _subscription: Subscription,
}

impl QuickLookPanel {
    /// The panel over `quick_look`. It draws nothing while that is closed.
    pub fn new(quick_look: Entity<QuickLook>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&quick_look, |_, _, cx| cx.notify());
        let reduced_motion = motion::reduced(cx);
        Self {
            quick_look,
            tab_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            fork_focus: std::array::from_fn(|_| cx.focus_handle().tab_stop(true)),
            close_focus: cx.focus_handle().tab_stop(true),
            text_scroll: UniformListScrollHandle::new(),
            details_scroll: UniformListScrollHandle::new(),
            widest: None,
            scrolled_for: 0,
            detail_rows: None,
            reduced_motion,
            focus: cx.focus_handle(),
            _subscription: subscription,
        }
    }

    /// The panel's focus stops in Tab order. Task 5b adds the bytes view's here.
    fn focus_order(&self, cx: &Context<Self>) -> Vec<FocusHandle> {
        let mut order: Vec<FocusHandle> = self.tab_focus.to_vec();
        if self.quick_look.read(cx).has_resource_fork() {
            order.extend(self.fork_focus.iter().cloned());
        }
        order.push(self.close_focus.clone());
        order
    }

    /// Tab (or Shift+Tab) in the window: moves focus to the next stop of the panel and wraps, so
    /// focus stays inside until Escape.
    pub fn cycle_focus(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let order = self.focus_order(cx);
        let current = order
            .iter()
            .position(|handle| handle.contains_focused(window, cx));
        if let Some(next) = cycle(current, order.len(), forward) {
            window.focus(&order[next]);
        }
    }

    /// Scrolls the text preview by `pages_x` and `pages_y` of its size (a page vertically, a
    /// fraction of the width horizontally; negative is up or left).
    fn scroll_text(&mut self, columns: i32, pages: i32, cx: &mut Context<Self>) {
        let handle = self.text_scroll.0.borrow().base_handle.clone();
        let (offset, max, bounds) = (handle.offset(), handle.max_offset(), handle.bounds());
        let x = offset.x - bounds.size.width / HORIZONTAL_STEP_DIVISOR * columns as f32;
        let y = offset.y - bounds.size.height * pages as f32;
        handle.set_offset(point(
            x.max(-max.width).min(Pixels::ZERO),
            y.max(-max.height).min(Pixels::ZERO),
        ));
        cx.notify();
    }

    fn show_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.quick_look
            .update(cx, |quick_look, cx| quick_look.set_tab(tab, cx));
    }

    /// The fork switch: reloads with the stream, and a resource fork opens on Bytes.
    fn show_fork(&mut self, stream: Stream, window: &mut Window, cx: &mut Context<Self>) {
        self.quick_look.update(cx, |quick_look, cx| {
            quick_look.set_stream(stream, window, cx);
            let tab = tab_after_fork_switch(stream, quick_look.tab());
            quick_look.set_tab(tab, cx);
        });
    }

    /// Closes the window, as Ctrl+W and the frame's close button do. The workspace sees it go,
    /// closes `QuickLook` and gives the keyboard back to the list: one route for every way out.
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        aux_window::close_window(window, cx);
    }

    /// An arrow key pressed in the window moves the selection of the main window's list.
    fn move_selection(&mut self, motion: Motion, cx: &mut Context<Self>) {
        self.quick_look
            .update(cx, |quick_look, cx| quick_look.move_selection(motion, cx));
    }

    /// Ctrl+Tab and Ctrl+Shift+Tab: the next and previous tab.
    fn cycle_tab(&mut self, forward: bool, cx: &mut Context<Self>) {
        self.quick_look.update(cx, |quick_look, cx| {
            let tab = quick_look.tab().cycle(forward);
            quick_look.set_tab(tab, cx);
        });
    }

    /// A header button: a tab, a fork choice or the close button. `active` fills it with `ink`,
    /// the way an active filter is filled; focus lightens the edge so it shows on the fill.
    /// Clicking it never takes keyboard focus (`prevent_default` stops the automatic focus on
    /// mouse-down), so Space and Escape keep reaching whatever had focus.
    fn button<F>(
        id: impl Into<ElementId>,
        focus: &FocusHandle,
        active: bool,
        hover_fill: Hsla,
        colors: &Colors,
        on_press: F,
        cx: &mut Context<Self>,
    ) -> Stateful<Div>
    where
        F: Fn(&mut Self, &mut Window, &mut Context<Self>) + Clone + 'static,
    {
        let (fill, text, edge, edge_focused) = if active {
            (colors.ink, colors.sheet, colors.ink, colors.ink3)
        } else {
            (
                transparent_black(),
                colors.ink2,
                transparent_black(),
                colors.ink,
            )
        };
        let on_click = on_press.clone();
        let pressed_fill = colors.field;
        div()
            .id(id)
            .key_context(QUICK_LOOK_BUTTON)
            .track_focus(focus)
            .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
            .on_action(cx.listener(move |this, _: &controls::Press, window, cx| {
                on_press(this, window, cx);
            }))
            // A keyboard "click" (Enter or Space on key-up) is ignored: Enter arrives as `Press`
            // and Space is the panel's.
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if !event.is_keyboard() {
                    on_click(this, window, cx);
                }
            }))
            .flex()
            .flex_none()
            .flex_row()
            .items_center()
            .justify_center()
            .h(metrics::QUICK_LOOK_SEGMENT_HEIGHT)
            .px(metrics::QUICK_LOOK_SEGMENT_PADDING)
            .rounded(radius::ROW)
            .border_1()
            .border_color(edge)
            .bg(fill)
            .text_color(text)
            .text_size(text::BODY.size)
            .whitespace_nowrap()
            .cursor_pointer()
            .when(!active, move |button| {
                button
                    .hover(move |style| style.bg(hover_fill))
                    .active(move |style| style.bg(pressed_fill))
            })
            .focus(move |style| style.border_color(edge_focused))
    }

    /// The kind tile, name and meta line (with the fork switch), the tabs centred, and the
    /// toolbar at the right. `title` is the name and the meta line.
    fn render_header(
        &self,
        title: (SharedString, SharedString),
        tab: Tab,
        stream: Stream,
        fork_switch: bool,
        colors: &Colors,
        cx: &mut Context<Self>,
    ) -> Div {
        let (name, meta) = title;
        let tabs = [
            (Tab::Preview, "Preview"),
            (Tab::Bytes, "Bytes"),
            (Tab::Details, "Details"),
        ];
        let tab_buttons = tabs
            .into_iter()
            .enumerate()
            .map(|(index, (target, label))| {
                Self::button(
                    ("quick-look-tab", index),
                    &self.tab_focus[index],
                    tab == target,
                    colors.hover,
                    colors,
                    move |this, _, cx| this.show_tab(target, cx),
                    cx,
                )
                .debug_selector(|| format!("quick-look-tab-{index}"))
                .child(label)
            })
            .collect::<Vec<_>>();
        let forks = [(Stream::Data, "Data"), (Stream::Resource, "Resource")];
        let fork_buttons = forks
            .into_iter()
            .enumerate()
            .map(|(index, (target, label))| {
                Self::button(
                    ("quick-look-fork", index),
                    &self.fork_focus[index],
                    stream == target,
                    colors.hover,
                    colors,
                    move |this, window, cx| this.show_fork(target, window, cx),
                    cx,
                )
                .debug_selector(|| format!("quick-look-fork-{index}"))
                .h(metrics::QUICK_LOOK_FORK_HEIGHT)
                .px(space::S8)
                .text_size(text::SMALL.size)
                .child(label)
            })
            .collect::<Vec<_>>();
        let close = Self::button(
            "quick-look-close",
            &self.close_focus,
            false,
            colors.hover,
            colors,
            |this, window, cx| this.close(window, cx),
            cx,
        )
        .debug_selector(|| "quick-look-close".to_owned())
        .w(metrics::QUICK_LOOK_TOOL_WIDTH)
        .h(size::CONTROL)
        .px(Pixels::ZERO)
        .child(
            svg()
                .path(icon::CLOSE)
                .flex_none()
                .size(metrics::ICON_CAPTION)
                .text_color(colors.ink2),
        );
        let glyph = Glyph::from_class(classify_entry(&name, false, None));
        let tile = div()
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .size(metrics::QUICK_LOOK_KIND_TILE)
            .rounded(radius::ROW)
            .bg(colors.wash(glyph))
            .child(
                svg()
                    .path(glyph.path())
                    .flex_none()
                    .size(metrics::QUICK_LOOK_KIND_ICON)
                    .text_color(colors.tint(glyph)),
            );
        let heading = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .truncate()
                    .text_size(text::HEADING.size)
                    .font_weight(FontWeight::BOLD)
                    .child(name),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(space::S8)
                    .text_size(text::SMALL.size)
                    .text_color(colors.ink2)
                    .child(div().min_w_0().truncate().child(meta))
                    .when(fork_switch, |line| {
                        line.child(
                            div()
                                .flex_none()
                                .flex()
                                .flex_row()
                                .gap(space::S4)
                                .children(fork_buttons),
                        )
                    }),
            );
        // The two outer groups share the width equally, which centres the tabs.
        div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(space::S16)
            .h(metrics::QUICK_LOOK_HEADER_HEIGHT)
            .px(space::S16)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(space::S8)
                    .child(tile)
                    .child(heading),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .p(metrics::QUICK_LOOK_SEGMENT_INSET)
                    .rounded(radius::ROW)
                    .bg(colors.field)
                    .children(tab_buttons),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .gap(space::S4)
                    .child(close),
            )
    }

    /// One line of the text preview, for the visible range.
    fn render_lines(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Div> {
        let colors = Theme::get(cx).colors;
        let LoadState::Ready(Preview::Text(text)) = self.quick_look.read(cx).state() else {
            return Vec::new();
        };
        range
            .filter_map(|index| text.row(index))
            .map(|row| {
                div()
                    .h(metrics::QUICK_LOOK_LINE_HEIGHT)
                    .px(space::S16)
                    .whitespace_nowrap()
                    .text_size(text::BODY.size)
                    .text_color(colors.ink)
                    .child(display_line(row))
            })
            .collect()
    }

    /// The Details tab's rows, for the visible range.
    fn render_detail_rows(
        &mut self,
        range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Div> {
        let colors = Theme::get(cx).colors;
        let Some((_, rows)) = &self.detail_rows else {
            return Vec::new();
        };
        let frame = || {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(space::S16)
                .h(metrics::QUICK_LOOK_ROW_HEIGHT)
                .px(space::S16)
                .text_size(text::BODY.size)
        };
        let label = |text: &'static str| {
            div()
                .flex_none()
                .w(metrics::QUICK_LOOK_LABEL_WIDTH)
                .truncate()
                .text_color(colors.ink3)
                .child(text)
        };
        range
            .filter_map(|index| rows.get(index))
            .map(|row| match row {
                DetailRow::Heading(text) => frame()
                    .items_end()
                    .pb(space::S4)
                    .font_weight(FontWeight::BOLD)
                    .child(text.clone()),
                DetailRow::Fact(name, value) => frame()
                    .child(label(name))
                    .child(div().flex_1().min_w_0().truncate().child(value.clone())),
                DetailRow::Columns(left, right) => frame()
                    .text_size(text::SMALL.size)
                    .text_color(colors.ink3)
                    .child(div().flex_1().min_w_0().truncate().child(*left))
                    .child(
                        div()
                            .flex_none()
                            .w(metrics::QUICK_LOOK_LABEL_WIDTH)
                            .child(*right),
                    ),
                DetailRow::Pair(left, right) => frame()
                    .child(div().flex_1().min_w_0().truncate().child(left.clone()))
                    .child(
                        div()
                            .flex_none()
                            .w(metrics::QUICK_LOOK_LABEL_WIDTH)
                            .truncate()
                            .child(right.clone()),
                    ),
                DetailRow::Note(text) => frame()
                    .text_color(colors.ink3)
                    .child(div().min_w_0().truncate().child(text.clone())),
            })
            .collect()
    }

    /// The Preview tab.
    fn preview_tab(&mut self, colors: &Colors, cx: &mut Context<Self>) -> AnyElement {
        let quick_look = self.quick_look.read(cx);
        let state = quick_look.state().clone();
        let generation = quick_look.generation();
        let loading = quick_look.show_loading();
        let stream = quick_look.stream();
        match state {
            LoadState::Idle => {
                message("Select a file to preview it.", None, colors).into_any_element()
            }
            LoadState::Loading if loading => {
                message("Reading this file\u{2026}", None, colors).into_any_element()
            }
            LoadState::Loading => div().size_full().into_any_element(),
            LoadState::Failed(text) => message(
                &text,
                Some("Open the Bytes tab to see what is stored, or select another file."),
                colors,
            )
            .into_any_element(),
            LoadState::Ready(Preview::None) => if stream == Stream::Resource {
                message(
                    "A resource fork has no preview yet. Open the Bytes tab to see its contents.",
                    None,
                    colors,
                )
            } else {
                message(
                    "No preview for this kind of file. Open the Bytes tab to see its contents.",
                    None,
                    colors,
                )
            }
            .into_any_element(),
            LoadState::Ready(Preview::Text(text)) if text.text().is_empty() => {
                message("This file is empty.", None, colors).into_any_element()
            }
            LoadState::Ready(Preview::Text(text)) => self
                .text_tab(&text, generation, colors, cx)
                .into_any_element(),
            LoadState::Ready(Preview::Image(image)) => Self::image_tab(&image).into_any_element(),
        }
    }

    /// Text: the notice when cut, a virtualized list of lines that scrolls sideways, and the
    /// encoding under it.
    fn text_tab(
        &mut self,
        text: &TextPreview,
        generation: u64,
        colors: &Colors,
        cx: &mut Context<Self>,
    ) -> Div {
        let widest = match self.widest {
            Some((held, row)) if held == generation => row,
            _ => {
                let row = widest_row(text);
                self.widest = Some((generation, row));
                row
            }
        };
        // A new file starts at the top left.
        if self.scrolled_for != generation {
            self.scrolled_for = generation;
            self.text_scroll
                .0
                .borrow()
                .base_handle
                .set_offset(point(Pixels::ZERO, Pixels::ZERO));
        }
        let list = uniform_list(
            "quick-look-text",
            text.row_count(),
            cx.processor(Self::render_lines),
        )
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .with_width_from_item(Some(widest))
        .track_scroll(self.text_scroll.clone())
        .size_full();
        let note = |text: String| {
            div()
                .flex_none()
                .px(space::S16)
                .py(space::S4)
                .text_size(text::SMALL.size)
                .text_color(colors.ink3)
                .child(SharedString::from(text))
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .when(text.truncated, |view| view.child(note(truncated_note())))
            .child(div().flex_1().min_h_0().py(space::S4).child(list))
            .child(
                note(text.encoding.to_owned())
                    .border_t_1()
                    .border_color(colors.line),
            )
    }

    /// A picture inside the well's inner margin, fitted and never drawn past 2x its size. Its
    /// size is in the header.
    fn image_tab(image: &ImagePreview) -> Div {
        let limit = metrics::QUICK_LOOK_IMAGE_MAX_SCALE;
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p(metrics::QUICK_LOOK_IMAGE_INSET)
            .child(
                img(ImageSource::Render(Arc::clone(&image.image)))
                    .object_fit(ObjectFit::Contain)
                    .size_full()
                    .max_w(px(image.width as f32 * limit))
                    .max_h(px(image.height as f32 * limit)),
            )
    }

    /// The Details tab: a virtualized list of the rows `components::details` builds.
    fn details_tab(&mut self, colors: &Colors, cx: &mut Context<Self>) -> AnyElement {
        let quick_look = self.quick_look.read(cx);
        let state = quick_look.details().clone();
        let generation = quick_look.generation();
        let loading = quick_look.show_details_loading();
        let name = quick_look
            .subject()
            .map(|subject| subject.name.clone())
            .unwrap_or_default();
        match state {
            LoadState::Idle => {
                message("Select a file to see its details.", None, colors).into_any_element()
            }
            LoadState::Loading if loading => {
                message("Reading details\u{2026}", None, colors).into_any_element()
            }
            LoadState::Loading => div().size_full().into_any_element(),
            LoadState::Failed(text) => {
                message("The details could not be read.", Some(&text), colors).into_any_element()
            }
            LoadState::Ready(details) => {
                let count = match &self.detail_rows {
                    Some((held, rows)) if *held == generation => rows.len(),
                    _ => {
                        let rows: Arc<[DetailRow]> = tab_rows(&name, &details).into();
                        let count = rows.len();
                        self.detail_rows = Some((generation, rows));
                        count
                    }
                };
                uniform_list(
                    "quick-look-details",
                    count,
                    cx.processor(Self::render_detail_rows),
                )
                .track_scroll(self.details_scroll.clone())
                .size_full()
                .py(space::S4)
                .into_any_element()
            }
        }
    }
}

impl QuickLookPanel {
    /// The header's meta line for the file shown.
    fn meta_text(&self, cx: &Context<Self>) -> SharedString {
        let quick_look = self.quick_look.read(cx);
        let Some(subject) = quick_look.subject() else {
            return SharedString::default();
        };
        let kind = kind_text(&subject.name, NodeKind::File, subject.finder_codes);
        let pixels = match quick_look.state() {
            LoadState::Ready(Preview::Image(image)) => {
                Some(pixels_text(image.width, image.height, image.frames))
            }
            _ => None,
        };
        let size = match quick_look.details() {
            LoadState::Ready(details) => details.metadata.size,
            _ => None,
        };
        meta_line(kind, pixels, size).into()
    }
}

impl Focusable for QuickLookPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for QuickLookPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let quick_look = self.quick_look.read(cx);
        if !quick_look.is_open() {
            // The window is closing.
            return Empty.into_any_element();
        }
        let tab = quick_look.tab();
        let stream = quick_look.stream();
        let fork_switch = quick_look.has_resource_fork();
        let name = quick_look.subject().map_or_else(
            || SharedString::from("Quick Look"),
            |subject| subject.name.clone(),
        );
        let meta = self.meta_text(cx);
        let colors = Theme::get(cx).colors;

        let body = match tab {
            Tab::Preview => self.preview_tab(&colors, cx),
            Tab::Bytes => bytes_tab(&colors).into_any_element(),
            Tab::Details => self.details_tab(&colors, cx),
        };
        let panel = div()
            .key_context(QUICK_LOOK)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &actions::Close, window, cx| {
                this.close(window, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::PreviousFile, _, cx| {
                this.move_selection(Motion::Up, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::NextFile, _, cx| {
                this.move_selection(Motion::Down, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::FirstFile, _, cx| {
                this.move_selection(Motion::First, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::LastFile, _, cx| {
                this.move_selection(Motion::Last, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::NextTab, _, cx| this.cycle_tab(true, cx)))
            .on_action(cx.listener(|this, _: &actions::PreviousTab, _, cx| {
                this.cycle_tab(false, cx);
            }))
            // Ctrl+G shows the Bytes tab, where "Go to byte" will live (M5 task 5b builds the
            // prompt).
            .on_action(cx.listener(|this, _: &actions::GoToByte, _, cx| {
                this.show_tab(Tab::Bytes, cx);
            }))
            .on_action(
                cx.listener(|this, _: &keymap::workspace::FocusNext, window, cx| {
                    this.cycle_focus(true, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &keymap::workspace::FocusPrev, window, cx| {
                    this.cycle_focus(false, window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &actions::ScrollPageUp, _, cx| {
                this.scroll_text(0, -1, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::ScrollPageDown, _, cx| {
                this.scroll_text(0, 1, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::ScrollLeft, _, cx| {
                this.scroll_text(-1, 0, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::ScrollRight, _, cx| {
                this.scroll_text(1, 0, cx);
            }))
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .text_color(colors.ink)
            .font(fonts::ui_font())
            .text_size(text::BODY.size)
            .child(self.render_header((name, meta), tab, stream, fork_switch, &colors, cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .mx(metrics::QUICK_LOOK_WELL_MARGIN)
                    .mb(metrics::QUICK_LOOK_WELL_MARGIN)
                    .rounded(radius::SHEET)
                    .bg(colors.well)
                    .overflow_hidden()
                    .child(body),
            );

        // The window opens with a fade (opacity only: the window is the panel's size).
        if self.reduced_motion {
            panel.into_any_element()
        } else {
            panel
                .with_animation(
                    "quick-look-open",
                    Animation::new(motion::QUICK_LOOK_FADE).with_easing(ease_out_quint()),
                    |panel, delta| panel.opacity(delta),
                )
                .into_any_element()
        }
    }
}

/// The Quick Look window of one workspace: opens when `QuickLook` opens, closes when it closes,
/// and tells the workspace when the window went away some other way (the close button, Escape,
/// Ctrl+W, Alt+F4), so it can close `QuickLook` too.
#[derive(Default)]
pub struct QuickLookWindow {
    handle: Option<AnyWindowHandle>,
}

impl QuickLookWindow {
    /// Brings the window in line with `quick_look`; call it when that changes. One window at a
    /// time: a window the user closed is not opened again until [`QuickLookWindow::vanished`]
    /// has been told.
    pub fn sync(&mut self, quick_look: &Entity<QuickLook>, cx: &mut App) {
        let open = quick_look.read(cx).is_open();
        match (open, self.handle) {
            (true, None) => {
                let state = quick_look.clone();
                let options = AuxOptions {
                    bounds_key: "window.quick_look",
                    title: "Quick Look".into(),
                    size: size(
                        metrics::QUICK_LOOK_WINDOW_WIDTH,
                        metrics::QUICK_LOOK_WINDOW_HEIGHT,
                    ),
                    min_size: size(
                        metrics::QUICK_LOOK_MIN_WIDTH,
                        metrics::QUICK_LOOK_MIN_HEIGHT,
                    ),
                    cascade: false,
                };
                match aux_window::open(options, cx, move |_, cx| {
                    cx.new(|cx| QuickLookPanel::new(state, cx))
                }) {
                    Ok(handle) => self.handle = Some(handle),
                    Err(error) => tracing::error!(%error, "could not open the Quick Look window"),
                }
            }
            (false, Some(handle)) => {
                self.handle = None;
                handle
                    .update(cx, |_, window, cx| aux_window::close_window(window, cx))
                    .ok();
            }
            _ => {}
        }
    }

    /// True once when the window has gone by itself. Call it when a window closes; the caller
    /// then closes `QuickLook` through the main window.
    pub fn vanished(&mut self, cx: &App) -> bool {
        match self.handle {
            Some(handle) if !cx.windows().contains(&handle) => {
                self.handle = None;
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_stays_inside_the_panel() {
        // Forward wraps from the last stop to the first, backward the other way.
        assert_eq!(cycle(Some(2), 3, true), Some(0));
        assert_eq!(cycle(Some(0), 3, false), Some(2));
        assert_eq!(cycle(Some(0), 3, true), Some(1));
        assert_eq!(cycle(Some(2), 3, false), Some(1));
        // From outside the ring: in at the first stop, or at the last.
        assert_eq!(cycle(None, 3, true), Some(0));
        assert_eq!(cycle(None, 3, false), Some(2));
        assert_eq!(cycle(None, 0, true), None);
        assert_eq!(cycle(Some(0), 1, true), Some(0));
        assert_eq!(cycle(Some(0), 1, false), Some(0));
    }

    #[test]
    fn picture_sizes_say_pixels_and_frames() {
        assert_eq!(pixels_text(1_024, 768, 1), "1,024 \u{d7} 768 pixels");
        assert_eq!(pixels_text(800, 600, 0), "800 \u{d7} 600 pixels");
        assert_eq!(
            pixels_text(800, 600, 12),
            "800 \u{d7} 600 pixels, 12 frames"
        );
    }

    #[test]
    fn the_note_names_the_cut() {
        assert_eq!(truncated_note(), "Showing the first 256 KB");
    }

    #[test]
    fn the_widest_row_is_found_with_tabs_counted() {
        let text = TextPreview::new(
            "short\nthis is the longest line\n\t\t\t\tx\nend".to_owned(),
            "UTF-8",
            false,
        );
        assert_eq!(widest_row(&text), 1);
        // Four tabs draw as sixteen spaces and an x: 17 columns, still shorter than 24.
        let tabs = TextPreview::new("ab\n\t\t\t\tx\nabcdefghijklmnop".to_owned(), "UTF-8", false);
        assert_eq!(widest_row(&tabs), 1);
        let empty = TextPreview::new(String::new(), "UTF-8", false);
        assert_eq!(widest_row(&empty), 0);
    }

    #[test]
    fn the_meta_line_leaves_out_what_is_not_known() {
        assert_eq!(meta_line("Text file", None, None), "Text file");
        assert_eq!(
            meta_line(
                "PNG image",
                Some("1 \u{d7} 2 pixels".to_owned()),
                Some(2_048)
            ),
            "PNG image \u{b7} 1 \u{d7} 2 pixels \u{b7} 2 KB"
        );
    }
}
