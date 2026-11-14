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

//! The top bar: back and forward, the path, the search field and the window's caption buttons.
//!
//! The window has no system title bar, so this bar is also what the user drags the window by.
//! It is the only bar in the app.

use gpui::{
    App, ClickEvent, ClipboardItem, Context, Div, Entity, FocusHandle, IntoElement, Pixels, Point,
    Render, SharedString, Stateful, Subscription, Window, WindowControlArea, div, prelude::*, svg,
};

use crate::assets::icon;
use crate::components::breadcrumb::Breadcrumb;
use crate::components::menu::{CloseReason, MenuSlot};
use crate::components::search_field::SearchField;
use crate::keymap::context;
use crate::state::menus::{BUILT, MenuState, MenuTarget, build, file_manager_name};
use crate::state::navigation::Navigation;
use crate::state::order::SortSpec;
use crate::state::properties::{PropertiesTarget, PropertiesWindows, Request};
use crate::state::sources::Sources;
use crate::theme::{
    Colors, Theme, metrics,
    scale::{radius, size, space},
};

/// The top bar: navigation buttons, the path, the search field and window controls.
pub struct TitleBar {
    navigation: Entity<Navigation>,
    sources: Entity<Sources>,
    search_field: Entity<SearchField>,
    /// Holds the focus while a breadcrumb menu is open, so a chosen item reaches this view.
    focus: FocusHandle,
    /// The breadcrumb's context menu, while one is open.
    menu: MenuSlot,
    /// The depth of the segment the open menu is about.
    depth: usize,
    /// Where the focus was before the menu took it here.
    previous: Option<FocusHandle>,
    _subscription: Subscription,
}

impl TitleBar {
    /// Builds the top bar over `navigation`, with `search_field` at its right.
    pub fn new(
        navigation: Entity<Navigation>,
        sources: Entity<Sources>,
        search_field: Entity<SearchField>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe_in(&navigation, window, |this, _, window, cx| {
            // A menu belongs to the path it was opened on.
            this.menu.dismiss(window, cx);
            cx.notify();
        });
        Self {
            navigation,
            sources,
            search_field,
            focus: cx.focus_handle(),
            menu: MenuSlot::default(),
            depth: 0,
            previous: None,
            _subscription: subscription,
        }
    }

    /// Opens the menu of the breadcrumb segment at `depth`.
    fn open_crumb_menu(
        &mut self,
        depth: usize,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let last = self
            .navigation
            .read(cx)
            .current()
            .map(|location| location.crumbs().len().saturating_sub(1));
        let target = MenuTarget::Crumb {
            current: last == Some(depth),
        };
        let state = MenuState {
            built: BUILT,
            can_paste: false,
            sort: SortSpec::default(),
            file_manager: file_manager_name(),
            show_hidden: false,
            show_system: false,
        };
        let items = build(&target, &state);
        if items.is_empty() {
            return;
        }
        self.depth = depth;
        if !self.focus.is_focused(window) {
            self.previous = window.focused(cx);
        }
        window.focus(&self.focus);
        let focus = self.focus.clone();
        self.menu.show(
            items,
            at,
            Some(focus),
            window,
            cx,
            |this, reason, window, cx| {
                this.menu.clear();
                let moved = matches!(
                    reason,
                    CloseReason::FocusLost
                        | CloseReason::Blur
                        | CloseReason::TabNext
                        | CloseReason::TabPrev
                );
                if let Some(previous) = this.previous.take()
                    && !moved
                {
                    window.focus(&previous);
                }
                cx.notify();
            },
        );
    }

    /// "Open": goes to the segment's folder.
    fn context_open(&mut self, _: &context::Open, _: &mut Window, cx: &mut Context<Self>) {
        let depth = self.depth;
        self.navigation
            .update(cx, |navigation, cx| navigation.go_to_crumb(depth, cx));
    }

    /// "Copy path": the segment's folder, as a path of this computer on the host.
    fn context_copy_path(
        &mut self,
        _: &context::CopyFolderPath,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(location) = self.navigation.read(cx).current().cloned() else {
            return;
        };
        let depth = self.depth;
        let host_root = self
            .sources
            .read(cx)
            .host_root(location.source())
            .map(std::path::Path::to_path_buf);
        let crumbs = location.crumbs();
        let text = match host_root {
            Some(mut path) => {
                for crumb in crumbs.iter().take(depth + 1).skip(1) {
                    path.push(crumb.name.as_ref());
                }
                path.display().to_string()
            }
            None => crumbs
                .iter()
                .take(depth + 1)
                .map(|crumb| crumb.name.as_ref())
                .collect::<Vec<_>>()
                .join("/"),
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    /// "Properties": the segment's folder, or its volume for the first segment.
    fn context_properties(
        &mut self,
        _: &context::Properties,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(location) = self.navigation.read(cx).current().cloned() else {
            return;
        };
        let source = location.source();
        let depth = self.depth;
        let Some(at) = location.ancestor(depth) else {
            return;
        };
        let (target, request) = match depth.checked_sub(1).and_then(|up| location.ancestor(up)) {
            Some(parent) => {
                let host_path = self
                    .sources
                    .read(cx)
                    .host_root(source)
                    .map(std::path::Path::to_path_buf)
                    .map(|mut path| {
                        for crumb in at.crumbs().iter().skip(1) {
                            path.push(crumb.name.as_ref());
                        }
                        path
                    });
                let target = PropertiesTarget::Item {
                    source,
                    parent: parent.node(),
                    node: at.node(),
                    name: at.name().clone(),
                };
                let request = Request {
                    from: Some(parent),
                    host_path,
                    folder: true,
                    selection_total: None,
                };
                (target, request)
            }
            None => (PropertiesTarget::Volume { source }, Request::default()),
        };
        PropertiesWindows::open_here(target, request, cx);
    }
}

/// Back or forward: a square button that is inert when `enabled` is false.
fn nav_button(
    id: &'static str,
    glyph: &'static str,
    enabled: bool,
    colors: Colors,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let button = div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(size::CONTROL)
        .rounded(radius::ROW)
        // A button is not part of the bar's drag area.
        .occlude()
        .child(
            svg()
                .path(glyph)
                .size(metrics::ICON_SMALL)
                .text_color(if enabled { colors.ink } else { colors.ink3 }),
        );
    if enabled {
        button
            .cursor_pointer()
            .hover(|style| style.bg(colors.hover))
            .active(|style| style.bg(colors.field))
            .on_click(on_click)
    } else {
        button
    }
}

/// A Windows caption button. Windows itself acts on the click (the control area maps to a
/// non-client hit-test code), so there is no click handler.
fn caption_button(
    id: &'static str,
    glyph: &'static str,
    area: WindowControlArea,
    colors: Colors,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w(metrics::CAPTION_BUTTON_WIDTH)
        .h(size::TOP_BAR)
        .window_control_area(area)
        // Without this the bar's Drag layer, which comes first in paint order, would win the hit
        // test over the button.
        .occlude()
        .hover(|style| style.bg(colors.hover))
        .child(
            svg()
                .path(glyph)
                .size(metrics::ICON_CAPTION)
                .text_color(colors.ink2),
        )
}

impl Render for TitleBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Theme::get(cx).colors;
        let navigation = self.navigation.read(cx);
        let can_go_back = navigation.can_go_back();
        let can_go_forward = navigation.can_go_forward();
        let names: Vec<SharedString> = navigation
            .current()
            .map(|location| {
                location
                    .crumbs()
                    .iter()
                    .map(|crumb| crumb.name.clone())
                    .collect()
            })
            .unwrap_or_default();

        let is_windows = cfg!(target_os = "windows");
        let is_macos = cfg!(target_os = "macos");
        let left_padding = if is_macos {
            metrics::TRAFFIC_LIGHT_SPACE
        } else {
            metrics::ROW_PADDING
        };

        let go_to_crumb = {
            let navigation = self.navigation.clone();
            move |depth: usize, _: &mut Window, cx: &mut App| {
                navigation.update(cx, |navigation, cx| navigation.go_to_crumb(depth, cx));
            }
        };

        let this = cx.entity();
        let on_crumb_menu =
            move |depth: usize, at: Point<Pixels>, window: &mut Window, cx: &mut App| {
                this.update(cx, |bar, cx| bar.open_crumb_menu(depth, at, window, cx));
            };
        div()
            .id("titlebar")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::context_open))
            .on_action(cx.listener(Self::context_copy_path))
            .on_action(cx.listener(Self::context_properties))
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .relative()
            .w_full()
            .h(size::TOP_BAR)
            .pl(left_padding)
            .gap(space::S4)
            // Windows turns this layer into the caption: it drags the window, a double click
            // maximizes it and the snap layouts work. Interactive children are marked `occlude`
            // so they stay out of it. GPUI asks for control areas before it tests its own top
            // resize edge, so the layer starts below that edge unless the window is maximized.
            // Other platforms ignore control areas.
            .when(is_windows, |bar| {
                let top = if window.is_maximized() {
                    Pixels::ZERO
                } else {
                    metrics::WINDOW_RESIZE_BAND
                };
                bar.child(
                    div()
                        .absolute()
                        .top(top)
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .window_control_area(WindowControlArea::Drag),
                )
            })
            // macOS drags a transparent title bar natively; GPUI only offers the double-click
            // action (zoom or minimize, per the system setting).
            .when(is_macos, |bar| {
                bar.on_click(|event, window, _| {
                    if event.click_count() == 2 {
                        window.titlebar_double_click();
                    }
                })
            })
            .child(nav_button(
                "back",
                icon::BACK,
                can_go_back,
                colors,
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.navigation
                        .update(cx, |navigation, cx| navigation.go_back(cx));
                }),
            ))
            .child(nav_button(
                "forward",
                icon::FORWARD,
                can_go_forward,
                colors,
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.navigation
                        .update(cx, |navigation, cx| navigation.go_forward(cx));
                }),
            ))
            // The breadcrumb takes the free width and shrinks first, which pushes the field to
            // the right edge.
            .child(Breadcrumb::new(names, go_to_crumb).on_context_menu(on_crumb_menu))
            .child(
                div()
                    .flex_none()
                    .mr(space::S8)
                    // Without this the bar's Drag layer would win the hit test over the field.
                    .occlude()
                    .child(self.search_field.clone()),
            )
            // Out of the row's flow, so the gap does not open while the menu is shown.
            .children(self.menu.entity().map(|menu| div().absolute().child(menu)))
            .when(is_windows, |bar| {
                let maximize = if window.is_maximized() {
                    icon::RESTORE
                } else {
                    icon::MAXIMIZE
                };
                // Grouped so the bar's gap does not open between the three buttons.
                bar.child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_none()
                        .child(caption_button(
                            "minimize",
                            icon::MINIMIZE,
                            WindowControlArea::Min,
                            colors,
                        ))
                        .child(caption_button(
                            "maximize",
                            maximize,
                            WindowControlArea::Max,
                            colors,
                        ))
                        .child(caption_button(
                            "close",
                            icon::CLOSE,
                            WindowControlArea::Close,
                            colors,
                        )),
                )
            })
    }
}
