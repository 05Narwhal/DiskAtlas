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

//! The Properties window's body (`r10-everyday.md` section 7): a header with the kind's tile, the
//! name and the line under it, then the sections of [`Properties`], each collapsible.
//!
//! Tone and space separate things, never outlines. A section that is `Absent` is not drawn; one
//! that is `Failed` shows its head and the reason; one still loading shows its head and a quiet
//! line. A fact that leads somewhere is a link; right-click a row for "Copy" and "Copy everything".
//! The window frame is `views/aux_window.rs`: Escape and Ctrl+W close it.

use std::collections::HashMap;
use std::f32::consts::FRAC_PI_2;

use gpui::{
    AnyElement, AnyWindowHandle, App, ClickEvent, ClipboardItem, Context, Div, Entity, FocusHandle,
    Focusable, FontWeight, Hsla, IntoElement, MouseButton, MouseDownEvent, Pixels, Point, Render,
    SharedString, Stateful, Subscription, Transformation, Window, div, prelude::*, radians,
    relative, size, svg, transparent_black,
};

use crate::assets::icon;
use crate::components::menu::{MenuItem, MenuSlot};
use crate::keymap::{BUTTON, PROPERTIES, controls, workspace};
use crate::state::checksum::{Checksum, Phase, running_text};
use crate::state::facts::{Fact, SectionId, SectionState};
use crate::state::properties::{Header, Properties, PropertiesEnv, PropertiesTarget};
use crate::theme::{
    Colors, Theme, fonts, metrics,
    scale::{radius, size as sizes, space, text},
};
use crate::views::aux_window::{self, AuxOptions};

gpui::actions!(
    properties,
    [
        /// Put the value of the row the menu was opened on on the clipboard.
        CopyValue,
        /// Put the whole window, as text, on the clipboard.
        CopyEverything,
    ]
);

/// Opens the window for `target`. The registry in `state/properties.rs` calls this.
pub fn open(
    target: PropertiesTarget,
    env: PropertiesEnv,
    cx: &mut App,
) -> anyhow::Result<AnyWindowHandle> {
    let title = Properties::title_of(&target, env.sources.read(cx));
    let options = AuxOptions {
        bounds_key: "window.properties",
        title,
        size: size(metrics::PROPERTIES_WIDTH, metrics::PROPERTIES_HEIGHT),
        min_size: size(
            metrics::PROPERTIES_MIN_WIDTH,
            metrics::PROPERTIES_MIN_HEIGHT,
        ),
        cascade: true,
    };
    aux_window::open(options, cx, move |_, cx| {
        cx.new(|cx| PropertiesView::new(target, env, cx))
    })
}

/// Something in the window the keyboard can stop on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Stop {
    /// The head of a section: Enter and Space fold it.
    Head(SectionId),
    /// The value of a row that leads somewhere.
    Link(SectionId, usize),
    /// The one button a section offers at a time: Calculate, then Cancel.
    Action(SectionId),
}

/// The Properties window's body.
pub struct PropertiesView {
    state: Entity<Properties>,
    /// The body's own handle: the frame gives it the keyboard when the window opens.
    focus: FocusHandle,
    /// Tab stops, made as the rows that need them appear.
    stops: HashMap<Stop, FocusHandle>,
    menu: MenuSlot,
    /// What "Copy" puts on the clipboard for the row the menu was opened on.
    menu_text: Option<SharedString>,
    _subscription: Subscription,
}

impl PropertiesView {
    fn new(target: PropertiesTarget, env: PropertiesEnv, cx: &mut Context<Self>) -> Self {
        let state = cx.new(|cx| Properties::new(target, env, cx));
        let subscription = cx.observe(&state, |_, _, cx| cx.notify());
        Self {
            state,
            focus: cx.focus_handle(),
            stops: HashMap::new(),
            menu: MenuSlot::default(),
            menu_text: None,
            _subscription: subscription,
        }
    }

    /// Opens the menu of a row: Copy (its value) and Copy everything.
    fn open_menu(
        &mut self,
        copy: SharedString,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.menu_text = Some(copy);
        let items = vec![
            MenuItem::action("Copy", CopyValue),
            MenuItem::action("Copy everything", CopyEverything),
        ];
        let focus = self.focus.clone();
        self.menu
            .show(items, at, Some(focus), window, cx, |this, _, _, cx| {
                // `menu_text` stays: a chosen item's action arrives after the menu closed.
                this.menu.clear();
                cx.notify();
            });
    }

    fn copy_value(&mut self, _: &CopyValue, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.menu_text.take() {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
        }
    }

    fn copy_everything(&mut self, _: &CopyEverything, _: &mut Window, cx: &mut Context<Self>) {
        let text = self.state.read(cx).as_text();
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    /// Makes the tab stops the rows now showing need.
    fn make_stops(&mut self, cx: &Context<Self>) {
        let state = self.state.read(cx);
        for (id, section) in state.sections() {
            if matches!(section, SectionState::Absent) {
                continue;
            }
            self.stops
                .entry(Stop::Head(*id))
                .or_insert_with(|| cx.focus_handle().tab_stop(true));
            if matches!(section, SectionState::Checksum(_)) && !state.is_collapsed(*id) {
                self.stops
                    .entry(Stop::Action(*id))
                    .or_insert_with(|| cx.focus_handle().tab_stop(true));
            }
            if let SectionState::Ready(rows) = section
                && !state.is_collapsed(*id)
            {
                for (index, fact) in rows.iter().enumerate() {
                    if fact.goes_to.is_some() {
                        self.stops
                            .entry(Stop::Link(*id, index))
                            .or_insert_with(|| cx.focus_handle().tab_stop(true));
                    }
                }
            }
        }
    }

    // ---- drawing ----

    /// The disclosure chevron: pointing right when folded, down when open.
    fn chevron(open: bool, colors: &Colors) -> gpui::Svg {
        let turn = if open { FRAC_PI_2 } else { 0.0 };
        svg()
            .path(icon::CHEVRON)
            .flex_none()
            .w(metrics::CHEVRON_WIDTH)
            .h(metrics::CHEVRON_HEIGHT)
            .text_color(colors.ink3)
            .with_transformation(Transformation::rotate(radians(turn)))
    }

    /// The glyph tile, the name and the line under it.
    fn render_header(header: &Header, colors: &Colors) -> Div {
        div()
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .gap(space::S16)
            .h(metrics::PROPERTIES_HEADER_HEIGHT)
            .px(space::S16)
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(metrics::DETAILS_KIND_TILE)
                    .rounded(radius::FIELD)
                    .bg(colors.wash(header.glyph))
                    .child(
                        svg()
                            .path(header.glyph.path())
                            .flex_none()
                            .size(metrics::DETAILS_KIND_ICON)
                            .text_color(colors.tint(header.glyph)),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(space::S4)
                    .child(
                        div()
                            .truncate()
                            .text_size(text::HEADING.size)
                            .font_weight(FontWeight::BOLD)
                            .text_color(colors.ink)
                            .child(header.name.clone()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_color(colors.ink2)
                            .child(header.subtitle.clone()),
                    ),
            )
    }

    /// A section head: the chevron and the title; Enter, Space or a click folds the section.
    fn render_head(
        &self,
        id: SectionId,
        open: bool,
        colors: &Colors,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let hover = colors.hover;
        let mut head = div()
            .id(("properties-head", id as usize))
            .key_context(BUTTON)
            .on_action(cx.listener(move |this, _: &controls::Press, _, cx| {
                this.state.update(cx, |state, cx| state.toggle(id, cx));
            }))
            // A keyboard "click" (Enter or Space on key-up) is ignored: both arrive as `Press`.
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                if !event.is_keyboard() {
                    this.state.update(cx, |state, cx| state.toggle(id, cx));
                }
            }))
            .flex()
            .flex_row()
            .items_center()
            .gap(space::S8)
            .rounded(radius::ROW)
            .text_size(text::SMALL_BOLD.size)
            .font_weight(FontWeight::BOLD)
            .text_color(colors.ink3)
            .cursor_pointer()
            .hover(move |style| style.bg(hover))
            .focus(move |style| style.bg(hover));
        if let Some(handle) = self.stops.get(&Stop::Head(id)) {
            head = head.track_focus(handle);
        }
        head.child(Self::chevron(open, colors)).child(id.title())
    }

    /// A line of quiet text under a head.
    fn quiet(text: impl Into<SharedString>, color: Hsla) -> Div {
        div().text_color(color).child(text.into())
    }

    /// One fact: the label column and the value, which is a link when the fact leads somewhere.
    fn render_fact(
        &self,
        id: SectionId,
        index: usize,
        fact: &Fact,
        colors: &Colors,
        cx: &Context<Self>,
    ) -> Div {
        let copy: SharedString = fact.copy.clone().unwrap_or_else(|| fact.value.clone());
        let ink = if fact.running {
            colors.ink3
        } else {
            colors.ink
        };
        let hover = colors.hover;
        let value: AnyElement = match (&fact.goes_to, self.stops.get(&Stop::Link(id, index))) {
            (Some(link), Some(handle)) => {
                let (pressed, clicked) = (link.clone(), link.clone());
                div()
                    .id(("properties-link", id as usize * 1000 + index))
                    .key_context(BUTTON)
                    .track_focus(handle)
                    .on_action(cx.listener(move |this, _: &controls::Press, _, cx| {
                        this.state.update(cx, |state, cx| state.go(&pressed, cx));
                    }))
                    .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                        if !event.is_keyboard() {
                            this.state.update(cx, |state, cx| state.go(&clicked, cx));
                        }
                    }))
                    .flex_1()
                    .min_w_0()
                    .text_color(ink)
                    .cursor_pointer()
                    .hover(|style| style.underline())
                    .focus(move |style| style.bg(hover).underline())
                    .child(fact.value.clone())
                    .into_any_element()
            }
            _ => div()
                .flex_1()
                .min_w_0()
                .text_color(ink)
                .child(fact.value.clone())
                .into_any_element(),
        };
        div()
            .flex()
            .flex_row()
            .gap(metrics::ROW_PADDING)
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    this.open_menu(copy.clone(), event.position, window, cx);
                }),
            )
            .child(
                div()
                    .flex_none()
                    .w(metrics::PROPERTIES_LABEL_WIDTH)
                    .text_color(colors.ink3)
                    .child(fact.label.clone()),
            )
            .child(value)
    }

    /// A bar of `height` whose stretches (fractions of its width, 0 to 1) are drawn in `fill`.
    fn bar(spans: &[(f32, f32)], track: Hsla, fill: Hsla) -> Div {
        div()
            .relative()
            .flex_none()
            .w_full()
            .h(metrics::PROPERTIES_STRIP_HEIGHT)
            .rounded(radius::MARK)
            .overflow_hidden()
            .bg(track)
            .children(spans.iter().map(|&(from, to)| {
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(relative(from.clamp(0.0, 1.0)))
                    .w(relative((to - from).clamp(0.0, 1.0)))
                    .min_w(metrics::STRIP_MARK_MIN_WIDTH)
                    .bg(fill)
            }))
    }

    /// A plain text button, on the `field` tone like the Settings window's.
    fn text_button(
        id: &'static str,
        label: &'static str,
        focus: Option<&FocusHandle>,
        colors: Colors,
    ) -> Stateful<Div> {
        let button = div()
            .id(id)
            .key_context(BUTTON)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .h(sizes::CONTROL)
            .px(space::S12)
            .rounded(radius::FIELD)
            .border_1()
            .border_color(transparent_black())
            .bg(colors.field)
            .text_size(text::BODY.size)
            .text_color(colors.ink)
            .whitespace_nowrap()
            .cursor_pointer()
            .hover(|style| style.opacity(0.85))
            .focus(move |style| style.border_color(colors.ink3));
        match focus {
            Some(handle) => button.track_focus(handle).child(label),
            None => button.child(label),
        }
    }

    /// The Checksum section's body: the button, the progress, or the digest.
    fn render_checksum(&self, checksum: &Checksum, colors: &Colors, cx: &Context<Self>) -> Div {
        let focus = self.stops.get(&Stop::Action(SectionId::Checksum));
        let label = || {
            div()
                .flex_none()
                .w(metrics::PROPERTIES_LABEL_WIDTH)
                .text_color(colors.ink3)
                .child(checksum.label())
        };
        let row = |label: Div, button: Stateful<Div>| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(metrics::ROW_PADDING)
                .child(label)
                .child(button)
        };
        let calculate = || {
            Self::text_button(
                "properties-checksum-start",
                "Calculate SHA-256",
                focus,
                *colors,
            )
            .on_action(cx.listener(|this, _: &controls::Press, _, cx| {
                this.state.update(cx, |state, cx| state.start_checksum(cx));
            }))
            .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                if !event.is_keyboard() {
                    this.state.update(cx, |state, cx| state.start_checksum(cx));
                }
            }))
        };
        let column = div().flex().flex_col().gap(space::S8);
        match &checksum.phase {
            Phase::Idle => column.child(row(label(), calculate())),
            Phase::Failed(reason) => column
                .child(Self::quiet(reason.clone(), colors.ink2))
                .child(row(label(), calculate())),
            Phase::Running { done, total } => {
                let share = if *total == 0 {
                    None
                } else {
                    Some(
                        (u128::from((*done).min(*total)) * 1000 / u128::from(*total)) as f32
                            / 1000.0,
                    )
                };
                let spans: Vec<(f32, f32)> = share.map(|share| (0.0, share)).into_iter().collect();
                let fact = Fact::new(checksum.label(), running_text(*done, *total)).running();
                let cancel =
                    Self::text_button("properties-checksum-cancel", "Cancel", focus, *colors)
                        .on_action(cx.listener(|this, _: &controls::Press, _, cx| {
                            this.state.update(cx, |state, cx| state.cancel_checksum(cx));
                        }))
                        .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                            if !event.is_keyboard() {
                                this.state.update(cx, |state, cx| state.cancel_checksum(cx));
                            }
                        }));
                column
                    .child(self.render_fact(SectionId::Checksum, 0, &fact, colors, cx))
                    .child(Self::bar(&spans, colors.well, colors.ink3))
                    .child(div().flex().flex_row().child(cancel))
            }
            Phase::Done(digest) => column.child(self.render_fact(
                SectionId::Checksum,
                0,
                &Fact::new(checksum.label(), digest.clone()),
                colors,
                cx,
            )),
        }
    }

    /// One section: its head, then what its state says.
    fn render_section(
        &self,
        id: SectionId,
        section: &SectionState,
        colors: &Colors,
        cx: &Context<Self>,
    ) -> Option<Div> {
        let state = self.state.read(cx);
        let open = !state.is_collapsed(id);
        let head = self.render_head(id, open, colors, cx);
        let body = div().flex().flex_col().gap(space::S8).child(head);
        let body = match section {
            SectionState::Absent => return None,
            SectionState::Loading => body.child(Self::quiet("Reading\u{2026}", colors.ink3)),
            SectionState::Failed(reason) => body.child(Self::quiet(reason.clone(), colors.ink2)),
            SectionState::Checksum(_) if !open => body,
            SectionState::Checksum(checksum) => {
                body.child(self.render_checksum(checksum, colors, cx))
            }
            SectionState::Ready(_) if !open => body,
            SectionState::Ready(rows) => {
                let mut body = body;
                // The pieces of a file, on a strip of the volume.
                if id == SectionId::OnDisk
                    && let Some(strip) = state.strip()
                {
                    body = body.child(Self::bar(&strip.spans, colors.well, colors.pin));
                }
                // A volume's used space.
                if id == SectionId::General
                    && let Some((used, total)) = state.usage().filter(|&(_, total)| total > 0)
                {
                    let share = (used as f64 / total as f64) as f32;
                    body = body.child(Self::bar(&[(0.0, share)], colors.well, colors.ink3));
                }
                body.children(
                    rows.iter()
                        .enumerate()
                        .map(|(index, fact)| self.render_fact(id, index, fact, colors, cx)),
                )
            }
        };
        Some(body)
    }
}

impl Focusable for PropertiesView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for PropertiesView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.make_stops(cx);
        let colors = Theme::get(cx).colors;
        let state = self.state.read(cx);
        let sections: Vec<Div> = state
            .sections()
            .iter()
            .filter_map(|(id, section)| self.render_section(*id, section, &colors, cx))
            .collect();
        div()
            .key_context(PROPERTIES)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::copy_value))
            .on_action(cx.listener(Self::copy_everything))
            .on_action(cx.listener(|_, _: &workspace::FocusNext, window, _| window.focus_next()))
            .on_action(cx.listener(|_, _: &workspace::FocusPrev, window, _| window.focus_prev()))
            .size_full()
            .flex()
            .flex_col()
            .bg(colors.sheet)
            .text_color(colors.ink)
            .font(fonts::ui_font())
            .text_size(text::BODY.size)
            .child(Self::render_header(state.header(), &colors))
            .child(
                div()
                    .id("properties-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(space::S24)
                    .px(space::S16)
                    .pb(space::S16)
                    .children(sections),
            )
            .children(self.menu.entity())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stop_is_told_apart_by_its_section_and_row() {
        assert_ne!(
            Stop::Link(SectionId::General, 1),
            Stop::Link(SectionId::General, 2)
        );
        assert_ne!(Stop::Head(SectionId::General), Stop::Head(SectionId::Mac));
    }
}
