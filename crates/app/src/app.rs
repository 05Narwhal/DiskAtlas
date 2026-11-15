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

//! The root view: creates the state entities and the views, and lays them out.

use std::time::Instant;

use gpui::{
    AnyElement, Context, Entity, Focusable, IntoElement, MouseButton, MouseDownEvent,
    NavigationDirection, PathPromptOptions, Render, Subscription, Window, WindowHandle, div,
    prelude::*,
};

use crate::components::{
    details::Details,
    disk_strip::DiskStrip,
    search_field::{SearchField, SearchFieldEvent},
    sidebar::Sidebar,
    statusbar::StatusBar,
    storage_bar::StorageBar,
    titlebar::TitleBar,
    toolbar::FilterRow,
};
use crate::keymap::workspace::{
    FocusNext, FocusPrev, GoBack, GoForward, GoUp, LeaveSearch, OpenImage, OpenImageFolder,
    OpenSettings, Properties, Refresh, Search as FocusSearch, ShowList, ShowOnDisk, ShowSpaceMap,
    ToggleDetails, ToggleHidden, ToggleSystemItems,
};
use crate::keymap::{context, file_list, quick_look};
use crate::state::settings::{SettingValue, Settings};
use crate::state::{
    listing::{Listing, Motion},
    navigation::{Location, Navigation},
    properties::{Ask, PropertiesEnv, PropertiesWindows, ask_for_selection},
    quick_look::{QuickLook, Tab},
    search::{Search, SearchEvent},
    sizes::Sizes,
    sources::{SourceKind, Sources, SourcesEvent},
    space_map::SpaceMap,
    strip::Strip,
    view::{ViewMode, ViewState},
};
use crate::theme::{
    Theme, fonts,
    motion::{self, Disclosure},
    scale::{elevation, radius, size, text},
};
use crate::views::disk_view::DiskView;
use crate::views::explorer::Explorer;
use crate::views::preview::QuickLookWindow;
use crate::views::space_map::SpaceMapView;

/// The window's root view.
pub struct Workspace {
    sources: Entity<Sources>,
    navigation: Entity<Navigation>,
    listing: Entity<Listing>,
    search: Entity<Search>,
    sizes: Entity<Sizes>,
    search_field: Entity<SearchField>,
    titlebar: Entity<TitleBar>,
    sidebar: Entity<Sidebar>,
    storage_bar: Entity<StorageBar>,
    explorer: Entity<Explorer>,
    view_state: Entity<ViewState>,
    map_view: Entity<SpaceMapView>,
    disk_view: Entity<DiskView>,
    statusbar: Entity<StatusBar>,
    details: Entity<Details>,
    details_visible: bool,
    /// The details pane opening or closing (`r1-redesign.md` 2.2); `None` at rest.
    details_motion: Option<Disclosure>,
    quick_look: Entity<QuickLook>,
    /// Quick Look's own window, open while `quick_look` is.
    quick_look_window: QuickLookWindow,
    /// This window, to close Quick Look through it when its window goes by itself.
    main: Option<WindowHandle<Workspace>>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    /// Creates the state entities and the views.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let sources = cx.new(Sources::new);
        let navigation = cx.new(|_| Navigation::new());
        let listing = cx.new(|cx| Listing::new(navigation.clone(), sources.clone(), cx));
        let search =
            cx.new(|cx| Search::new(sources.clone(), navigation.clone(), listing.clone(), cx));
        let sizes = cx.new(|cx| Sizes::new(sources.clone(), search.clone(), listing.clone(), cx));
        // The Properties windows read these; every menu and key that opens one finds them here.
        PropertiesWindows::install(
            PropertiesEnv::new(
                sources.clone(),
                sizes.clone(),
                search.clone(),
                navigation.clone(),
            ),
            cx,
        );

        let search_field =
            cx.new(|cx| SearchField::new(search.clone(), listing.clone(), navigation.clone(), cx));
        let titlebar = cx.new(|cx| {
            TitleBar::new(
                navigation.clone(),
                sources.clone(),
                search_field.clone(),
                window,
                cx,
            )
        });
        let view_state = cx.new(|cx| ViewState::new(sources.clone(), navigation.clone(), cx));
        let sidebar = cx.new(|cx| {
            Sidebar::new(
                sources.clone(),
                navigation.clone(),
                view_state.clone(),
                window,
                cx,
            )
        });
        let space_map = cx.new(|cx| {
            SpaceMap::new(
                sources.clone(),
                navigation.clone(),
                search.clone(),
                sizes.clone(),
                listing.clone(),
                view_state.clone(),
                cx,
            )
        });
        let map_view = cx.new(|cx| SpaceMapView::new(space_map.clone(), view_state.clone(), cx));
        let disk_view =
            cx.new(|cx| DiskView::new(sources.clone(), view_state.clone(), map_view.clone(), cx));
        let strip =
            cx.new(|cx| Strip::new(sources.clone(), navigation.clone(), listing.clone(), cx));
        let disk_strip =
            cx.new(|cx| DiskStrip::new(strip, sources.clone(), navigation.clone(), cx));
        let storage_bar = cx.new(|cx| {
            StorageBar::new(
                sources.clone(),
                navigation.clone(),
                sizes.clone(),
                search.clone(),
                listing.clone(),
                view_state.clone(),
                disk_strip,
                cx,
            )
        });
        let filter_row = cx.new(|cx| FilterRow::new(search.clone(), window, cx));
        let explorer = cx.new(|cx| {
            Explorer::new(
                listing.clone(),
                search.clone(),
                filter_row.clone(),
                window,
                cx,
            )
        });
        let statusbar = cx.new(|cx| StatusBar::new(listing.clone(), search.clone(), cx));
        let quick_look = cx.new(|cx| QuickLook::new(listing.clone(), cx));
        let details = cx.new(|cx| Details::new(listing.clone(), quick_look.clone(), cx));
        // The details pane starts visible, and its preview well follows the selection.
        quick_look.update(cx, |quick_look, cx| quick_look.set_peek(true, None, cx));

        let mut subscriptions = Vec::new();

        subscriptions.push(
            cx.subscribe(
                &sources,
                |this, sources, event: &SourcesEvent, cx| match event {
                    // Open the first source that can be browsed as soon as there is one, so the
                    // app never starts on a disk it cannot read.
                    SourcesEvent::Changed => {
                        if this.navigation.read(cx).current().is_some() {
                            return;
                        }
                        let first = sources
                            .read(cx)
                            .items()
                            .iter()
                            .find(|source| source.availability.is_browsable())
                            .map(Location::root);
                        if let Some(first) = first {
                            this.navigation
                                .update(cx, |navigation, cx| navigation.navigate(first, cx));
                        }
                    }
                    // An image the user opened: show it.
                    SourcesEvent::Selected(id) => {
                        let to = sources.read(cx).source(*id).map(Location::root);
                        if let Some(to) = to {
                            this.navigation
                                .update(cx, |navigation, cx| navigation.navigate(to, cx));
                        }
                    }
                },
            ),
        );

        subscriptions.push(cx.subscribe_in(
            &search_field,
            window,
            |this, _, event: &SearchFieldEvent, window, cx| match event {
                SearchFieldEvent::Edited(text) => {
                    let text = text.clone();
                    this.search.update(cx, |search, cx| {
                        if !search.is_active() {
                            search.activate(cx);
                        }
                        search.set_text(text, cx);
                    });
                }
                SearchFieldEvent::Escape => {
                    let searching = this.search.read(cx).is_active();
                    if searching || !this.search_field.read(cx).is_empty() {
                        this.leave_search(cx);
                    } else {
                        window.focus(&this.explorer.focus_handle(cx));
                    }
                }
                SearchFieldEvent::ToList => {
                    window.focus(&this.explorer.focus_handle(cx));
                    this.listing.update(cx, |listing, cx| {
                        if !listing.is_empty() {
                            listing.move_cursor(Motion::First, false, cx);
                        }
                    });
                }
            },
        ));
        // A navigation ends the search from underneath the field: it must not keep stale text.
        subscriptions.push(cx.subscribe(&search, |this, search, _: &SearchEvent, cx| {
            if !search.read(cx).is_active() && !this.search_field.read(cx).is_empty() {
                this.search_field
                    .update(cx, |field, cx| field.set_text("", cx));
            }
        }));

        // Quick Look is a window of its own: it opens and closes with the shared state...
        subscriptions.push(cx.observe(&quick_look, |this, quick_look, cx| {
            this.quick_look_window.sync(&quick_look, cx);
        }));
        // ... and when it goes some other way (its close button, Escape, Ctrl+W, Alt+F4) the
        // state is closed too. The callback may run inside an update of this view, so the work
        // waits for the end of it.
        let workspace = cx.weak_entity();
        subscriptions.push(cx.on_window_closed(move |cx| {
            let workspace = workspace.clone();
            cx.defer(move |cx| {
                let main = workspace
                    .update(cx, |workspace, cx| {
                        workspace
                            .quick_look_window
                            .vanished(cx)
                            .then_some(workspace.main)
                            .flatten()
                    })
                    .ok()
                    .flatten();
                if let Some(main) = main {
                    main.update(cx, |workspace, window, cx| {
                        workspace.close_quick_look(window, cx);
                        window.activate_window();
                    })
                    .ok();
                }
            });
        }));

        // Coming back to the window: a volume that changed meanwhile is read again, and the
        // system's reduced-motion setting is asked for again.
        subscriptions.push(cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.sizes.update(cx, |sizes, cx| sizes.recheck(cx));
                motion::refresh_system(cx);
            }
        }));

        subscriptions.push(cx.observe_window_appearance(window, |_, window, cx| {
            crate::theme::apply_setting(window.appearance().into(), cx);
        }));
        // The `theme` setting changes the palette at once.
        subscriptions.push(cx.observe_global_in::<Settings>(window, |_, window, cx| {
            crate::theme::apply_setting(window.appearance().into(), cx);
        }));
        // The theme was installed at startup from the app-level appearance; align it with the
        // window's own.
        crate::theme::apply_setting(window.appearance().into(), cx);

        window.focus(&explorer.focus_handle(cx));

        // Debug builds: DISK_ATLAS_OPEN_IMAGE=<path> opens a disk image at start, so a screenshot
        // run needs no file dialog.
        if cfg!(debug_assertions)
            && let Some(path) = std::env::var_os("DISK_ATLAS_OPEN_IMAGE")
        {
            sources.update(cx, |sources, cx| {
                sources.open_image(std::path::PathBuf::from(path), cx);
            });
        }

        Self {
            sources,
            navigation,
            listing,
            search,
            sizes,
            search_field,
            titlebar,
            sidebar,
            storage_bar,
            explorer,
            view_state,
            map_view,
            disk_view,
            statusbar,
            details,
            details_visible: true,
            details_motion: None,
            quick_look,
            quick_look_window: QuickLookWindow::default(),
            main: window.window_handle().downcast::<Workspace>(),
            _subscriptions: subscriptions,
        }
    }

    fn go_back(&mut self, _: &GoBack, _: &mut Window, cx: &mut Context<Self>) {
        self.navigation
            .update(cx, |navigation, cx| navigation.go_back(cx));
    }

    fn go_forward(&mut self, _: &GoForward, _: &mut Window, cx: &mut Context<Self>) {
        self.navigation
            .update(cx, |navigation, cx| navigation.go_forward(cx));
    }

    fn go_up(&mut self, _: &GoUp, _: &mut Window, cx: &mut Context<Self>) {
        self.navigation
            .update(cx, |navigation, cx| navigation.go_up(cx));
    }

    /// F5: reads the current folder again.
    fn refresh(&mut self, _: &Refresh, _: &mut Window, cx: &mut Context<Self>) {
        self.listing.update(cx, |listing, cx| listing.refresh(cx));
        self.sizes.update(cx, |sizes, cx| sizes.recheck(cx));
    }

    /// Ctrl+Shift+. and the View menu: flips `show_hidden`.
    fn toggle_hidden(&mut self, _: &ToggleHidden, _: &mut Window, cx: &mut Context<Self>) {
        flip(cx, "show_hidden");
    }

    /// Ctrl+1: the folder as a list.
    fn show_list(&mut self, _: &ShowList, window: &mut Window, cx: &mut Context<Self>) {
        self.view_state
            .update(cx, |view, cx| view.set_mode(ViewMode::List, cx));
        window.focus(&self.explorer.focus_handle(cx));
    }

    /// Ctrl+3: the folder as the space map.
    fn show_space_map(&mut self, _: &ShowSpaceMap, window: &mut Window, cx: &mut Context<Self>) {
        self.view_state
            .update(cx, |view, cx| view.set_mode(ViewMode::SpaceMap, cx));
        window.focus(&self.map_view.focus_handle(cx));
    }

    /// "Show where it is on the disk": the disk view of the disk the folder is on, in Layout mode.
    fn show_on_disk(&mut self, _: &ShowOnDisk, _: &mut Window, cx: &mut Context<Self>) {
        let disk = self
            .navigation
            .read(cx)
            .current()
            .and_then(|location| self.sources.read(cx).source(location.source()))
            .and_then(|source| match source.kind {
                SourceKind::Volume { disk, .. } => Some(disk),
                _ => None,
            });
        if let Some(disk) = disk {
            self.view_state
                .update(cx, |view, cx| view.show_disk_layout(disk, cx));
        }
    }

    /// Ctrl+, : opens the Settings window, or brings the open one forward.
    fn open_settings(&mut self, _: &OpenSettings, _: &mut Window, cx: &mut Context<Self>) {
        crate::views::settings::open(cx);
    }

    /// Alt+Enter, Ctrl+I and the file list's "Properties": the selected row, the selected rows
    /// together, or the folder shown when nothing is selected.
    fn open_properties(&mut self, _: &Properties, _: &mut Window, cx: &mut Context<Self>) {
        let ask = ask_for_selection(
            self.listing.read(cx),
            self.search.read(cx),
            self.sources.read(cx),
            cx,
        );
        match ask {
            Some(Ask::Open(target, request)) => {
                PropertiesWindows::open_here(target, request, cx);
            }
            // A hit on this computer: its names are walked off the UI thread first.
            Some(Ask::Walk(walk)) => {
                cx.spawn(async move |_, cx| {
                    let (walk, found) = cx
                        .background_executor()
                        .spawn(async move {
                            let found = walk.resolve();
                            (walk, found)
                        })
                        .await;
                    match found {
                        Ok(Some(node)) => {
                            cx.update(|cx| {
                                PropertiesWindows::open_here(
                                    walk.target(node),
                                    walk.request.clone(),
                                    cx,
                                );
                            })
                            .ok();
                        }
                        Ok(None) => tracing::info!("Properties: the search hit is gone"),
                        Err(error) => tracing::warn!(%error, "Properties: the hit was not found"),
                    }
                })
                .detach();
            }
            None => {}
        }
    }

    /// The View menu: flips `show_system_items`.
    fn toggle_system_items(
        &mut self,
        _: &ToggleSystemItems,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        flip(cx, "show_system_items");
    }

    /// Asks for a disk image with the system file prompt and adds it to the sidebar. Files and
    /// folders together where the platform's prompt allows it (sparse bundles are folders);
    /// otherwise files only.
    fn open_image(&mut self, _: &OpenImage, _: &mut Window, cx: &mut Context<Self>) {
        let options = PathPromptOptions {
            files: true,
            directories: cx.can_select_mixed_files_and_dirs(),
            multiple: false,
            prompt: Some("Open".into()),
        };
        self.prompt_for_image(options, cx);
    }

    /// Asks for one folder with the system prompt and adds it to the sidebar as a disk image. For
    /// sparse bundles on platforms whose prompt cannot pick files and folders together.
    fn open_image_folder(&mut self, _: &OpenImageFolder, _: &mut Window, cx: &mut Context<Self>) {
        let options = PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open".into()),
        };
        self.prompt_for_image(options, cx);
    }

    /// "Open in Disk Atlas" on a disk image of this computer: adds it to the sidebar and shows it.
    fn open_image_under_cursor(
        &mut self,
        _: &context::OpenInDiskAtlas,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = {
            let listing = self.listing.read(cx);
            listing
                .cursor()
                .and_then(|position| listing.host_path_of(position, cx))
        };
        if let Some(path) = path {
            self.sources
                .update(cx, |sources, cx| sources.open_image(path, cx));
        }
    }

    /// Shows the system prompt and hands the chosen path to `Sources::open_image`.
    fn prompt_for_image(&mut self, options: PathPromptOptions, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(options);
        let sources = self.sources.clone();
        cx.spawn(async move |_, cx| match chosen.await {
            Ok(Ok(Some(paths))) => {
                if let Some(path) = paths.into_iter().next() {
                    sources
                        .update(cx, |sources, cx| sources.open_image(path, cx))
                        .ok();
                }
            }
            Ok(Ok(None)) => {}
            Ok(Err(error)) => tracing::warn!(%error, "the file prompt failed"),
            Err(_) => {}
        })
        .detach();
    }

    /// Ends the search and empties the field.
    fn leave_search(&mut self, cx: &mut Context<Self>) {
        self.search.update(cx, |search, cx| search.leave(cx));
        self.search_field
            .update(cx, |field, cx| field.set_text("", cx));
    }

    /// Ctrl+F: starts a search with the field focused and its text selected.
    fn focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.search_field
            .update(cx, |field, cx| field.focus_and_select_all(window, cx));
        self.search.update(cx, |search, cx| search.activate(cx));
    }

    /// Escape in the file list: closes Quick Look first, then leaves a search; otherwise the key
    /// is left for others.
    fn leave_search_action(
        &mut self,
        _: &LeaveSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.quick_look.read(cx).is_open() {
            self.close_quick_look(window, cx);
        } else if self.search.read(cx).is_active() {
            self.leave_search(cx);
        } else {
            cx.propagate();
        }
    }

    fn focus_next(&mut self, _: &FocusNext, window: &mut Window, _: &mut Context<Self>) {
        window.focus_next();
    }

    fn focus_prev(&mut self, _: &FocusPrev, window: &mut Window, _: &mut Context<Self>) {
        window.focus_prev();
    }

    fn toggle_details_action(
        &mut self,
        _: &ToggleDetails,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_details(window, cx);
    }

    /// Shows or hides the details pane. Its preview well loads only while it is visible.
    pub fn toggle_details(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.details_visible = !self.details_visible;
        let visible = self.details_visible;
        self.details_motion = Some(Disclosure::new(
            visible,
            Instant::now(),
            motion::reduced(cx),
        ));
        self.quick_look.update(cx, |quick_look, cx| {
            quick_look.set_peek(visible, Some(window), cx);
        });
        cx.notify();
    }

    /// Space in the list.
    fn toggle_quick_look(
        &mut self,
        _: &quick_look::Toggle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.quick_look
            .update(cx, |quick_look, cx| quick_look.toggle(window, cx));
    }

    /// Enter on a file: the list leaves it to the window.
    fn open_quick_look(&mut self, _: &file_list::Open, _: &mut Window, cx: &mut Context<Self>) {
        self.quick_look
            .update(cx, |quick_look, cx| quick_look.open(cx));
    }

    /// Closes Quick Look (its window follows) and gives the keyboard back to the list.
    fn close_quick_look(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.quick_look
            .update(cx, |quick_look, cx| quick_look.close(window, cx));
        window.focus(&self.explorer.focus_handle(cx));
    }

    /// Ctrl+Tab and Ctrl+Shift+Tab: the next and previous tab, while the panel is open.
    fn cycle_tab(&mut self, forward: bool, cx: &mut Context<Self>) {
        self.quick_look.update(cx, |quick_look, cx| {
            if quick_look.is_open() {
                let tab = quick_look.tab().cycle(forward);
                quick_look.set_tab(tab, cx);
            }
        });
    }

    /// Ctrl+G: shows the Bytes tab, where "Go to byte" will live (M5 task 5b builds the prompt).
    fn go_to_byte(&mut self, _: &quick_look::GoToByte, _: &mut Window, cx: &mut Context<Self>) {
        self.quick_look.update(cx, |quick_look, cx| {
            if quick_look.is_open() {
                quick_look.set_tab(Tab::Bytes, cx);
            }
        });
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::frame_trace::begin(cx);
        let theme = Theme::get(cx);
        let (colors, appearance) = (theme.colors, theme.appearance);

        // The details pane grows from 0 to its width with its content clipped, not reflowed;
        // closing, it stays drawn until it has shrunk away.
        let details_frame = self
            .details_motion
            .and_then(|disclosure| disclosure.at(Instant::now()));
        if details_frame.is_none() {
            self.details_motion = None;
        } else {
            window.request_animation_frame();
            crate::frame_trace::note(cx, "details-pane");
        }

        // The folder as a list, as the space map, or inside the disk view.
        let main_area: AnyElement = {
            let view = self.view_state.read(cx);
            if view.disk().is_some() {
                self.disk_view.clone().into_any_element()
            } else if view.mode() == ViewMode::SpaceMap {
                self.map_view.clone().into_any_element()
            } else {
                self.explorer.clone().into_any_element()
            }
        };
        let mut content = div().flex().flex_row().size_full().child(
            div()
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .flex_col()
                .child(div().flex_1().min_h_0().child(main_area))
                .child(self.statusbar.clone()),
        );
        if self.details_visible || details_frame.is_some() {
            let (extent, opacity) =
                details_frame.map_or((1.0, 1.0), |frame| (frame.extent, frame.opacity));
            content = content.child(
                div()
                    .flex_none()
                    .w(size::DETAILS * extent)
                    .h_full()
                    .overflow_hidden()
                    .opacity(opacity)
                    .child(
                        div()
                            .w(size::DETAILS)
                            .h_full()
                            .border_l_1()
                            .border_color(colors.line)
                            .child(self.details.clone()),
                    ),
            );
        }

        div()
            .size_full()
            .flex()
            .flex_col()
            .on_action(cx.listener(Self::go_back))
            .on_action(cx.listener(Self::go_forward))
            .on_action(cx.listener(Self::go_up))
            .on_action(cx.listener(Self::refresh))
            .on_action(cx.listener(Self::toggle_hidden))
            .on_action(cx.listener(Self::toggle_system_items))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::open_properties))
            .on_action(cx.listener(Self::show_list))
            .on_action(cx.listener(Self::show_space_map))
            .on_action(cx.listener(Self::show_on_disk))
            .on_action(cx.listener(Self::toggle_details_action))
            .on_action(cx.listener(Self::open_image))
            .on_action(cx.listener(Self::open_image_folder))
            .on_action(cx.listener(Self::open_image_under_cursor))
            .on_action(cx.listener(Self::focus_next))
            .on_action(cx.listener(Self::focus_prev))
            .on_action(cx.listener(Self::focus_search))
            .on_action(cx.listener(Self::leave_search_action))
            .on_action(cx.listener(Self::toggle_quick_look))
            .on_action(cx.listener(Self::open_quick_look))
            .on_action(cx.listener(Self::go_to_byte))
            .on_action(cx.listener(|this, _: &quick_look::NextTab, _, cx| this.cycle_tab(true, cx)))
            .on_action(cx.listener(|this, _: &quick_look::PreviousTab, _, cx| {
                this.cycle_tab(false, cx);
            }))
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Back),
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.go_back(&GoBack, window, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Navigate(NavigationDirection::Forward),
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    this.go_forward(&GoForward, window, cx);
                }),
            )
            .bg(colors.chrome)
            .text_color(colors.ink)
            .font(fonts::ui_font())
            .text_size(text::BODY.size)
            .child(self.titlebar.clone())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .flex_none()
                            .w(size::SIDEBAR)
                            .h_full()
                            .child(self.sidebar.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .bg(colors.sheet)
                            .shadow(elevation::sheet(appearance, &colors))
                            .rounded_tl(radius::SHEET)
                            .overflow_hidden()
                            .flex()
                            .flex_col()
                            // The strip spans the list and the details pane; it takes no space
                            // for a host folder.
                            .child(self.storage_bar.clone())
                            .child(div().flex_1().min_h_0().child(content)),
                    ),
            )
            .child(crate::frame_trace::end_marker())
    }
}

/// Turns the switch `key` of the settings over; the settings entity repaints what follows it.
fn flip(cx: &mut gpui::App, key: &'static str) {
    let on = Settings::get(cx).bool_of(key);
    Settings::set(cx, key, SettingValue::Bool(!on));
}

/// Quick Look in a window of its own, driven through a real workspace.
#[cfg(test)]
mod quick_look_window {
    use gpui::{
        AnyWindowHandle, FocusHandle, Modifiers, TestAppContext, VisualTestContext, WindowHandle,
    };

    use super::*;
    use crate::keymap::{self, workspace as window_actions};
    use crate::theme;
    use crate::views::aux_window;

    /// The workspace as `main.rs` makes it: theme, keys, the window-closed rule.
    fn open_workspace(cx: &mut TestAppContext) -> WindowHandle<Workspace> {
        cx.update(|cx| {
            cx.set_global(Settings::defaults());
            // Fonts may not load in a test; the palette is installed first.
            theme::init(cx).ok();
            keymap::init(cx);
            cx.on_window_closed(|cx| {
                aux_window::window_closed(cx);
            })
            .detach();
        });
        let main = cx.add_window(Workspace::new);
        cx.update(|cx| aux_window::register_main(cx, main.into()));
        cx.run_until_parked();
        main
    }

    /// Opens Quick Look in `main` (Space in the list) and returns its window.
    fn open_window_in(cx: &mut TestAppContext, main: WindowHandle<Workspace>) -> AnyWindowHandle {
        cx.dispatch_action(main.into(), quick_look::Toggle);
        cx.run_until_parked();
        cx.windows()
            .into_iter()
            .find(|window| *window != AnyWindowHandle::from(main))
            .expect("Quick Look opened a window of its own")
    }

    /// The workspace with Quick Look open, and Quick Look's window.
    fn open_quick_look(cx: &mut TestAppContext) -> (WindowHandle<Workspace>, AnyWindowHandle) {
        let main = open_workspace(cx);
        let panel = open_window_in(cx, main);
        (main, panel)
    }

    fn is_open(cx: &mut TestAppContext, main: WindowHandle<Workspace>) -> bool {
        cx.update(|cx| {
            main.read(cx)
                .expect("the main window is open")
                .quick_look
                .read(cx)
                .is_open()
        })
    }

    fn windows_open(cx: &mut TestAppContext) -> usize {
        cx.windows().len()
    }

    fn click(cx: &mut TestAppContext, window: AnyWindowHandle, selector: &'static str) {
        let mut window = VisualTestContext::from_window(window, cx);
        let bounds = window
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} was not drawn"));
        window.simulate_click(bounds.center(), Modifiers::none());
        window.run_until_parked();
    }

    /// True when the focus of `window` is the handle `held`.
    fn focus_is(cx: &mut TestAppContext, window: AnyWindowHandle, held: &FocusHandle) -> bool {
        focused(cx, window).as_ref() == Some(held)
    }

    fn focused(cx: &mut TestAppContext, window: AnyWindowHandle) -> Option<FocusHandle> {
        cx.update(|cx| {
            window
                .update(cx, |_, window, cx| window.focused(cx))
                .expect("the window is open")
        })
    }

    #[gpui::test]
    fn space_in_the_list_opens_quick_look_in_a_window_of_its_own(cx: &mut TestAppContext) {
        let (main, _) = open_quick_look(cx);
        assert!(is_open(cx, main));
        assert_eq!(windows_open(cx), 2);
        // Space again, in the list: it closes, and the window with it.
        cx.dispatch_action(main.into(), quick_look::Toggle);
        cx.run_until_parked();
        assert!(!is_open(cx, main));
        assert_eq!(windows_open(cx), 1);
    }

    #[gpui::test]
    fn there_is_one_quick_look_window_at_a_time(cx: &mut TestAppContext) {
        let (main, _) = open_quick_look(cx);
        // Enter on a file asks again while it is open.
        cx.dispatch_action(main.into(), file_list::Open);
        cx.run_until_parked();
        assert_eq!(windows_open(cx), 2);
        assert!(is_open(cx, main));
    }

    #[gpui::test]
    fn space_and_escape_close_after_clicking_any_tab(cx: &mut TestAppContext) {
        let main = open_workspace(cx);
        for keys in ["space", "escape"] {
            for tab in ["quick-look-tab-0", "quick-look-tab-1", "quick-look-tab-2"] {
                let panel = open_window_in(cx, main);
                let before = focused(cx, panel).expect("the panel holds focus when it opens");
                click(cx, panel, tab);
                assert!(is_open(cx, main), "{tab}: a click does not close");
                // The click selected the tab without taking keyboard focus.
                assert!(
                    focus_is(cx, panel, &before),
                    "{tab}: a click must not move keyboard focus"
                );
                cx.simulate_keystrokes(panel, keys);
                cx.run_until_parked();
                assert!(!is_open(cx, main), "{keys} after {tab}");
                assert_eq!(windows_open(cx), 1, "{keys} after {tab}: the window closed");
            }
        }
    }

    #[gpui::test]
    fn the_tab_button_picks_the_tab(cx: &mut TestAppContext) {
        let (main, panel) = open_quick_look(cx);
        click(cx, panel, "quick-look-tab-1");
        let tab = cx.update(|cx| main.read(cx).unwrap().quick_look.read(cx).tab());
        assert_eq!(tab, Tab::Bytes);
    }

    #[gpui::test]
    fn space_and_escape_close_from_a_focused_button(cx: &mut TestAppContext) {
        let main = open_workspace(cx);
        for keys in ["space", "escape"] {
            let panel = open_window_in(cx, main);
            let before = focused(cx, panel).expect("the panel holds focus when it opens");
            // Tab moves focus onto the first tab button.
            cx.simulate_keystrokes(panel, "tab");
            assert!(
                !focus_is(cx, panel, &before),
                "tab moved focus onto a button"
            );
            cx.simulate_keystrokes(panel, keys);
            cx.run_until_parked();
            assert!(!is_open(cx, main), "{keys} on a focused tab");
            assert_eq!(windows_open(cx), 1);
        }
    }

    #[gpui::test]
    fn the_close_button_closes(cx: &mut TestAppContext) {
        let (main, panel) = open_quick_look(cx);
        click(cx, panel, "quick-look-close");
        assert!(!is_open(cx, main));
        assert_eq!(windows_open(cx), 1);
    }

    #[gpui::test]
    fn ctrl_w_closes_quick_look_and_its_window(cx: &mut TestAppContext) {
        let (main, panel) = open_quick_look(cx);
        // "secondary" is Ctrl here and Cmd on macOS, as the keymap says.
        cx.simulate_keystrokes(panel, "secondary-w");
        cx.run_until_parked();
        assert!(!is_open(cx, main));
        assert_eq!(windows_open(cx), 1);
    }

    #[gpui::test]
    fn a_window_closed_by_the_system_closes_quick_look(cx: &mut TestAppContext) {
        let (main, panel) = open_quick_look(cx);
        // Alt+F4 and the caption's close button remove the window without any action.
        cx.update(|cx| {
            panel
                .update(cx, |_, window, _| window.remove_window())
                .expect("the window is open");
        });
        cx.run_until_parked();
        assert!(!is_open(cx, main), "the state follows the window");
        // The keyboard is back on the file list of the main window.
        let list = cx.update(|cx| main.read(cx).unwrap().explorer.focus_handle(cx));
        assert!(focus_is(cx, main.into(), &list));
        // And Quick Look opens again, in a new window.
        cx.dispatch_action(main.into(), quick_look::Toggle);
        cx.run_until_parked();
        assert!(is_open(cx, main));
        assert_eq!(windows_open(cx), 2);
    }

    #[gpui::test]
    fn escape_in_the_main_list_closes_quick_look_first(cx: &mut TestAppContext) {
        let (main, _) = open_quick_look(cx);
        cx.dispatch_action(main.into(), window_actions::LeaveSearch);
        cx.run_until_parked();
        assert!(!is_open(cx, main));
        assert_eq!(windows_open(cx), 1);
    }

    #[gpui::test]
    fn closing_the_main_window_closes_quick_look_with_it(cx: &mut TestAppContext) {
        let (main, _) = open_quick_look(cx);
        cx.update(|cx| {
            main.update(cx, |_, window, _| window.remove_window())
                .expect("the main window is open");
        });
        cx.run_until_parked();
        assert!(cx.windows().is_empty(), "windows are still open");
    }

    #[gpui::test]
    fn the_arrow_keys_work_in_the_quick_look_window(cx: &mut TestAppContext) {
        let (main, panel) = open_quick_look(cx);
        // The listing is empty here; the keys reach the panel and nothing breaks.
        cx.simulate_keystrokes(panel, "down up home end left right pageup pagedown");
        cx.simulate_keystrokes(panel, "ctrl-tab ctrl-shift-tab");
        assert!(is_open(cx, main));
        assert_eq!(windows_open(cx), 2);
    }
}
