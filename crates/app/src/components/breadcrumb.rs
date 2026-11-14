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

//! The path as plain clickable segments: every folder above the current one is a link, the
//! current folder is bold text. When the path is wider than the space, the beginning is clipped
//! and the current folder stays visible.

use std::rc::Rc;

use gpui::{
    AnyElement, App, FontWeight, IntoElement, MouseButton, MouseDownEvent, Pixels, Point,
    RenderOnce, SharedString, Window, div, prelude::*, svg,
};

use crate::assets::icon;
use crate::theme::{Theme, metrics, scale::space};

/// What a click on a segment does: gets the segment's depth.
type SelectHandler = Rc<dyn Fn(usize, &mut Window, &mut App)>;

/// What a right press on a segment does: gets the segment's depth and the pointer's place.
type ContextHandler = Rc<dyn Fn(usize, Point<Pixels>, &mut Window, &mut App)>;

/// The breadcrumb: one segment per folder of the path, root first.
#[derive(IntoElement)]
pub struct Breadcrumb {
    names: Vec<SharedString>,
    on_select: SelectHandler,
    on_context_menu: Option<ContextHandler>,
}

impl Breadcrumb {
    /// A breadcrumb over `names`, root first. `on_select` gets the depth (0 is the root) of the
    /// segment the user clicked. The last segment is the current folder and is not clickable.
    pub fn new(
        names: Vec<SharedString>,
        on_select: impl Fn(usize, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            names,
            on_select: Rc::new(on_select),
            on_context_menu: None,
        }
    }

    /// Called on a right press on any segment, the current folder's included.
    pub fn on_context_menu(
        mut self,
        handler: impl Fn(usize, Point<Pixels>, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_context_menu = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for Breadcrumb {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let colors = Theme::get(cx).colors;
        let last = self.names.len().checked_sub(1);

        // The row is as wide as its segments (it never shrinks) but grows to fill the space when
        // they are narrower, so a short path starts at the left. The clipping box around it
        // aligns its content to the end: when the row is wider than the box, the overflow goes
        // off the start side and is clipped there.
        let mut path = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(space::S4)
            .flex_grow()
            .flex_shrink_0();

        for (depth, name) in self.names.into_iter().enumerate() {
            if depth > 0 {
                path = path.child(
                    svg()
                        .path(icon::CHEVRON)
                        .w(metrics::CHEVRON_WIDTH)
                        .h(metrics::CHEVRON_HEIGHT)
                        .flex_none()
                        .text_color(colors.ink3),
                );
            }
            let context = self.on_context_menu.clone();
            let on_right = move |event: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                if let Some(handler) = &context {
                    cx.stop_propagation();
                    handler(depth, event.position, window, cx);
                }
            };
            let segment: AnyElement = if Some(depth) == last {
                div()
                    .on_mouse_down(MouseButton::Right, on_right)
                    .flex_none()
                    .whitespace_nowrap()
                    .font_weight(FontWeight::BOLD)
                    .text_color(colors.ink)
                    .child(name)
                    .into_any_element()
            } else {
                let on_select = Rc::clone(&self.on_select);
                div()
                    .id(("crumb", depth))
                    .flex_none()
                    .whitespace_nowrap()
                    .text_color(colors.ink3)
                    .cursor_pointer()
                    .hover(|style| style.text_color(colors.ink))
                    // A clickable segment must not count as part of the bar's drag area.
                    .occlude()
                    .on_mouse_down(MouseButton::Right, on_right)
                    .on_click(move |_, window, cx| on_select(depth, window, cx))
                    .child(name)
                    .into_any_element()
            };
            path = path.child(segment);
        }

        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_row()
            .items_center()
            .px(space::S8)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_end()
                    .overflow_hidden()
                    .child(path),
            )
    }
}
