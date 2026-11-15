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

//! The two small pieces of the copy to this computer that float outside the footer line
//! (`m5-quicklook-copy.md` 6): the strip above the footer that asks about resource forks once per
//! session, and the panel behind "Details" that lists what could not be copied.
//!
//! Both read [`Transfers`] and change it only through its methods. Each takes the keyboard when it
//! appears and gives it back when it goes: Escape closes either one.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    Bounds, Context, Corner, Div, Entity, FocusHandle, FontWeight, IntoElement, MouseButton,
    Pixels, Render, SharedString, Stateful, Subscription, Window, anchored, canvas, deferred, div,
    point, prelude::*,
};

use crate::components::sidebar::tooltip;
use crate::keymap::{BUTTON, COPY_DETAILS, COPY_STRIP, controls, transfers as actions};
use crate::state::transfers::Transfers;
use crate::theme::{
    Colors, Theme, metrics,
    scale::{elevation, radius, space, text},
};

/// What the checkbox of the fork question says.
pub const FORKS_LABEL: &str = "Keep resource forks and Finder information";

/// What the checkbox's tooltip adds: where the forks go, and what is not copied.
pub const FORKS_TIP: &str = "Saved beside each file as a ._name file, as the Finder does on a disk without forks. Other extended attributes are not copied.";

/// A button with a line round it that lightens on hover and shows its focus with the ink edge.
/// Never filled: the one filled button of the app is the details pane's "Copy to this computer".
pub fn outline_button(
    id: &'static str,
    label: &'static str,
    focus: &FocusHandle,
    colors: &Colors,
) -> Stateful<Div> {
    let ink = colors.ink;
    div()
        .id(id)
        .key_context(BUTTON)
        .track_focus(focus)
        .flex_none()
        .px(space::S12)
        .h(metrics::QUICK_LOOK_FORK_HEIGHT)
        .flex()
        .items_center()
        .rounded(radius::ROW)
        .border_1()
        .border_color(colors.line)
        .text_color(colors.ink)
        .cursor_pointer()
        .hover(|style| style.bg(colors.hover))
        .focus(move |style| style.border_color(ink))
        .child(label)
}

// ---- the fork question ---------------------------------------------------------------------

/// The strip above the footer: "Keep resource forks and Finder information [ ] [Copy] [Cancel]".
/// It shows while [`Transfers::pending`] holds a selection.
pub struct ForkStrip {
    transfers: Entity<Transfers>,
    checkbox: FocusHandle,
    copy: FocusHandle,
    cancel: FocusHandle,
    /// The strip was showing at the last change, to move the focus once on each side.
    shown: bool,
    /// Where the keyboard was before the strip took it.
    previous: Option<FocusHandle>,
    _subscription: Subscription,
}

impl ForkStrip {
    /// The strip of `transfers`.
    pub fn new(transfers: Entity<Transfers>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe_in(&transfers, window, |this, transfers, window, cx| {
            let showing = transfers.read(cx).pending().is_some();
            if showing && !this.shown {
                this.previous = window.focused(cx);
                window.focus(&this.copy);
            } else if !showing
                && this.shown
                && let Some(previous) = this.previous.take()
                && this.holds_focus(window)
            {
                window.focus(&previous);
            }
            this.shown = showing;
            cx.notify();
        });
        Self {
            transfers,
            checkbox: cx.focus_handle().tab_stop(true),
            copy: cx.focus_handle().tab_stop(true),
            cancel: cx.focus_handle().tab_stop(true),
            shown: false,
            previous: None,
            _subscription: subscription,
        }
    }

    fn holds_focus(&self, window: &Window) -> bool {
        [&self.checkbox, &self.copy, &self.cancel]
            .iter()
            .any(|handle| handle.is_focused(window))
    }

    fn toggle(&mut self, cx: &mut Context<Self>) {
        self.transfers.update(cx, |transfers, cx| {
            let on = transfers
                .pending()
                .is_some_and(|pending| pending.keep_forks);
            transfers.set_pending_forks(!on, cx);
        });
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        self.transfers
            .update(cx, |transfers, cx| transfers.confirm_pending(cx));
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.transfers
            .update(cx, |transfers, cx| transfers.cancel_pending(cx));
    }
}

impl Render for ForkStrip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(keep) = self
            .transfers
            .read(cx)
            .pending()
            .map(|pending| pending.keep_forks)
        else {
            return div().into_any_element();
        };
        let colors = Theme::get(cx).colors;
        let ink = colors.ink;
        let tip = SharedString::new_static(FORKS_TIP);
        let checkbox = div()
            .id("copy-strip-forks")
            .key_context(BUTTON)
            .track_focus(&self.checkbox)
            .on_action(cx.listener(|this, _: &controls::Press, _, cx| this.toggle(cx)))
            .on_click(cx.listener(|this, _, _, cx| this.toggle(cx)))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .size(metrics::MENU_CHECK_WIDTH)
            .rounded(radius::MARK)
            .border_1()
            .border_color(colors.ink2)
            .cursor_pointer()
            .focus(move |style| style.border_color(ink))
            .tooltip(move |_, cx| tooltip(tip.clone(), cx))
            .when(keep, |checkbox| checkbox.child("\u{2713}"));
        let copy = outline_button("copy-strip-copy", "Copy", &self.copy, &colors)
            .on_action(cx.listener(|this, _: &controls::Press, _, cx| this.confirm(cx)))
            .on_click(cx.listener(|this, _, _, cx| this.confirm(cx)));
        let cancel = outline_button("copy-strip-cancel", "Cancel", &self.cancel, &colors)
            .on_action(cx.listener(|this, _: &controls::Press, _, cx| this.cancel(cx)))
            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx)));

        div()
            .id("copy-strip")
            .key_context(COPY_STRIP)
            .on_action(cx.listener(|this, _: &actions::CancelAsk, _, cx| this.cancel(cx)))
            .flex_none()
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(space::S12)
            .pl(metrics::ROW_INSET + metrics::ROW_PADDING)
            .pr(metrics::ROW_PADDING)
            .py(space::S8)
            .border_t_1()
            .border_color(colors.line)
            .bg(colors.well)
            .text_size(text::SMALL.size)
            .text_color(colors.ink)
            .whitespace_nowrap()
            .child(div().min_w_0().truncate().child(FORKS_LABEL))
            .child(checkbox)
            .child(div().flex_1())
            .child(copy)
            .child(cancel)
            .into_any_element()
    }
}

// ---- the details of a copy -----------------------------------------------------------------

/// The panel behind "Details": the counts and the first things that could not be copied. It
/// floats above the footer, at the button that opened it.
pub struct DetailsPanel {
    transfers: Entity<Transfers>,
    /// Where the "Details" button was last drawn; the footer fills it.
    anchor: Rc<Cell<Bounds<Pixels>>>,
    /// The panel's Close button: it takes the keyboard, and Escape and Enter reach the panel's
    /// context from it.
    focus: FocusHandle,
    shown: bool,
    previous: Option<FocusHandle>,
    _subscription: Subscription,
}

impl DetailsPanel {
    /// The panel of `transfers`, placed at `anchor`.
    pub fn new(
        transfers: Entity<Transfers>,
        anchor: Rc<Cell<Bounds<Pixels>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe_in(&transfers, window, |this, transfers, window, cx| {
            let showing = transfers.read(cx).details_open();
            if showing && !this.shown {
                this.previous = window.focused(cx);
                window.focus(&this.focus);
            } else if !showing
                && this.shown
                && let Some(previous) = this.previous.take()
                && this.focus.is_focused(window)
            {
                window.focus(&previous);
            }
            this.shown = showing;
            cx.notify();
        });
        Self {
            transfers,
            anchor,
            focus: cx.focus_handle().tab_stop(true),
            shown: false,
            previous: None,
            _subscription: subscription,
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.transfers
            .update(cx, |transfers, cx| transfers.set_details_open(false, cx));
    }
}

impl Render for DetailsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let transfers = self.transfers.read(cx);
        let details = transfers
            .notice()
            .and_then(|notice| notice.details.clone())
            .filter(|_| transfers.details_open());
        let Some(details) = details else {
            return div();
        };
        let theme = Theme::get(cx);
        let (colors, appearance) = (theme.colors, theme.appearance);
        let close_focus = self.focus.clone();
        let at = self.anchor.get();

        let mut body = div()
            .id("copy-details-body")
            .flex()
            .flex_col()
            .gap(space::S8)
            .max_h(metrics::PROPERTIES_MIN_HEIGHT)
            .overflow_y_scroll()
            .child(
                div()
                    .font_weight(FontWeight::BOLD)
                    .text_size(text::BODY.size)
                    .child(SharedString::from(details.heading)),
            )
            .children(details.lines.into_iter().map(|line| {
                div()
                    .text_color(colors.ink2)
                    .child(SharedString::from(line))
            }));
        if !details.failures.is_empty() {
            body = body.child(
                div()
                    .mt(space::S4)
                    .font_weight(FontWeight::BOLD)
                    .child("Could not be read"),
            );
            body = body.children(details.failures.into_iter().map(|(path, error)| {
                div()
                    .flex()
                    .flex_col()
                    .child(div().child(SharedString::from(path)))
                    .child(
                        div()
                            .text_color(colors.ink3)
                            .child(SharedString::from(error)),
                    )
            }));
        }
        if details.more > 0 {
            body = body.child(
                div()
                    .text_color(colors.ink3)
                    .child(SharedString::from(format!(
                        "and {} more",
                        crate::format::grouped(details.more)
                    ))),
            );
        }

        let panel = div()
            .id("copy-details")
            .key_context(COPY_DETAILS)
            .on_action(cx.listener(|this, _: &actions::CloseDetails, _, cx| this.close(cx)))
            .on_action(cx.listener(|this, _: &controls::Press, _, cx| this.close(cx)))
            .occlude()
            .flex()
            .flex_col()
            .gap(space::S12)
            .w(metrics::PROPERTIES_WIDTH)
            .p(space::S16)
            .rounded(radius::WELL)
            .bg(colors.raised)
            .shadow(elevation::popover(appearance, &colors))
            .text_size(text::SMALL.size)
            .text_color(colors.ink)
            .child(body)
            .child(
                div().flex().flex_row().justify_end().child(
                    outline_button("copy-details-close", "Close", &close_focus, &colors)
                        .on_click(cx.listener(|this, _, _, cx| this.close(cx))),
                ),
            );

        // Above the button that opened it; the window's edge keeps it on screen.
        let position = point(at.left(), at.top() - space::S4);
        let size = window.viewport_size();
        div()
            .child(
                deferred(
                    anchored()
                        .position(point(Pixels::ZERO, Pixels::ZERO))
                        .child(
                            div()
                                .id("copy-details-backdrop")
                                .w(size.width)
                                .h(size.height)
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.close(cx);
                                    }),
                                ),
                        ),
                )
                .with_priority(1),
            )
            .child(
                deferred(
                    anchored()
                        .anchor(Corner::BottomLeft)
                        .position(position)
                        .snap_to_window_with_margin(metrics::MENU_WINDOW_MARGIN)
                        .child(panel),
                )
                .with_priority(2),
            )
    }
}

/// Records where an element is drawn into `cell`: a zero-size-cost canvas that fills its parent.
pub fn measure(cell: Rc<Cell<Bounds<Pixels>>>) -> impl IntoElement {
    canvas(move |bounds, _, _| cell.set(bounds), |_, (), _, _| {})
        .absolute()
        .top_0()
        .left_0()
        .size_full()
}
