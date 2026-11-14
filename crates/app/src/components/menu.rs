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

//! The one menu component (`r10-everyday.md` 6.1): a list of items that opens at a point, with
//! one level of submenus. The search filter menus use it; the context menus of R11.2 will.
//!
//! The owner builds the [`MenuItem`]s, creates a [`Menu`] entity, draws it as a child of one of
//! its own elements and listens for [`MenuEvent::Closed`] to drop it. A chosen item's action is
//! dispatched to the element that was focused before the menu opened (`return_to`), so a menu
//! item and its key binding are the same code path; the shortcut shown beside an item is read
//! from the keymap for that element, never typed.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    Action, AnimationExt, AnyElement, App, Bounds, Context, Corner, Div, Entity, EventEmitter,
    FocusHandle, KeyDownEvent, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels,
    Point, Render, ScrollWheelEvent, SharedString, Stateful, Subscription, Task, Window, anchored,
    canvas, deferred, div, point, prelude::*, px, svg,
};

use crate::assets::icon;
use crate::components::sidebar::tooltip;
use crate::keymap::{MENU, menu};
use crate::theme::{
    Colors, Glyph, Theme, metrics, motion,
    scale::{elevation, radius, space, text},
};

/// Whether an item can be chosen.
pub enum Enabled {
    Yes,
    /// Greyed; `reason` is its tooltip ("Nothing to paste").
    No {
        reason: SharedString,
    },
}

/// One row of a menu.
pub enum MenuItem {
    Action {
        label: SharedString,
        /// Dispatched to the element that was focused before the menu opened.
        action: Box<dyn Action>,
        glyph: Option<Glyph>,
        /// `Some(true)` draws a check mark; `Some(false)` and `None` draw nothing.
        checked: Option<bool>,
        enabled: Enabled,
        /// The menu stays open after the item is chosen (a toggle in a list of toggles).
        keep_open: bool,
    },
    /// Opens a menu to the right (or left, at the window's edge). One level deep only.
    Submenu {
        label: SharedString,
        glyph: Option<Glyph>,
        items: Vec<MenuItem>,
    },
    Separator,
}

impl MenuItem {
    /// An enabled item that dispatches `action`.
    pub fn action(label: impl Into<SharedString>, action: impl Action) -> Self {
        MenuItem::Action {
            label: label.into(),
            action: Box::new(action),
            glyph: None,
            checked: None,
            enabled: Enabled::Yes,
            keep_open: false,
        }
    }

    /// A submenu.
    pub fn submenu(label: impl Into<SharedString>, items: Vec<MenuItem>) -> Self {
        MenuItem::Submenu {
            label: label.into(),
            glyph: None,
            items,
        }
    }

    /// Puts `glyph` in the glyph column.
    #[allow(
        dead_code,
        reason = "no menu item has a glyph yet; the column is drawn"
    )]
    pub fn glyph(mut self, new: Glyph) -> Self {
        match &mut self {
            MenuItem::Action { glyph, .. } | MenuItem::Submenu { glyph, .. } => *glyph = Some(new),
            MenuItem::Separator => {}
        }
        self
    }

    /// Draws a check mark when `on`.
    pub fn checked(mut self, on: bool) -> Self {
        if let MenuItem::Action { checked, .. } = &mut self {
            *checked = Some(on);
        }
        self
    }

    /// Keeps the menu open when this item is chosen.
    pub fn keep_open(mut self) -> Self {
        if let MenuItem::Action { keep_open, .. } = &mut self {
            *keep_open = true;
        }
        self
    }

    /// Greys the item; `reason` is its tooltip.
    pub fn disabled(mut self, reason: impl Into<SharedString>) -> Self {
        if let MenuItem::Action { enabled, .. } = &mut self {
            *enabled = Enabled::No {
                reason: reason.into(),
            };
        }
        self
    }

    fn label(&self) -> Option<&str> {
        match self {
            MenuItem::Action { label, .. } | MenuItem::Submenu { label, .. } => Some(label),
            MenuItem::Separator => None,
        }
    }

    /// True when the keyboard can stop on the item.
    fn is_selectable(&self) -> bool {
        match self {
            MenuItem::Action { enabled, .. } => matches!(enabled, Enabled::Yes),
            MenuItem::Submenu { .. } => true,
            MenuItem::Separator => false,
        }
    }
}

/// Why a menu closed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CloseReason {
    /// An item was chosen.
    Chosen,
    /// Escape on the top level.
    Escape,
    /// A left click outside; it was not passed on.
    Outside,
    /// A right click outside, at this point of the window; it was not passed on, so the owner may
    /// open the menu of that place.
    OutsideRight(Point<Pixels>),
    /// The wheel turned outside the menu.
    Scroll,
    /// The window lost the focus.
    Blur,
    /// The focus went to something else.
    FocusLost,
    /// Tab: closed, and the focus moved to the next control.
    TabNext,
    /// Shift+Tab: closed, and the focus moved to the previous control.
    TabPrev,
    /// The owner closed it (navigation).
    Owner,
}

/// What a [`Menu`] tells its owner.
pub enum MenuEvent {
    /// The menu is over; drop it.
    Closed(CloseReason),
}

// ---- pure logic ----------------------------------------------------------------------------

/// The first selectable row.
fn first_selectable(selectable: &[bool]) -> Option<usize> {
    selectable.iter().position(|s| *s)
}

/// The last selectable row.
fn last_selectable(selectable: &[bool]) -> Option<usize> {
    selectable.iter().rposition(|s| *s)
}

/// The row one step from `from` in the given direction, skipping rows the keyboard cannot stop
/// on. At the end it wraps to the other end when `wrap`, otherwise it gives `None` (the cursor
/// stays). From no row, forward is the first and backward the last.
fn step(selectable: &[bool], from: Option<usize>, forward: bool, wrap: bool) -> Option<usize> {
    let n = selectable.len();
    let ok = |i: &usize| selectable.get(*i).copied().unwrap_or(false);
    match (from, forward) {
        (None, true) => first_selectable(selectable),
        (None, false) => last_selectable(selectable),
        (Some(i), true) => {
            let i = i.min(n);
            (i + 1..n).find(ok).or_else(|| {
                if wrap {
                    (0..=i.min(n.saturating_sub(1))).find(ok)
                } else {
                    None
                }
            })
        }
        (Some(i), false) => {
            let i = i.min(n);
            (0..i).rev().find(ok).or_else(|| {
                if wrap {
                    (i.min(n)..n).rev().find(ok)
                } else {
                    None
                }
            })
        }
    }
}

/// The next selectable row after `from` (wrapping, `from` last) whose label starts with `typed`,
/// ignoring case.
fn jump_letter(
    labels: &[Option<&str>],
    selectable: &[bool],
    from: Option<usize>,
    typed: char,
) -> Option<usize> {
    let n = labels.len();
    let start = from.map_or(0, |i| i + 1);
    (0..n).map(|k| (start + k) % n.max(1)).find(|&i| {
        selectable.get(i).copied().unwrap_or(false)
            && labels[i]
                .and_then(|label| label.chars().next())
                .is_some_and(|first| first.to_lowercase().eq(typed.to_lowercase()))
    })
}

/// Which side of its parent a submenu opens on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Right,
    Left,
}

/// Right of the parent unless it would pass the window's edge (less `margin`) and the left has
/// more room. All numbers are window pixels.
fn submenu_side(
    parent_left: f32,
    parent_right: f32,
    width: f32,
    window_width: f32,
    margin: f32,
) -> Side {
    let room_right = window_width - margin - parent_right;
    let room_left = parent_left - margin;
    if room_right >= width || room_right >= room_left {
        Side::Right
    } else {
        Side::Left
    }
}

/// Distance from the top of a menu's content to the top of row `index`.
fn item_top(separators: &[bool], index: usize, item: f32, separator: f32) -> f32 {
    separators
        .iter()
        .take(index)
        .map(|is_separator| if *is_separator { separator } else { item })
        .sum()
}

/// A keystroke as shown at the right of an item: "Ctrl+Shift+C", or "⌃⇧C" on macOS.
fn shortcut_text(modifiers: &Modifiers, key: &str, mac: bool) -> String {
    let mut out = String::new();
    let mut part = |mac_symbol: &str, name: &str| {
        if mac {
            out.push_str(mac_symbol);
        } else {
            out.push_str(name);
            out.push('+');
        }
    };
    if modifiers.control {
        part("\u{2303}", "Ctrl");
    }
    if modifiers.alt {
        part("\u{2325}", "Alt");
    }
    if modifiers.shift {
        part("\u{21E7}", "Shift");
    }
    if modifiers.platform {
        part(
            "\u{2318}",
            if cfg!(target_os = "windows") {
                "Win"
            } else {
                "Super"
            },
        );
    }
    out.push_str(&key_label(key));
    out
}

/// The name of a key: letters upper case, the rest as a word.
fn key_label(key: &str) -> String {
    match key {
        "escape" => "Esc".into(),
        "pageup" => "PgUp".into(),
        "pagedown" => "PgDn".into(),
        "backspace" => "Backspace".into(),
        _ => {
            let mut chars = key.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        }
    }
}

/// The text at the right of each item of `items`, from the keymap as it applies to `target`
/// (the element the action will be dispatched to).
fn shortcuts_for(
    items: &[MenuItem],
    target: Option<&FocusHandle>,
    window: &Window,
) -> Vec<Option<SharedString>> {
    items
        .iter()
        .map(|item| {
            let MenuItem::Action { action, .. } = item else {
                return None;
            };
            let bindings = match target {
                Some(target) => window.bindings_for_action_in(action.as_ref(), target),
                None => window.bindings_for_action(action.as_ref()),
            };
            let binding = bindings.last()?;
            let text = binding
                .keystrokes()
                .iter()
                .map(|k| shortcut_text(k.modifiers(), k.key(), cfg!(target_os = "macos")))
                .collect::<Vec<_>>()
                .join(" ");
            Some(SharedString::from(text))
        })
        .collect()
}

// ---- the slot an owner keeps its menu in ------------------------------------------------

/// The context menu an owner has open, if any. The owner draws [`MenuSlot::entity`] as a child of
/// one of its elements and clears the slot when the menu closes.
#[derive(Default)]
pub struct MenuSlot {
    open: Option<(Entity<Menu>, Subscription)>,
}

impl MenuSlot {
    /// The menu to draw.
    pub fn entity(&self) -> Option<Entity<Menu>> {
        self.open.as_ref().map(|(menu, _)| menu.clone())
    }

    /// Forgets the menu. Call it from the `on_closed` of [`MenuSlot::show`].
    pub fn clear(&mut self) {
        self.open = None;
    }

    /// Closes the menu from outside (navigation) and forgets it.
    pub fn dismiss(&mut self, window: &mut Window, cx: &mut App) {
        if let Some((menu, _)) = self.open.take() {
            menu.update(cx, |menu, cx| menu.dismiss(window, cx));
        }
    }

    /// Opens a menu of `items` at `position` (replacing one that is open). `return_to` is the
    /// element chosen actions go to and the focus goes back to. `on_closed` hears why it closed
    /// and must call [`MenuSlot::clear`]. A right click outside closes the menu and still reaches
    /// what lies under the pointer (the backdrop does not block it), so the menu of that place
    /// opens in its owner.
    pub fn show<T: 'static>(
        &mut self,
        items: Vec<MenuItem>,
        position: Point<Pixels>,
        return_to: Option<FocusHandle>,
        window: &mut Window,
        cx: &mut Context<T>,
        on_closed: impl Fn(&mut T, CloseReason, &mut Window, &mut Context<T>) + 'static,
    ) {
        let menu = cx.new(|cx| Menu::new(items, position, return_to, window, cx));
        let subscription = cx.subscribe_in(
            &menu,
            window,
            move |this, _, event: &MenuEvent, window, cx| {
                let MenuEvent::Closed(reason) = event;
                on_closed(this, *reason, window, cx);
            },
        );
        self.open = Some((menu, subscription));
        cx.notify();
    }
}

// ---- the view ------------------------------------------------------------------------------

/// An open menu. See the module documentation.
pub struct Menu {
    items: Vec<MenuItem>,
    /// The shortcut text of each top-level item, and of each item of each submenu.
    shortcuts: Vec<Option<SharedString>>,
    sub_shortcuts: Vec<Vec<Option<SharedString>>>,
    position: Point<Pixels>,
    return_to: Option<FocusHandle>,
    focus: FocusHandle,
    /// The highlighted top-level row.
    cursor: Option<usize>,
    /// The top-level row whose submenu is open.
    open_sub: Option<usize>,
    /// The highlighted row of the open submenu.
    sub_cursor: Option<usize>,
    /// True while the keyboard works in the submenu.
    in_sub: bool,
    wrap: bool,
    reduced_motion: bool,
    closed: bool,
    /// Where the top-level panel was last drawn, for placing a submenu.
    bounds: Rc<Cell<Bounds<Pixels>>>,
    rest: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<MenuEvent> for Menu {}

impl Menu {
    /// A menu of `items` opening at `position` (window pixels, its top-left corner). Takes the
    /// focus; gives it back to `return_to` when it closes, and dispatches chosen actions there.
    pub fn new(
        items: Vec<MenuItem>,
        position: Point<Pixels>,
        return_to: Option<FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let return_to = return_to.or_else(|| window.focused(cx));
        let focus = cx.focus_handle();
        let subscriptions = vec![
            // Focus moving anywhere else (Ctrl+F, a click that reached another window) ends the
            // menu without taking the focus back.
            cx.on_focus_out(&focus, window, |this, _, window, cx| {
                this.close(CloseReason::FocusLost, window, cx);
            }),
            cx.observe_window_activation(window, |this, window, cx| {
                if !window.is_window_active() {
                    this.close(CloseReason::Blur, window, cx);
                }
            }),
        ];
        let mut menu = Self {
            items: Vec::new(),
            shortcuts: Vec::new(),
            sub_shortcuts: Vec::new(),
            position,
            return_to,
            focus,
            cursor: None,
            open_sub: None,
            sub_cursor: None,
            in_sub: false,
            wrap: true,
            reduced_motion: motion::reduced(cx),
            closed: false,
            bounds: Rc::new(Cell::new(Bounds::default())),
            rest: None,
            _subscriptions: subscriptions,
        };
        menu.install(items, window);
        window.focus(&menu.focus);
        menu
    }

    /// Whether Up on the first row goes to the last (and Down the other way). Default true.
    pub fn wrap(mut self, wrap: bool) -> Self {
        self.wrap = wrap;
        self
    }

    /// The row highlighted when the menu opens.
    pub fn cursor(mut self, cursor: Option<usize>) -> Self {
        self.cursor = cursor;
        self
    }

    /// Replaces the items (a toggle was chosen and the checks changed); the cursor stays.
    pub fn set_items(&mut self, items: Vec<MenuItem>, window: &Window, cx: &mut Context<Self>) {
        self.install(items, window);
        let n = self.items.len();
        self.cursor = self.cursor.filter(|i| *i < n);
        if self.open_sub.is_some_and(|i| i >= n) {
            self.open_sub = None;
            self.sub_cursor = None;
            self.in_sub = false;
        }
        cx.notify();
    }

    fn install(&mut self, items: Vec<MenuItem>, window: &Window) {
        let target = self.return_to.as_ref();
        self.shortcuts = shortcuts_for(&items, target, window);
        self.sub_shortcuts = items
            .iter()
            .map(|item| match item {
                MenuItem::Submenu { items, .. } => shortcuts_for(items, target, window),
                _ => Vec::new(),
            })
            .collect();
        self.items = items;
    }

    /// Closes the menu from outside (navigation).
    pub fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close(CloseReason::Owner, window, cx);
    }

    fn close(&mut self, reason: CloseReason, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.rest = None;
        let gives_focus_back = !matches!(reason, CloseReason::Blur | CloseReason::FocusLost);
        if gives_focus_back && let Some(target) = &self.return_to {
            window.focus(target);
        }
        match reason {
            CloseReason::TabNext => window.focus_next(),
            CloseReason::TabPrev => window.focus_prev(),
            _ => {}
        }
        cx.emit(MenuEvent::Closed(reason));
        cx.notify();
    }

    /// The items the keyboard is working in.
    fn level(&self, in_sub: bool) -> &[MenuItem] {
        if in_sub
            && let Some(MenuItem::Submenu { items, .. }) =
                self.open_sub.and_then(|i| self.items.get(i))
        {
            return items;
        }
        &self.items
    }

    fn selectable(items: &[MenuItem]) -> Vec<bool> {
        items.iter().map(MenuItem::is_selectable).collect()
    }

    fn current(&self) -> Option<usize> {
        if self.in_sub {
            self.sub_cursor
        } else {
            self.cursor
        }
    }

    fn set_current(&mut self, index: Option<usize>) {
        if self.in_sub {
            self.sub_cursor = index;
        } else {
            self.cursor = index;
        }
    }

    fn move_cursor(&mut self, forward: bool, cx: &mut Context<Self>) {
        let selectable = Self::selectable(self.level(self.in_sub));
        if let Some(next) = step(&selectable, self.current(), forward, self.wrap) {
            self.set_current(Some(next));
            cx.notify();
        }
    }

    fn move_to_end(&mut self, last: bool, cx: &mut Context<Self>) {
        let selectable = Self::selectable(self.level(self.in_sub));
        let target = if last {
            last_selectable(&selectable)
        } else {
            first_selectable(&selectable)
        };
        if target.is_some() {
            self.set_current(target);
            cx.notify();
        }
    }

    /// Opens the submenu of top-level row `index`; with `keyboard` the keyboard goes into it.
    fn open_submenu(&mut self, index: usize, keyboard: bool, cx: &mut Context<Self>) {
        let Some(MenuItem::Submenu { items, .. }) = self.items.get(index) else {
            return;
        };
        let first = first_selectable(&Self::selectable(items));
        self.rest = None;
        self.cursor = Some(index);
        self.open_sub = Some(index);
        if keyboard {
            self.in_sub = true;
            self.sub_cursor = first;
        }
        cx.notify();
    }

    fn close_submenu(&mut self, cx: &mut Context<Self>) {
        self.open_sub = None;
        self.sub_cursor = None;
        self.in_sub = false;
        cx.notify();
    }

    /// Right: into the submenu of the row under the cursor.
    fn on_right(&mut self, cx: &mut Context<Self>) {
        if !self.in_sub
            && let Some(index) = self.cursor
        {
            self.open_submenu(index, true, cx);
        }
    }

    /// Left: out of an open submenu.
    fn on_left(&mut self, cx: &mut Context<Self>) {
        if self.open_sub.is_some() {
            self.close_submenu(cx);
        }
    }

    /// Escape: closes one level.
    fn on_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_sub.is_some() {
            self.close_submenu(cx);
        } else {
            self.close(CloseReason::Escape, window, cx);
        }
    }

    /// Enter or Space: chooses the row under the cursor.
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.current() {
            self.choose(self.in_sub, index, window, cx);
        }
    }

    /// Chooses row `index` of the top level or of the open submenu.
    fn choose(&mut self, in_sub: bool, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (action, keep_open) = match self.level(in_sub).get(index) {
            Some(MenuItem::Action {
                action,
                enabled: Enabled::Yes,
                keep_open,
                ..
            }) => (action.boxed_clone(), *keep_open),
            Some(MenuItem::Submenu { .. }) if !in_sub => {
                self.open_submenu(index, true, cx);
                return;
            }
            _ => return,
        };
        if !keep_open {
            self.close(CloseReason::Chosen, window, cx);
        }
        // After the current update: the action's handler may touch the menu's owner.
        let target = self.return_to.clone();
        window.defer(cx, move |window, cx| match target {
            Some(target) => target.dispatch_action(action.as_ref(), window, cx),
            None => window.dispatch_action(action, cx),
        });
    }

    /// The pointer moved over top-level row `index`.
    fn hover_top(&mut self, index: usize, cx: &mut Context<Self>) {
        let selectable = self.items.get(index).is_some_and(MenuItem::is_selectable);
        if !selectable {
            if self.cursor.is_some() || self.in_sub {
                self.cursor = None;
                self.in_sub = false;
                self.rest = None;
                cx.notify();
            }
            return;
        }
        if self.cursor == Some(index) && !self.in_sub {
            return;
        }
        self.cursor = Some(index);
        self.in_sub = false;
        if self.open_sub != Some(index) {
            self.open_sub = None;
            self.sub_cursor = None;
            self.rest = None;
            if matches!(self.items.get(index), Some(MenuItem::Submenu { .. })) {
                self.rest = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(motion::SUBMENU_REST).await;
                    this.update(cx, |this, cx| {
                        if this.cursor == Some(index) && this.open_sub.is_none() {
                            this.open_submenu(index, false, cx);
                        }
                    })
                    .ok();
                }));
            }
        }
        cx.notify();
    }

    /// The pointer moved over row `index` of the open submenu.
    fn hover_sub(&mut self, index: usize, cx: &mut Context<Self>) {
        let selectable = self
            .level(true)
            .get(index)
            .is_some_and(MenuItem::is_selectable);
        let next = selectable.then_some(index);
        if self.in_sub && self.sub_cursor == next {
            return;
        }
        self.in_sub = true;
        self.sub_cursor = next;
        cx.notify();
    }

    /// A letter jumps to the next row that starts with it.
    fn type_letter(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let m = &event.keystroke.modifiers;
        if m.control || m.alt || m.platform || m.function {
            return;
        }
        let mut chars = event.keystroke.key_char.as_deref().unwrap_or("").chars();
        let (Some(typed), None) = (chars.next(), chars.next()) else {
            return;
        };
        if !typed.is_alphanumeric() {
            return;
        }
        let items = self.level(self.in_sub);
        let labels: Vec<Option<&str>> = items.iter().map(MenuItem::label).collect();
        let selectable = Self::selectable(items);
        if let Some(found) = jump_letter(&labels, &selectable, self.current(), typed) {
            self.set_current(Some(found));
            cx.stop_propagation();
            cx.notify();
        }
    }

    // ---- drawing ----

    fn render_row(
        &self,
        (in_sub, index): (bool, usize),
        item: &MenuItem,
        shortcut: Option<&SharedString>,
        highlighted: bool,
        colors: &Colors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (label, glyph, checked, enabled) = match item {
            MenuItem::Separator => {
                return div()
                    .my(space::S4)
                    .h(metrics::HAIRLINE)
                    .bg(colors.line)
                    .into_any_element();
            }
            MenuItem::Action {
                label,
                glyph,
                checked,
                enabled,
                ..
            } => (label, glyph, *checked == Some(true), Some(enabled)),
            MenuItem::Submenu { label, glyph, .. } => (label, glyph, false, None),
        };
        let usable = !matches!(enabled, Some(Enabled::No { .. }));
        let id = (if in_sub { "menu-sub-item" } else { "menu-item" }, index);

        let mut mark = div()
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .w(metrics::MENU_CHECK_WIDTH)
            .h(metrics::MENU_CHECK_WIDTH);
        if checked {
            mark = mark.child("\u{2713}");
        } else if let Some(glyph) = glyph {
            mark = mark.child(
                svg()
                    .path(glyph.path())
                    .size(metrics::MENU_CHECK_WIDTH)
                    .text_color(colors.ink2),
            );
        }

        let mut row: Stateful<Div> = div()
            .id(id)
            .flex()
            .flex_row()
            .items_center()
            .gap(space::S8)
            .h(metrics::MENU_ITEM_HEIGHT)
            .px(space::S8)
            .rounded(radius::ROW)
            .whitespace_nowrap()
            .child(mark)
            .child(div().flex_1().min_w_0().truncate().child(label.clone()));
        if let Some(shortcut) = shortcut {
            row = row.child(
                div()
                    .flex_none()
                    .text_color(colors.ink3)
                    .child(shortcut.clone()),
            );
        }
        if matches!(item, MenuItem::Submenu { .. }) {
            row = row.child(
                svg()
                    .path(icon::CHEVRON)
                    .flex_none()
                    .w(metrics::CHEVRON_WIDTH)
                    .h(metrics::CHEVRON_HEIGHT)
                    .text_color(colors.ink2),
            );
        }

        if usable {
            row = row
                .cursor_pointer()
                // `hover` is the same shade as `raised` at night, so the row under the cursor
                // would not show: `field` is darker than the panel in both themes.
                .when(highlighted, |row| row.bg(colors.field))
                .active(|style| style.bg(colors.line))
                .on_mouse_move(cx.listener(move |this, _: &MouseMoveEvent, _, cx| {
                    if in_sub {
                        this.hover_sub(index, cx);
                    } else {
                        this.hover_top(index, cx);
                    }
                }))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.choose(in_sub, index, window, cx);
                }));
        } else {
            row = row.text_color(colors.ink3).on_mouse_move(cx.listener(
                move |this, _: &MouseMoveEvent, _, cx| {
                    if in_sub {
                        this.hover_sub(index, cx);
                    } else {
                        this.hover_top(index, cx);
                    }
                },
            ));
            if let Some(Enabled::No { reason }) = enabled {
                let reason = reason.clone();
                row = row.tooltip(move |_, cx| tooltip(reason.clone(), cx));
            }
        }
        row.into_any_element()
    }

    /// One panel: the card with its rows.
    fn render_panel(
        &self,
        in_sub: bool,
        items: &[MenuItem],
        shortcuts: &[Option<SharedString>],
        cursor: Option<usize>,
        colors: &Colors,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let appearance = Theme::get(cx).appearance;
        let rows: Vec<AnyElement> = items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let shortcut = shortcuts.get(index).and_then(Option::as_ref);
                let lit = cursor == Some(index);
                self.render_row((in_sub, index), item, shortcut, lit, colors, cx)
            })
            .collect();
        div()
            .id(if in_sub { "menu-sub" } else { "menu" })
            .occlude()
            .min_w(metrics::MENU_MIN_WIDTH)
            .max_w(metrics::MENU_MAX_WIDTH)
            .flex()
            .flex_col()
            .p(space::S4)
            .bg(colors.raised)
            .shadow(elevation::popover(appearance, colors))
            .rounded(radius::WELL)
            .text_color(colors.ink)
            .text_size(text::SMALL.size)
            .children(rows)
    }

    /// Fades `panel` in with a rise, or only fades it under reduced motion.
    fn entrance(&self, panel: Stateful<Div>, id: &'static str) -> AnyElement {
        if self.reduced_motion {
            return panel
                .with_animation(id, motion::QUICK.animation(), |panel, delta| {
                    panel.opacity(delta)
                })
                .into_any_element();
        }
        panel
            .relative()
            .with_animation(id, motion::QUICK.animation(), |panel, delta| {
                panel.opacity(delta).top(metrics::MENU_RISE * (1.0 - delta))
            })
            .into_any_element()
    }

    /// A transparent cover over the whole window under the menu: a click anywhere else closes
    /// the menu and does nothing more.
    fn render_backdrop(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let size = window.viewport_size();
        deferred(
            anchored()
                .position(point(Pixels::ZERO, Pixels::ZERO))
                .child(
                    div()
                        .id("menu-backdrop")
                        .w(size.width)
                        .h(size.height)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                // Not passed on: the click that closes a menu does nothing else.
                                cx.stop_propagation();
                                this.close(CloseReason::Outside, window, cx);
                            }),
                        )
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(|this, event: &MouseDownEvent, window, cx| {
                                this.close(CloseReason::OutsideRight(event.position), window, cx);
                            }),
                        )
                        .on_scroll_wheel(cx.listener(|this, _: &ScrollWheelEvent, window, cx| {
                            this.close(CloseReason::Scroll, window, cx);
                        })),
                ),
        )
        .with_priority(1)
        .into_any_element()
    }

    /// The open submenu, beside its row.
    fn render_submenu(
        &self,
        parent: usize,
        colors: &Colors,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let MenuItem::Submenu { items, .. } = self.items.get(parent)? else {
            return None;
        };
        let bounds = self.bounds.get();
        let separators: Vec<bool> = self
            .items
            .iter()
            .map(|item| matches!(item, MenuItem::Separator))
            .collect();
        let item = f32::from(metrics::MENU_ITEM_HEIGHT);
        let separator = f32::from(metrics::HAIRLINE + space::S4 + space::S4);
        let inside = space::S4;
        let top =
            bounds.origin.y + inside + px(item_top(&separators, parent, item, separator)) - inside;
        let side = submenu_side(
            f32::from(bounds.left()),
            f32::from(bounds.right()),
            f32::from(bounds.size.width),
            f32::from(window.viewport_size().width),
            f32::from(metrics::MENU_WINDOW_MARGIN),
        );
        let (corner, x) = match side {
            Side::Right => (Corner::TopLeft, bounds.right()),
            Side::Left => (Corner::TopRight, bounds.left()),
        };
        let empty = Vec::new();
        let shortcuts = self.sub_shortcuts.get(parent).unwrap_or(&empty);
        let panel = self.render_panel(true, items, shortcuts, self.sub_cursor, colors, cx);
        Some(
            deferred(
                anchored()
                    .anchor(corner)
                    .position(point(x, top))
                    .snap_to_window_with_margin(metrics::MENU_WINDOW_MARGIN)
                    .child(self.entrance(panel, "menu-sub-open")),
            )
            .with_priority(3)
            .into_any_element(),
        )
    }
}

impl Render for Menu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div();
        if self.closed {
            return root;
        }
        let colors = Theme::get(cx).colors;
        let cell = Rc::clone(&self.bounds);
        let mut panel = self
            .render_panel(
                false,
                &self.items,
                &self.shortcuts,
                self.cursor,
                &colors,
                cx,
            )
            .key_context(MENU)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &menu::SelectPrevious, _, cx| {
                this.move_cursor(false, cx);
            }))
            .on_action(cx.listener(|this, _: &menu::SelectNext, _, cx| {
                this.move_cursor(true, cx);
            }))
            .on_action(cx.listener(|this, _: &menu::SelectFirst, _, cx| {
                this.move_to_end(false, cx);
            }))
            .on_action(cx.listener(|this, _: &menu::SelectLast, _, cx| {
                this.move_to_end(true, cx);
            }))
            .on_action(cx.listener(|this, _: &menu::OpenSubmenu, _, cx| this.on_right(cx)))
            .on_action(cx.listener(|this, _: &menu::CloseSubmenu, _, cx| this.on_left(cx)))
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                this.confirm(window, cx);
            }))
            .on_action(cx.listener(|this, _: &menu::Close, window, cx| {
                this.on_escape(window, cx);
            }))
            .on_action(cx.listener(|this, _: &menu::FocusNext, window, cx| {
                this.close(CloseReason::TabNext, window, cx);
            }))
            .on_action(cx.listener(|this, _: &menu::FocusPrev, window, cx| {
                this.close(CloseReason::TabPrev, window, cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                this.type_letter(event, cx);
            }));
        panel = panel.child(
            canvas(move |bounds, _, _| cell.set(bounds), |_, (), _, _| {})
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
        );
        let top = deferred(
            anchored()
                .position(self.position)
                .snap_to_window_with_margin(metrics::MENU_WINDOW_MARGIN)
                .child(self.entrance(panel, "menu-open")),
        )
        .with_priority(2);
        let submenu = self
            .open_sub
            .and_then(|parent| self.render_submenu(parent, &colors, window, cx));
        root.child(self.render_backdrop(window, cx))
            .child(top)
            .children(submenu)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROWS: [bool; 5] = [true, false, true, true, false];

    #[test]
    fn down_skips_separators_and_disabled_rows() {
        assert_eq!(step(&ROWS, Some(0), true, false), Some(2));
        assert_eq!(step(&ROWS, Some(2), true, false), Some(3));
        assert_eq!(step(&ROWS, Some(3), false, false), Some(2));
        assert_eq!(step(&ROWS, Some(2), false, false), Some(0));
    }

    #[test]
    fn without_wrap_the_ends_stay_put() {
        assert_eq!(step(&ROWS, Some(3), true, false), None);
        assert_eq!(step(&ROWS, Some(0), false, false), None);
    }

    #[test]
    fn with_wrap_the_ends_meet() {
        assert_eq!(step(&ROWS, Some(3), true, true), Some(0));
        assert_eq!(step(&ROWS, Some(0), false, true), Some(3));
    }

    #[test]
    fn from_nowhere_down_is_first_and_up_is_last() {
        assert_eq!(step(&ROWS, None, true, false), Some(0));
        assert_eq!(step(&ROWS, None, false, false), Some(3));
        assert_eq!(first_selectable(&ROWS), Some(0));
        assert_eq!(last_selectable(&ROWS), Some(3));
    }

    #[test]
    fn nothing_selectable_gives_nowhere_to_go() {
        let none = [false, false];
        assert_eq!(step(&none, None, true, true), None);
        assert_eq!(step(&none, Some(0), true, true), None);
        assert_eq!(step(&[], None, false, true), None);
        assert_eq!(first_selectable(&none), None);
    }

    #[test]
    fn a_lone_row_wraps_onto_itself() {
        assert_eq!(step(&[true], Some(0), true, true), Some(0));
        assert_eq!(step(&[true], Some(0), false, true), Some(0));
    }

    #[test]
    fn a_stale_cursor_does_not_panic() {
        assert_eq!(step(&ROWS, Some(40), true, true), Some(0));
        assert_eq!(step(&ROWS, Some(40), false, true), Some(3));
    }

    #[test]
    fn a_letter_jumps_to_the_next_row_starting_with_it() {
        let labels = [
            Some("Open"),
            Some("Open with"),
            None,
            Some("Copy"),
            Some("Cut"),
        ];
        let selectable = [true, true, false, true, true];
        assert_eq!(jump_letter(&labels, &selectable, Some(0), 'o'), Some(1));
        // From the last match it wraps, and ignores case.
        assert_eq!(jump_letter(&labels, &selectable, Some(1), 'O'), Some(0));
        assert_eq!(jump_letter(&labels, &selectable, Some(3), 'c'), Some(4));
        assert_eq!(jump_letter(&labels, &selectable, None, 'c'), Some(3));
        assert_eq!(jump_letter(&labels, &selectable, Some(0), 'z'), None);
    }

    #[test]
    fn a_letter_skips_disabled_rows() {
        let labels = [Some("Paste"), Some("Properties")];
        assert_eq!(jump_letter(&labels, &[false, true], None, 'p'), Some(1));
        assert_eq!(jump_letter(&labels, &[false, false], None, 'p'), None);
        assert_eq!(jump_letter(&[], &[], None, 'p'), None);
    }

    #[test]
    fn a_submenu_opens_right_unless_the_window_ends() {
        // Room on the right.
        assert_eq!(submenu_side(100., 320., 220., 800., 8.), Side::Right);
        // Not enough on the right, plenty on the left.
        assert_eq!(submenu_side(500., 720., 220., 800., 8.), Side::Left);
        // Exactly fits, margin included.
        assert_eq!(submenu_side(100., 320., 220., 548., 8.), Side::Right);
        assert_eq!(submenu_side(500., 720., 220., 947., 8.), Side::Left);
        // Narrow window: neither fits, the larger side wins.
        assert_eq!(submenu_side(10., 240., 220., 300., 8.), Side::Right);
        assert_eq!(submenu_side(200., 290., 220., 300., 8.), Side::Left);
    }

    #[test]
    fn a_row_sits_below_the_rows_and_separators_before_it() {
        let separators = [false, true, false];
        assert_eq!(item_top(&separators, 0, 28., 9.), 0.);
        assert_eq!(item_top(&separators, 1, 28., 9.), 28.);
        assert_eq!(item_top(&separators, 2, 28., 9.), 37.);
        assert_eq!(item_top(&separators, 9, 28., 9.), 65.);
    }

    #[test]
    fn shortcuts_read_as_text() {
        let ctrl_shift = Modifiers {
            control: true,
            shift: true,
            ..Modifiers::default()
        };
        assert_eq!(shortcut_text(&ctrl_shift, "c", false), "Ctrl+Shift+C");
        assert_eq!(shortcut_text(&ctrl_shift, "c", true), "\u{2303}\u{21E7}C");
        assert_eq!(shortcut_text(&Modifiers::default(), "f5", false), "F5");
        assert_eq!(
            shortcut_text(&Modifiers::default(), "enter", false),
            "Enter"
        );
        assert_eq!(
            shortcut_text(&Modifiers::default(), "pagedown", false),
            "PgDn"
        );
        assert_eq!(key_label(""), "");
    }
}
